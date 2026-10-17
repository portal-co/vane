//! Browser surface for vane machine mode + encrypted swap (M6,
//! PLAN_LINUX_WASM.md §6.2). Wraps the no-JS `vane::Reactor` (the proven
//! machine path) with `wasm-bindgen` bindings so a dedicated Web Worker can
//! boot OpenSBI + Linux and page guest RAM to an OPFS backing store.
//!
//! The emulator runs entirely in the worker; the OPFS **sync access handle**
//! (worker-only) backs the swap via a host-provided JS store object:
//!
//! ```js
//! {
//!   slots(): bigint,                          // total page-record slots
//!   read(slot: bigint): Uint8Array | null,    // record bytes, or null
//!   write(slot: bigint, record: Uint8Array): boolean,
//! }
//! ```
//!
//! Because the OPFS sync handle is synchronous, the swap store is synchronous
//! and the interpreter's fault path stays sync (the IndexedDB async fallback
//! is not wired here; see PAGING_SWAP.md).

use wasm_bindgen::prelude::*;

use vane::vane_riscv::machine::{self, PowerAction};
use vane::Reactor;
use vane_swap::{PageCache, SwapError, SwapStore, NONCE_BYTES, RECORD_BYTES};

const RAM_BASE: u64 = 0x8000_0000;
const KERNEL_ADDR: u64 = 0x8020_0000;
const INITRD_ADDR: u64 = 0x8400_0000;
const DTB_ADDR: u64 = 0x87F0_0000;
const FW_DYNAMIC_INFO_ADDR: u64 = 0x87E0_0000;

fn fill_random(buf: &mut [u8]) -> Result<(), JsValue> {
    getrandom::fill(buf).map_err(|e| JsValue::from_str(&format!("CSPRNG: {e}")))
}

/// Minimal ELF64 loader (little-endian, PT_LOAD segments). Returns the entry.
fn load_elf64(reactor: &Reactor, bytes: &[u8]) -> Result<u64, JsValue> {
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return Err(JsValue::from_str("not an ELF64"));
    }
    let u16at = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let u64at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    let entry = u64at(24);
    let phoff = u64at(32) as usize;
    let phentsize = u16at(54) as usize;
    let phnum = u16at(56) as usize;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let p_type = u32::from_le_bytes(bytes[ph..ph + 4].try_into().unwrap());
        if p_type != 1 {
            continue; // PT_LOAD only
        }
        let off = u64at(ph + 8) as usize;
        let vaddr = u64at(ph + 16);
        let paddr = u64at(ph + 24);
        let filesz = u64at(ph + 32) as usize;
        let dest = if paddr != 0 { paddr } else { vaddr };
        reactor.write_bytes(dest, &bytes[off..off + filesz]);
    }
    Ok(entry)
}

/// A [`SwapStore`] backed by a host-provided JS object (the OPFS sync access
/// handle wrapper in the worker).
struct JsSwapStore {
    obj: js_sys::Object,
    slots: u64,
}

impl JsSwapStore {
    fn new(obj: js_sys::Object) -> Result<Self, JsValue> {
        let slots = call0(&obj, "slots")?
            .as_f64()
            .ok_or_else(|| JsValue::from_str("swap.slots() must return a number"))?;
        Ok(JsSwapStore {
            obj,
            slots: slots as u64,
        })
    }
}

fn call0(obj: &js_sys::Object, name: &str) -> Result<JsValue, JsValue> {
    let f = js_sys::Reflect::get(obj, &JsValue::from_str(name))?;
    let f: js_sys::Function = f
        .dyn_into()
        .map_err(|_| JsValue::from_str(&format!("swap.{name} is not a function")))?;
    f.call0(obj)
}

impl SwapStore for JsSwapStore {
    fn read_record(&mut self, slot: u64, buf: &mut [u8; RECORD_BYTES]) -> Result<(), SwapError> {
        let f = js_sys::Reflect::get(&self.obj, &JsValue::from_str("read"))
            .and_then(|f| f.dyn_into::<js_sys::Function>())
            .map_err(|_| SwapError::Store)?;
        let ret = f
            .call1(&self.obj, &JsValue::from(slot))
            .map_err(|_| SwapError::Store)?;
        if ret.is_null() || ret.is_undefined() {
            return Err(SwapError::Store);
        }
        let arr = js_sys::Uint8Array::new(&ret);
        if arr.length() as usize != RECORD_BYTES {
            return Err(SwapError::Store);
        }
        arr.copy_to(&mut buf[..]);
        Ok(())
    }
    fn write_record(&mut self, slot: u64, buf: &[u8; RECORD_BYTES]) -> Result<(), SwapError> {
        let f = js_sys::Reflect::get(&self.obj, &JsValue::from_str("write"))
            .and_then(|f| f.dyn_into::<js_sys::Function>())
            .map_err(|_| SwapError::Store)?;
        let arr = js_sys::Uint8Array::from(&buf[..]);
        let ok = f
            .call2(&self.obj, &JsValue::from(slot), &arr)
            .map_err(|_| SwapError::Store)?;
        if ok.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(SwapError::Store)
        }
    }
    fn slot_count(&self) -> u64 {
        self.slots
    }
}

/// A vane machine (QEMU-virt + privileged architecture + optional encrypted
/// swap) running OpenSBI + a Linux kernel, driven from a Web Worker.
#[wasm_bindgen]
pub struct WebMachine {
    reactor: Reactor,
    pc: u64,
}

#[wasm_bindgen]
impl WebMachine {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        let reactor = Reactor::new();
        reactor.set_hart_enabled(true);
        reactor.set_machine_enabled(true);
        WebMachine {
            reactor,
            pc: RAM_BASE,
        }
    }

    /// Enable OPFS-backed encrypted swap for the RAM region. `resident_bytes`
    /// is the resident-set cap; `ram_size` is the guest RAM size. `store` is
    /// the JS swap-store object (see module docs). Call before `boot`.
    #[wasm_bindgen(js_name = "enableSwap")]
    pub fn enable_swap(
        &self,
        resident_bytes: u64,
        ram_size: u64,
        store: js_sys::Object,
    ) -> Result<(), JsValue> {
        let frames = (resident_bytes / 0x1_0000).max(1) as usize;
        let js_store = JsSwapStore::new(store)?;
        let mut key = [0u8; 32];
        fill_random(&mut key)?;
        let cache = PageCache::new(
            frames,
            Box::new(js_store),
            key,
            Box::new(|| {
                let mut n = [0u8; NONCE_BYTES];
                let _ = getrandom::fill(&mut n);
                n
            }),
        );
        self.reactor.enable_swap(RAM_BASE, ram_size, cache);
        Ok(())
    }

    /// Load a guest image at a physical address (helper; `boot` is the
    /// usual path).
    #[wasm_bindgen]
    pub fn load(&self, addr: u64, bytes: &[u8]) {
        self.reactor.write_bytes(addr, bytes);
    }

    /// Boot OpenSBI + kernel (+ optional initrd). Sets up the DTB and
    /// fw_dynamic_info, pins the boot artifacts when swap is active, and
    /// returns the boot entry pc.
    #[wasm_bindgen]
    pub fn boot(
        &mut self,
        opensbi: &[u8],
        kernel: &[u8],
        initrd: Option<Vec<u8>>,
        append: &str,
        ram_size: u64,
    ) -> Result<(), JsValue> {
        self.reactor.write_bytes(RAM_BASE, opensbi);
        self.reactor.write_bytes(KERNEL_ADDR, kernel);
        let initrd_range = initrd.map(|b| {
            let len = b.len() as u64;
            self.reactor.write_bytes(INITRD_ADDR, &b);
            (INITRD_ADDR, INITRD_ADDR + len)
        });
        // fw_dynamic_info (a2).
        let info: [u64; 6] = [0x4942_534F, 2, KERNEL_ADDR, 1, 0, 0];
        let mut bytes = Vec::new();
        for w in info {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        self.reactor.write_bytes(FW_DYNAMIC_INFO_ADDR, &bytes);
        let dtb = machine::virt_dtb(ram_size, initrd_range, append);
        self.reactor.write_bytes(DTB_ADDR, &dtb);

        if self.reactor.swap_active() {
            self.reactor
                .pin_swap_range(RAM_BASE, KERNEL_ADDR - RAM_BASE);
            self.reactor
                .pin_swap_range(KERNEL_ADDR, kernel.len() as u64);
            if let Some((s, e)) = initrd_range {
                self.reactor.pin_swap_range(s, e - s);
            }
            self.reactor.pin_swap_range(FW_DYNAMIC_INFO_ADDR, 0x1_0000);
            self.reactor.pin_swap_range(DTB_ADDR, dtb.len() as u64);
        }

        // Boot registers: a0 = hartid, a1 = dtb, a2 = fw_dynamic_info.
        self.reactor.write_reg(10, 0);
        self.reactor.write_reg(11, DTB_ADDR);
        self.reactor.write_reg(12, FW_DYNAMIC_INFO_ADDR);
        self.pc = RAM_BASE;
        Ok(())
    }

    /// Load a bare-metal ELF64 payload (e.g. a test payload) and set the
    /// entry pc. Useful for smoke tests; `boot` is the Linux path.
    #[wasm_bindgen(js_name = "loadElf")]
    pub fn load_elf(&mut self, bytes: &[u8]) -> Result<(), JsValue> {
        self.pc = load_elf64(&self.reactor, bytes)?;
        Ok(())
    }

    /// Feed console input bytes into the emulated UART RX queue.
    #[wasm_bindgen(js_name = "uartInput")]
    pub fn uart_input(&self, bytes: &[u8]) {
        self.reactor.with_machine(|m| m.uart.push_rx(bytes));
    }

    /// Swap accounting (resident/faults/evictions/bytes/integrity failures).
    /// Returns a JS object or null when swap is disabled.
    #[wasm_bindgen(js_name = "swapStats")]
    pub fn swap_stats(&self) -> JsValue {
        match self.reactor.swap_stats() {
            Some(s) => {
                let o = js_sys::Object::new();
                let set = |k: &str, v: u64| {
                    let _ = js_sys::Reflect::set(&o, &k.into(), &JsValue::from(v));
                };
                set("resident", s.resident);
                set("cap", s.cap);
                set("faults", s.faults);
                set("evictions", s.evictions);
                set("writebacks", s.writebacks);
                set("swapBytesRead", s.swap_bytes_read);
                set("swapBytesWritten", s.swap_bytes_written);
                set("zeroPages", s.zero_pages);
                set("integrityFailures", s.integrity_failures);
                o.into()
            }
            None => JsValue::NULL,
        }
    }

    /// Run one execution quantum (a bounded number of trace dispatches).
    /// Returns a JS object: `{ out: string, done: boolean, code: number,
    /// swapError: string | null }`. `done` is true when the guest powered off
    /// or rebooted. This is async so a future async swap backend can suspend;
    /// with the OPFS sync handle it completes without yielding to the event
    /// loop beyond the microtask boundary.
    #[wasm_bindgen]
    pub async fn tick(&mut self, dispatches: u64) -> Result<JsValue, JsValue> {
        let mut out: Vec<u8> = Vec::new();
        let mut done = false;
        let mut code = 0u32;

        let mut noop =
            async |_: &mut vane::vane_riscv::interp::LocalStackHost<Reactor>| -> bool { false };
        let result = self
            .reactor
            .trace_run_fueled(self.pc, &mut noop, Some(dispatches))
            .await;
        match result {
            Ok(next) => self.pc = next,
            Err(e) => return Err(JsValue::from_str(&format!("interp: {e}"))),
        }
        // Device tick: drain UART TX, check power.
        let power = self.reactor.with_hart(|h| h.machine_tick(&[]));
        if let Some(tx) = self.reactor.with_machine(|m| m.uart.drain_tx()) {
            out.extend_from_slice(&tx);
        }
        if let Some(p) = power {
            done = true;
            match p {
                PowerAction::PowerOff { code: c } => code = c,
                PowerAction::Reboot => code = 0,
            }
        }
        let swap_error = self.reactor.take_swap_error().map(|e| format!("{e:?}"));

        let o = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&o, &"done".into(), &JsValue::from(done));
        let _ = js_sys::Reflect::set(&o, &"code".into(), &JsValue::from(code));
        let _ = js_sys::Reflect::set(
            &o,
            &"swapError".into(),
            &swap_error.map(JsValue::from).unwrap_or(JsValue::NULL),
        );
        let _ = js_sys::Reflect::set(
            &o,
            &"out".into(),
            &JsValue::from(String::from_utf8_lossy(&out).into_owned()),
        );
        Ok(o.into())
    }
}

impl Default for WebMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "js-host")]
mod utils;

// Re-export Mem for use in tests
use rv_asm::{Inst, Reg, Xlen};
#[cfg(feature = "js-host")]
use std::cell::OnceCell;
use std::rc::Rc;
/// Engine selection for `run` (interpreter vs JS JIT); see PLAN_LINUX_WASM.md §4.
pub use vane_arch::ExecEngine;
/// Re-exported for host/test access to the privileged hart state.
pub use vane_riscv;
pub use vane_riscv::hint;
use vane_riscv::template::Params;
pub use vane_riscv::Mem;
use vane_riscv::ReleaseFlate;
use vane_riscv::Riscv;
#[cfg(feature = "js-host")]
use wasm_bindgen::prelude::*;
#[cfg(feature = "js-host")]
use wasm_bindgen_futures::JsFuture;

#[cfg(all(test, feature = "js-host"))]
#[path = "../_tests/rv_corpus.rs"]
mod _tests;
vane_meta_gen::vane_meta!(
    Reactor,
    Core,
    Riscv,
    ReleaseFlate {},
    vane_riscv::template::riscv::rv_emit,
    vane_riscv::hart::HartState,
    Reactor::privileged_step
);

/// wasm-bindgen calls this once when the module is instantiated.
/// Installs `console_error_panic_hook` so Rust panics surface as readable
/// `console.error` messages (with the panic location/message) instead of an
/// opaque WASM trap.
#[cfg(feature = "js-host")]
#[wasm_bindgen(start)]
fn start() {
    utils::set_panic_hook();
}

#[cfg(feature = "js-host")]
#[wasm_bindgen(inline_js = r#"
    export function has_success(err){
        return err?.kind === 'exit';
    }
    export function log_test_hint(pc, hint_value){
        console.log(`[HINT] PC=0x${pc.toString(16)}: Test case ${hint_value}`);
    }"#)]
extern "C" {
    fn has_success(a: JsValue) -> bool;
    fn log_test_hint(pc: u64, hint_value: i64);
}

/// Why [`Reactor::interp_core`] failed.
///
/// This is the host-independent error vocabulary of the interpreter; the
/// `js-host` wrapper converts it to a `JsValue` string at the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpError {
    /// Instruction decode failed at the reported pc (message from `rv-asm`).
    Decode(String),
    /// A decoded instruction has no interpreter implementation yet.
    Unsupported(String),
}

impl core::fmt::Display for InterpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InterpError::Decode(e) => write!(f, "{e}"),
            InterpError::Unsupported(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InterpError {}

/// The result of executing one instruction via [`Reactor::step_one`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Continue at this pc (normal fallthrough, taken branch/jump, or a
    /// trap handler's vector when the privileged architecture is live).
    Next(u64),
    /// Legacy mode (`hart` disabled): the guest executed `ecall` at this
    /// pc and the async host hook must run.
    LegacyEcall(u64),
}

// Non-wasm impl block for Rust-only APIs
impl Reactor {
    /// Bulk-write guest memory (e.g. loading an ELF segment). Chunks the
    /// copy across `Mem`'s 64 KiB pages via `get_page`.
    pub fn write_bytes(&self, mut addr: u64, mut data: &[u8]) {
        // Swap-backed RAM (M5) must be written via the swap-aware byte path,
        // not the `get_page` raw pointer (the resident frame may be evicted
        // and the data must land in the PageCache).
        if self.core.lock().mem.swap_active() {
            let mut lock = self.core.lock();
            for (i, b) in data.iter().enumerate() {
                lock.mem.write_byte(addr.wrapping_add(i as u64), *b);
            }
            return;
        }
        while !data.is_empty() {
            let page_offset = (addr & 0xffff) as usize;
            let n = (65536 - page_offset).min(data.len());
            let dst = self.get_page(addr);
            // SAFETY: `get_page` returns a pointer to the live 64 KiB page
            // holding `addr`; `n` never crosses the page end, and pages are
            // boxed allocations that do not alias `data`.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst, n) };
            addr = addr.wrapping_add(n as u64);
            data = &data[n..];
        }
    }

    /// Read one guest-memory byte (host-side inspection / syscall shims).
    ///
    /// Named differently from the plain-Rust twin's `read_byte` so the same
    /// call exists under both `js-host` and non-JS builds.
    pub fn read_guest_byte(&self, addr: u64) -> u8 {
        self.core.lock().mem.read_byte(addr)
    }

    /// Create a new Reactor with custom memory
    /// This is a safe constructor that allows initializing the reactor with pre-loaded memory
    pub fn new_with_mem(mem: vane_riscv::Mem) -> Self {
        Reactor {
            _handle: (),
            core: Rc::new(spin::Mutex::new(Core {
                mem,
                #[cfg(feature = "js-host")]
                state: OnceCell::new(),
                #[cfg(feature = "js-host")]
                regs: OnceCell::new(),
                #[cfg(feature = "js-host")]
                host_api: OnceCell::new(),
                test_mode: false,
                trace_cache: std::cell::RefCell::new(std::collections::BTreeMap::new()),
                compiled: std::cell::RefCell::new(std::collections::BTreeMap::new()),
                engine: vane_arch::ExecEngine::Interp,
                pc: 0,
                code_dirty: false,
                #[cfg(not(feature = "js-host"))]
                regs: [0; 32],
                hart: Default::default(),
                hooks: Rc::new(vane_arch::hooks::HookSet::new()),
                hook_events: Vec::new(),
                pause_on_hook: false,
                skip_hook_pause_once: None,
            })),
        }
    }

    /// Enable or disable the privileged architecture (M3,
    /// PLAN_LINUX_WASM.md §7.2). When enabled, the hart boots in M-mode,
    /// `ecall` raises environment-call exceptions through the trap engine,
    /// and CSR/Sv39 semantics are live. Default: disabled (legacy
    /// userspace-ABI-only behavior).
    pub fn set_hart_enabled(&self, enabled: bool) {
        self.core.lock().hart.enabled = enabled;
    }

    /// Whether the privileged architecture is live.
    pub fn hart_enabled(&self) -> bool {
        self.core.lock().hart.enabled
    }

    /// Set a guest register (works under both host configurations).
    pub fn write_reg(&self, reg: u8, value: u64) {
        use vane_arch::interp::LocalReactor;
        self.lr_store_reg(reg, value);
    }

    /// Read a guest register.
    pub fn read_reg(&self, reg: u8) -> u64 {
        use vane_arch::interp::LocalReactor;
        self.lr_load_reg(reg)
    }

    /// Attach/detach the QEMU-virt-compatible machine (devices) — M4,
    /// PLAN_LINUX_WASM.md §7.4. When attached, sub-RAM physical accesses
    /// route to devices (UART/ACLINT/PLIC/syscon) and the trace engine
    /// steps every instruction instead of tracing.
    pub fn set_machine_enabled(&self, enabled: bool) {
        let mut lock = self.core.lock();
        lock.hart.machine = if enabled {
            Some(vane_riscv::machine::Machine::new())
        } else {
            None
        };
    }

    /// Access the machine (None when not attached) — for hosts feeding
    /// console input and draining output.
    pub fn with_machine<R>(
        &self,
        f: impl FnOnce(&mut vane_riscv::machine::Machine) -> R,
    ) -> Option<R> {
        let mut lock = self.core.lock();
        lock.hart.machine.as_mut().map(f)
    }

    /// Access the privileged hart state (CSRs, privilege mode, mip/mie,
    /// satp) — used by device models, tests, and host drivers.
    pub fn with_hart<R>(&self, f: impl FnOnce(&mut vane_riscv::hart::HartState) -> R) -> R {
        f(&mut self.core.lock().hart)
    }

    /// Host tick for a machine-enabled reactor (PLAN_VIRTIO_WEVAL §A): process
    /// virtio device queues — which need guest-memory access the MMIO path
    /// doesn't have — then feed console input and fold device-driven IRQ
    /// lines. Call each quantum. Returns a power action if the guest powered
    /// off or rebooted.
    pub fn machine_io_tick(&self, rx: &[u8]) -> Option<vane_riscv::machine::PowerAction> {
        let mut lock = self.core.lock();
        let core = &mut *lock;
        let Core { mem, hart, .. } = core;
        if let Some(m) = hart.machine.as_mut() {
            m.process_virtio(mem);
        }
        hart.machine_tick(rx)
    }

    /// Register a virtio-mmio device on the machine (next free slot, PLIC
    /// source `1 + slot`). Returns the slot index; `None` if no machine is
    /// attached. Register devices before generating the DTB so their nodes
    /// are emitted.
    pub fn add_virtio_device(
        &self,
        device: Box<dyn vane_riscv::virtio::VirtioDevice>,
    ) -> Option<usize> {
        self.with_machine(|m| m.add_virtio(device))
    }

    /// Register a compiled trace callable for the trace rooted at guest
    /// `pc` (M-perf / PLAN §4.2). This is the global exposure point for
    /// Weval/JIT/binary-patcher backends: they produce a [`TraceFn`] (a
    /// native/compiled callable) and insert it here; the dispatch loop then
    /// runs it instead of interpreting the trace's `StackOp`s.
    ///
    /// [`TraceFn`]: vane_arch::interp::TraceFn
    pub fn register_compiled_trace(&self, pc: u64, f: vane_arch::interp::TraceFn<Self>) {
        self.core.lock().compiled.borrow_mut().insert(pc, f);
    }

    /// Whether a compiled trace is registered for `pc`.
    pub fn has_compiled_trace(&self, pc: u64) -> bool {
        self.core.lock().compiled.borrow().contains_key(&pc)
    }

    /// Compile the trace rooted at `pc` with the default producer (the dense
    /// interpreter) and register it, so subsequent dispatches call it as a
    /// function pointer instead of interpreting. Returns false if the trace
    /// isn't compiled yet (run it once first to populate the trace cache).
    ///
    /// This is the hook a heat policy calls to promote hot traces; a
    /// Weval/JIT backend would instead call `register_compiled_trace` with a
    /// natively-compiled artifact.
    pub fn compile_trace_at(&self, pc: u64) -> bool {
        let ops = self.core.lock().trace_cache.borrow().get(&pc).cloned();
        match ops {
            Some(ops) => {
                self.register_compiled_trace(pc, vane_arch::interp::compile_trace(ops));
                true
            }
            None => false,
        }
    }

    /// Enable encrypted swap for the machine-mode RAM region (M5,
    /// PLAN_LINUX_WASM.md §6). `swap_base`/`swap_len` delimit the physical
    /// RAM range routed through the [`vane_swap::PageCache`]. Call before
    /// loading guest images so they land in the swap-backed store.
    pub fn enable_swap(&self, swap_base: u64, swap_len: u64, cache: vane_swap::PageCache) {
        let mut lock = self.core.lock();
        lock.mem.swap_base = swap_base;
        lock.mem.swap_len = swap_len;
        lock.mem.swap = Some(spin::Mutex::new(cache));
    }

    /// Whether encrypted swap is active.
    pub fn swap_active(&self) -> bool {
        self.core.lock().mem.swap_active()
    }

    /// Swap accounting (resident set, faults, evictions, crypto/IO bytes) —
    /// S4 report + browser UI. None when swap is disabled.
    pub fn swap_stats(&self) -> Option<vane_swap::MemStats> {
        let lock = self.core.lock();
        lock.mem.swap.as_ref().map(|s| s.lock().stats())
    }

    /// Take a pending swap error (integrity/IO), if any. The host checks this
    /// each quantum and reports it as a hard VM error (S5).
    pub fn take_swap_error(&self) -> Option<vane_swap::SwapError> {
        self.core.lock().mem.take_swap_error()
    }

    /// Pin the physical RAM pages covering `[base, base + len)` so they are
    /// never evicted (kernel image, DTB, OpenSBI — §6.2 item 2).
    pub fn pin_swap_range(&self, base: u64, len: u64) {
        let lock = self.core.lock();
        if let Some(swap) = &lock.mem.swap {
            let mut s = swap.lock();
            let start = base.saturating_sub(lock.mem.swap_base) >> 16;
            let end = (base + len)
                .saturating_sub(lock.mem.swap_base)
                .div_ceil(0x1_0000);
            for page_no in start..end {
                s.pin(page_no);
            }
        }
    }

    /// Execute exactly one instruction at `pc` (synchronous).
    ///
    /// This is the single instruction-semantics implementation shared by
    /// `interp_core` and the trace dispatch loop's privileged stepper (D5:
    /// one code path for all engines). When the privileged architecture is
    /// enabled it includes: pending-interrupt delivery, Sv39 fetch/load/
    /// store translation, CSR instructions with privilege checks, trap
    /// entry, and xRET.
    pub fn step_one(&self, pc: u64, regs: &mut [u64; 32]) -> Result<StepOutcome, InterpError> {
        use vane_riscv::hart::{
            Access, CsrOp, PrivMode, Trap, CAUSE_BREAKPOINT, CAUSE_ECALL_M, CAUSE_ECALL_S,
            CAUSE_ECALL_U, CAUSE_FETCH_ACCESS, CAUSE_ILLEGAL, CAUSE_LOAD_ACCESS,
            CAUSE_MISALIGNED_LOAD, CAUSE_MISALIGNED_STORE, CAUSE_STORE_ACCESS, MSTATUS_FS,
        };
        let mut guard = self.core.lock();
        // itime advances on every step, before any early return — virtual
        // machine time must never stall, even in a trap storm.
        guard.hart.itime = guard.hart.itime.wrapping_add(1);
        #[cfg(feature = "js-host")]
        let test_mode = guard.test_mode;
        // Destructure into disjoint field borrows: the MMU walker takes
        // &mut hart and &mut mem simultaneously.
        let core = &mut *guard;
        #[allow(unused_variables)]
        let Core {
            mem,
            hart,
            code_dirty,
            trace_cache,
            ..
        } = core;
        let hart_enabled = hart.enabled;
        if hart_enabled {
            // Refresh device-driven interrupt lines, then take any pending
            // interrupt before executing the next instruction.
            hart.machine_step_irqs();
            if let Some(cause) = hart.pending_interrupt() {
                let npc = hart.raise(
                    Trap {
                        cause,
                        tval: 0,
                        interrupt: true,
                    },
                    pc,
                );
                return Ok(StepOutcome::Next(npc));
            }
            // With a machine attached, the cycle CSR reads the machine
            // clock instead (see hart.rs), so only count instructions when
            // bare.
            if hart.mcountinhibit & 1 == 0 && hart.machine.is_none() {
                hart.mcycle += 1;
            }
            if hart.mcountinhibit & 4 == 0 {
                hart.minstret += 1;
            }
        }

        regs[0] = 0;

        // Fetch (translated when Sv39 is active).
        let fetch_pa = if hart_enabled && hart.translation_active() {
            match hart.translate(&mut *mem, pc, Access::Exec) {
                Ok(p) => p,
                Err(t) => return Ok(StepOutcome::Next(hart.raise(t, pc))),
            }
        } else {
            pc
        };
        // MMIO regions are not executable.
        if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(fetch_pa) {
            let t = Trap::exception(CAUSE_FETCH_ACCESS, pc);
            return Ok(StepOutcome::Next(hart.raise(t, pc)));
        }
        // Unaligned-safe fetch: with compressed instructions pc can be
        // 2 mod 4, and a raw aligned deref is both UB in Rust and broken
        // on backends that honor the alignment hint (wasm2js lowers an
        // align=4 load to HEAP32[addr >> 2], reading the wrong word).
        let code = if (fetch_pa & 0xFFFF) > 0xFFFC || mem.swap_active() {
            // Fetch crosses a 64 KiB Mem page boundary: read byte-wise.
            (0..4).fold(0u32, |acc, i| {
                acc | (mem.read_byte(fetch_pa.wrapping_add(i)) as u32) << (8 * i)
            })
        } else {
            unsafe { (mem.get_page(fetch_pa) as *mut u32).read_unaligned() }
        };
        // Decode memoization (pure function of the instruction word).
        let cache_idx = (code as usize) & 1023;
        let cached = hart.decode_cache[cache_idx]
            .as_ref()
            .filter(|(w, _, _)| *w == code);
        let i = match cached {
            Some((_, inst, comp)) => Ok((*inst, *comp)),
            None => {
                let d = Inst::decode(code, Xlen::Rv64);
                if let Ok((inst, comp)) = &d {
                    hart.decode_cache[cache_idx] = Some((code, *inst, *comp));
                }
                d
            }
        };
        let (i, b) = match i {
            Ok(a) => a,
            Err(e) => {
                if hart_enabled {
                    // Illegal/unrecognized instruction → trap with the raw
                    // encoding in xtval.
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                return Err(InterpError::Decode(format!("{e}")));
            }
        };

        // Check for test marker HINTs when test_mode is enabled
        #[cfg(feature = "js-host")]
        if test_mode {
            if let Some(hint_value) = hint::detect_test_marker(&i) {
                log_test_hint(pc, hint_value);
            }
        }

        let next = match b {
            rv_asm::IsCompressed::Yes => 2,
            rv_asm::IsCompressed::No => 4,
        } + pc;

        // Translate a guest virtual address for a data access.
        macro_rules! xlate {
            ($va:expr, $acc:expr) => {{
                let va = $va;
                if hart_enabled && hart.translation_active() {
                    match hart.translate(&mut *mem, va, $acc) {
                        Ok(p) => p,
                        Err(t) => return Ok(StepOutcome::Next(hart.raise(t, pc))),
                    }
                } else {
                    va
                }
            }};
        }
        // Load/store through the machine when one is attached and the
        // physical address is in the MMIO region; raw page access otherwise.
        macro_rules! loadv {
            ($pa:expr, $width:expr, $signed:expr) => {{
                let pa = $pa;
                let width: u64 = $width;
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    match hart
                        .machine
                        .as_mut()
                        .unwrap()
                        .load(pa, width as u8, $signed)
                    {
                        Ok(v) => v,
                        Err(t) => return Ok(StepOutcome::Next(hart.raise(t, pc))),
                    }
                } else if (pa & 0xFFFF) + width > 0x1_0000 || mem.swap_active() {
                    // Page-crossing access (or swap-backed RAM, M5: the
                    // get_page fast path is unsafe with an evictable resident
                    // set): read byte-wise via the swap-aware path.
                    let mut v = 0u64;
                    for i in 0..width {
                        v |= (mem.read_byte(pa.wrapping_add(i)) as u64) << (8 * i);
                    }
                    if $signed {
                        match width {
                            1 => v as u8 as i8 as i64 as u64,
                            2 => v as u16 as i16 as i64 as u64,
                            4 => v as u32 as i32 as i64 as u64,
                            _ => v,
                        }
                    } else {
                        v
                    }
                } else {
                    let ptr = mem.get_page(pa);
                    match (width, $signed) {
                        (1, true) => (unsafe { *(ptr as *mut i8) }) as i64 as u64,
                        (1, false) => (unsafe { *(ptr as *mut u8) }) as u64,
                        (2, true) => (unsafe { (ptr as *mut i16).read_unaligned() }) as i64 as u64,
                        (2, false) => (unsafe { (ptr as *mut u16).read_unaligned() }) as u64,
                        (4, true) => (unsafe { (ptr as *mut i32).read_unaligned() }) as i64 as u64,
                        (4, false) => (unsafe { (ptr as *mut u32).read_unaligned() }) as u64,
                        (8, _) => unsafe { (ptr as *mut u64).read_unaligned() },
                        _ => unreachable!(),
                    }
                }
            }};
        }
        macro_rules! storev {
            ($pa:expr, $width:expr, $val:expr) => {{
                let pa = $pa;
                let width: u64 = $width;
                let v = $val;
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    if let Err(t) = hart.machine.as_mut().unwrap().store(pa, width as u8, v) {
                        return Ok(StepOutcome::Next(hart.raise(t, pc)));
                    }
                } else if (pa & 0xFFFF) + width > 0x1_0000 || mem.swap_active() {
                    // Page-crossing store (or swap-backed RAM, M5): byte-wise
                    // via the swap-aware path.
                    for i in 0..width {
                        mem.write_byte(pa.wrapping_add(i), (v >> (8 * i)) as u8);
                    }
                } else {
                    let ptr = mem.get_page(pa);
                    unsafe {
                        match width {
                            1 => *(ptr as *mut u8) = v as u8,
                            2 => (ptr as *mut u16).write_unaligned(v as u16),
                            4 => (ptr as *mut u32).write_unaligned(v as u32),
                            8 => (ptr as *mut u64).write_unaligned(v),
                            _ => unreachable!(),
                        }
                    }
                }
            }};
        }
        // Raise an illegal-instruction trap (privileged mode) or fail
        // interpretation (legacy mode).
        macro_rules! illegal_or_unsupported {
            ($what:expr) => {{
                if hart_enabled {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                return Err(InterpError::Unsupported(format!("iop:{}", $what)));
            }};
        }
        // Zicsr operation with privilege checks.
        macro_rules! csr_op {
            ($op:expr, $csr:expr, $dest:expr, $val:expr, $write:expr) => {{
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if ($csr).0 == 0x180 && $write {
                    // satp write: flush the VA-keyed trace cache (the TLB
                    // flush happens inside hart.csr_op).
                    trace_cache.borrow_mut().clear();
                }
                match hart.csr_op($op, ($csr).0, $val, $write) {
                    Ok(old) => {
                        set_reg!($dest => old);
                    }
                    Err(t) => {
                        return Ok(StepOutcome::Next(hart.raise(t, pc)));
                    }
                }
            }};
        }

        macro_rules! reg {
            ($a:expr) => {
                match $a {
                    Reg(v) => regs[(v % 32) as usize],
                }
            };
        }
        macro_rules! reg32 {
            ($a:expr) => {
                match reg!($a) {
                    v => (v & 0xffff_ffff) as u32,
                }
            };
        }
        macro_rules! set_reg {
            ($a:expr => $b:expr) => {{
                let v = $b;
                // Writes to x0 are discarded (architectural).
                match $a {
                    Reg(0) => {}
                    Reg(r) => regs[(r % 32) as usize] = v,
                }
                v
            }};
        }
        macro_rules! set_reg32 {
            ($a:expr => $b:expr) => {
                set_reg!($a => match $b{
                    val => (val as i32 as i64 as u64)
                })
            }
        }
        macro_rules! done {
            () => {
                return Ok(StepOutcome::Next(next))
            };
        }
        match i {
            Inst::Lui { uimm, dest } => {
                set_reg!(dest => uimm.as_u64());
            }
            Inst::Auipc { uimm, dest } => {
                set_reg!(dest => uimm.as_u64().wrapping_add(pc));
            }
            //Immediates
            Inst::Addi { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1).wrapping_add(imm.as_u64()));
            }
            Inst::Slti { imm, dest, src1 } => {
                set_reg!(dest => match (reg!(src1) as i64) < imm.as_i64() {
                    true => 1,
                    false => 0,
                });
            }
            Inst::Sltiu { imm, dest, src1 } => {
                set_reg!(dest => match (reg!(src1)) < imm.as_u64() {
                    true => 1,
                    false => 0,
                });
            }
            Inst::Andi { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1) & imm.as_u64());
            }
            Inst::Ori { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1) | imm.as_u64());
            }
            Inst::Xori { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1) ^ imm.as_u64());
            }
            Inst::Slli { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1) << (imm.as_u32() % 64));
            }
            Inst::Srli { imm, dest, src1 } => {
                set_reg!(dest => reg!(src1) >> (imm.as_u32() % 64));
            }
            Inst::Srai { imm, dest, src1 } => {
                set_reg!(dest => ((reg!(src1) as i64) >> (imm.as_u32() % 64)) as u64);
            }
            //Regs
            Inst::Add { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1).wrapping_add(reg!(src2)));
            }
            Inst::Slt { src2, dest, src1 } => {
                set_reg!(dest => match (reg!(src1) as i64) < (reg!(src2) as i64) {
                    true => 1,
                    false => 0,
                });
            }
            Inst::Sltu { src2, dest, src1 } => {
                set_reg!(dest => match (reg!(src1)) < reg!(src2) {
                    true => 1,
                    false => 0,
                });
            }
            Inst::And { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1) & reg!(src2));
            }
            Inst::Or { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1) | reg!(src2));
            }
            Inst::Xor { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1) ^ reg!(src2));
            }
            Inst::Sll { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1) << ((reg!(src2) & 0xffff_ffff) as u32 % 64));
            }
            Inst::Srl { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1) >> ((reg!(src2) & 0xffff_ffff) as u32 % 64));
            }
            Inst::Sra { src2, dest, src1 } => {
                set_reg!(dest => ((reg!(src1) as i64) >> ((reg!(src2) & 0xffff_ffff) as u32 % 64)) as u64);
            }
            Inst::Sub { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1).wrapping_sub(reg!(src2)));
            }
            //Multipliaction
            Inst::Mul { src2, dest, src1 } => {
                set_reg!(dest => reg!(src1).wrapping_mul(reg!(src2)));
            }
            Inst::Mulh { src2, dest, src1 } => {
                set_reg!(dest => (((reg!(src1) as i64 as i128) * (reg!(src2) as i64 as i128)) >> 64) as u64);
            }
            Inst::Mulhu { src2, dest, src1 } => {
                set_reg!(dest => ((reg!(src1) as u128 * reg!(src2) as u128) >> 64) as u64);
            }
            Inst::Mulhsu { src2, dest, src1 } => {
                set_reg!(dest => (((reg!(src1) as i64 as i128) * (reg!(src2) as u128 as i128)) >> 64) as u64);
            }
            Inst::Div { src2, dest, src1 } => {
                let (a, b) = (reg!(src1) as i64, reg!(src2) as i64);
                set_reg!(dest => if b == 0 { u64::MAX } else { a.wrapping_div(b) as u64 });
            }
            Inst::Divu { src2, dest, src1 } => {
                let (a, b) = (reg!(src1), reg!(src2));
                set_reg!(dest => if b == 0 { u64::MAX } else { a / b });
            }
            Inst::Rem { src2, dest, src1 } => {
                let (a, b) = (reg!(src1) as i64, reg!(src2) as i64);
                set_reg!(dest => if b == 0 { a as u64 } else { a.wrapping_rem(b) as u64 });
            }
            Inst::Remu { src2, dest, src1 } => {
                let (a, b) = (reg!(src1), reg!(src2));
                set_reg!(dest => if b == 0 { a } else { a % b });
            }
            //Jumps
            Inst::Jal { offset, dest } => {
                set_reg!(dest => next);
                return Ok(StepOutcome::Next(
                    pc.wrapping_add_signed(offset.as_i64()) & (!1),
                ));
            }
            Inst::Jalr { offset, base, dest } => {
                // Read the base BEFORE writing the link register: when
                // rd == rs1 (e.g. `jalr ra, off(ra)`), the target must be
                // computed from rs1's original value. The trace lowering
                // (rv_emit) already handles this; the interpreter did not.
                let target = reg!(base).wrapping_add_signed(offset.as_i64()) & (!1);
                set_reg!(dest => next);
                return Ok(StepOutcome::Next(target));
            }
            //Branches
            Inst::Beq { offset, src1, src2 } => {
                if reg!(src1) == reg!(src2) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            Inst::Bne { offset, src1, src2 } => {
                if reg!(src1) != reg!(src2) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            Inst::Bltu { offset, src1, src2 } => {
                if reg!(src1) < reg!(src2) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            Inst::Bgeu { offset, src1, src2 } => {
                if reg!(src1) >= reg!(src2) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            Inst::Blt { offset, src1, src2 } => {
                if (reg!(src1) as i64) < (reg!(src2) as i64) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            Inst::Bge { offset, src1, src2 } => {
                if (reg!(src1) as i64) >= (reg!(src2) as i64) {
                    return Ok(StepOutcome::Next(pc.wrapping_add_signed(offset.as_i64())));
                }
            }
            //Loads
            Inst::Lb { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 1, true));
            }
            Inst::Lbu { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 1, false));
            }
            Inst::Lh { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 2, true));
            }
            Inst::Lhu { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 2, false));
            }
            Inst::Lw { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 4, true));
            }
            Inst::Lwu { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 4, false));
            }
            Inst::Ld { offset, dest, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                set_reg!(dest => loadv!(pa, 8, false));
            }
            //Stores
            Inst::Sb { offset, src, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 1, reg!(src));
            }
            Inst::Sh { offset, src, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 2, reg!(src));
            }
            Inst::Sw { offset, src, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 4, reg!(src));
            }
            Inst::Sd { offset, src, base } => {
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 8, reg!(src));
            }
            //Fence
            Inst::Fence { fence: _ } => {}
            Inst::FenceI => {
                // No icache to flush; per the trace-invalidation contract
                // (PLAN_LINUX_WASM.md §9), just mark code dirty — the trace
                // cache revalidates once on the next dispatch.
                if hart_enabled {
                    *code_dirty = true;
                }
            }
            //Wide
            //Immediates
            Inst::AddiW { imm, dest, src1 } => {
                set_reg32!(dest => reg32!(src1).wrapping_add(imm.as_u32()));
            }
            Inst::SlliW { imm, dest, src1 } => {
                set_reg32!(dest => reg32!(src1) << (imm.as_u32() % 32));
            }
            Inst::SrliW { imm, dest, src1 } => {
                set_reg32!(dest => reg32!(src1) >> (imm.as_u32() % 32));
            }
            Inst::SraiW { imm, dest, src1 } => {
                set_reg32!(dest => ((reg32!(src1) as i32) >> (imm.as_u32() % 32)) as u32);
            }
            //Regs
            Inst::AddW { src2, dest, src1 } => {
                set_reg32!(dest => reg32!(src1).wrapping_add(reg32!(src2)));
            }
            Inst::SllW { src2, dest, src1 } => {
                set_reg32!(dest => reg32!(src1) << ((reg32!(src2) & 0xffff_ffff) % 32));
            }
            Inst::SrlW { src2, dest, src1 } => {
                set_reg32!(dest => reg32!(src1) >> ((reg32!(src2) & 0xffff_ffff) % 32));
            }
            Inst::SraW { src2, dest, src1 } => {
                set_reg32!(dest => ((reg32!(src1) as i32) >> ((reg32!(src2) & 0xffff_ffff) % 32)) as u32);
            }
            Inst::SubW { src2, dest, src1 } => {
                set_reg32!(dest => reg32!(src1).wrapping_sub(reg32!(src2)));
            }
            //Multipliaction
            Inst::MulW { src2, dest, src1 } => {
                set_reg32!(dest => reg32!(src1).wrapping_mul(reg32!(src2)));
            }
            Inst::DivW { src2, dest, src1 } => {
                let (a, b) = (reg32!(src1) as i32, reg32!(src2) as i32);
                set_reg32!(dest => if b == 0 { u32::MAX } else { a.wrapping_div(b) as u32 });
            }
            Inst::DivuW { src2, dest, src1 } => {
                let (a, b) = (reg32!(src1), reg32!(src2));
                set_reg32!(dest => if b == 0 { u32::MAX } else { a / b });
            }
            Inst::RemW { src2, dest, src1 } => {
                let (a, b) = (reg32!(src1) as i32, reg32!(src2) as i32);
                set_reg32!(dest => if b == 0 { a as u32 } else { a.wrapping_rem(b) as u32 });
            }
            Inst::RemuW { src2, dest, src1 } => {
                let (a, b) = (reg32!(src1), reg32!(src2));
                set_reg32!(dest => if b == 0 { a } else { a % b });
            }
            //Atomics (A extension)
            Inst::LrW { dest, addr, .. } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Load);
                if pa % 4 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_LOAD, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_LOAD_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                hart.reservation = Some(pa);
                set_reg!(dest => loadv!(pa, 4, true));
            }
            Inst::ScW {
                dest, addr, src, ..
            } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Store);
                if pa % 4 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_STORE, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_STORE_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let ok = hart.reservation == Some(pa);
                hart.reservation = None;
                if ok {
                    storev!(pa, 4, reg!(src));
                }
                set_reg!(dest => if ok { 0 } else { 1 });
            }
            Inst::AmoW {
                op,
                dest,
                addr,
                src,
                ..
            } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Store);
                if pa % 4 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_STORE, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_STORE_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let old = loadv!(pa, 4, false) as u32;
                let srcv = reg!(src) as u32;
                let new = match op {
                    rv_asm::AmoOp::Swap => srcv,
                    rv_asm::AmoOp::Add => old.wrapping_add(srcv),
                    rv_asm::AmoOp::Xor => old ^ srcv,
                    rv_asm::AmoOp::And => old & srcv,
                    rv_asm::AmoOp::Or => old | srcv,
                    rv_asm::AmoOp::Min => (old as i32).min(srcv as i32) as u32,
                    rv_asm::AmoOp::Max => (old as i32).max(srcv as i32) as u32,
                    rv_asm::AmoOp::Minu => old.min(srcv),
                    rv_asm::AmoOp::Maxu => old.max(srcv),
                };
                storev!(pa, 4, new as u64);
                set_reg!(dest => old as i32 as i64 as u64);
            }
            Inst::LrD { dest, addr, .. } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Load);
                if pa % 8 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_LOAD, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_LOAD_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                hart.reservation = Some(pa);
                set_reg!(dest => loadv!(pa, 8, false));
            }
            Inst::ScD {
                dest, addr, src, ..
            } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Store);
                if pa % 8 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_STORE, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_STORE_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let ok = hart.reservation == Some(pa);
                hart.reservation = None;
                if ok {
                    storev!(pa, 8, reg!(src));
                }
                set_reg!(dest => if ok { 0 } else { 1 });
            }
            Inst::AmoD {
                op,
                dest,
                addr,
                src,
                ..
            } => {
                let va = reg!(addr);
                let pa = xlate!(va, Access::Store);
                if pa % 8 != 0 {
                    let t = Trap::exception(CAUSE_MISALIGNED_STORE, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                if hart.machine.is_some() && vane_riscv::machine::Machine::is_mmio(pa) {
                    let t = Trap::exception(CAUSE_STORE_ACCESS, va);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let old = loadv!(pa, 8, false);
                let srcv = reg!(src);
                let new = match op {
                    rv_asm::AmoOp::Swap => srcv,
                    rv_asm::AmoOp::Add => old.wrapping_add(srcv),
                    rv_asm::AmoOp::Xor => old ^ srcv,
                    rv_asm::AmoOp::And => old & srcv,
                    rv_asm::AmoOp::Or => old | srcv,
                    rv_asm::AmoOp::Min => (old as i64).min(srcv as i64) as u64,
                    rv_asm::AmoOp::Max => (old as i64).max(srcv as i64) as u64,
                    rv_asm::AmoOp::Minu => old.min(srcv),
                    rv_asm::AmoOp::Maxu => old.max(srcv),
                };
                storev!(pa, 8, new);
                set_reg!(dest => old);
            }
            //Environment
            Inst::Ecall => {
                if hart_enabled {
                    let cause = match hart.priv_mode {
                        PrivMode::U => CAUSE_ECALL_U,
                        PrivMode::S => CAUSE_ECALL_S,
                        PrivMode::M => CAUSE_ECALL_M,
                    };
                    let npc = hart.raise(Trap::exception(cause, 0), pc);
                    return Ok(StepOutcome::Next(npc));
                }
                return Ok(StepOutcome::LegacyEcall(pc));
            }
            Inst::Ebreak => {
                if hart_enabled {
                    let npc = hart.raise(Trap::exception(CAUSE_BREAKPOINT, pc), pc);
                    return Ok(StepOutcome::Next(npc));
                }
                illegal_or_unsupported!(i);
            }
            //CSRs
            Inst::Csrrw { csr, dest, src } => {
                csr_op!(CsrOp::Write, csr, dest, reg!(src), true);
            }
            Inst::Csrrs { csr, dest, src } => {
                csr_op!(CsrOp::Set, csr, dest, reg!(src), src.0 != 0);
            }
            Inst::Csrrc { csr, dest, src } => {
                csr_op!(CsrOp::Clear, csr, dest, reg!(src), src.0 != 0);
            }
            Inst::Csrrwi { csr, dest, uimm } => {
                csr_op!(CsrOp::Write, csr, dest, uimm.as_u64(), true);
            }
            Inst::Csrrsi { csr, dest, uimm } => {
                csr_op!(CsrOp::Set, csr, dest, uimm.as_u64(), uimm.as_u64() != 0);
            }
            Inst::Csrrci { csr, dest, uimm } => {
                csr_op!(CsrOp::Clear, csr, dest, uimm.as_u64(), uimm.as_u64() != 0);
            }
            //Trap return and fences
            Inst::Mret => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if hart.priv_mode != PrivMode::M {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let npc = hart.mret();
                return Ok(StepOutcome::Next(npc));
            }
            Inst::Sret => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                let priv_mode = hart.priv_mode;
                if priv_mode < PrivMode::S
                    || (priv_mode == PrivMode::S
                        && hart.mstatus & vane_riscv::hart::MSTATUS_TSR != 0)
                {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let npc = hart.sret();
                return Ok(StepOutcome::Next(npc));
            }
            Inst::Wfi => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                // Implemented as a NOP (the spec explicitly allows this);
                // interrupts are checked at every step anyway.
                if hart.priv_mode < PrivMode::M && hart.mstatus & vane_riscv::hart::MSTATUS_TW != 0
                {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
            }
            Inst::SfenceVma { addr, asid } => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                let priv_mode = hart.priv_mode;
                if priv_mode < PrivMode::S
                    || (priv_mode == PrivMode::S
                        && hart.mstatus & vane_riscv::hart::MSTATUS_TVM != 0)
                {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                hart.sfence_vma(reg!(addr), reg!(asid));
                // Mappings may have changed: traces are keyed by virtual pc.
                trace_cache.borrow_mut().clear();
            }
            // ── F/D extension (privileged mode only; the JS JIT handles FP
            // in legacy mode via StackOp) ──
            Inst::Flw { offset, dest, base } => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if hart.mstatus & MSTATUS_FS == 0 {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                let v = { loadv!(pa, 4, false) as u32 };
                hart.write_f32(dest, f32::from_bits(v));
            }
            Inst::Fld { offset, dest, base } => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if hart.mstatus & MSTATUS_FS == 0 {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Load);
                let v = loadv!(pa, 8, false);
                hart.write_f64(dest, f64::from_bits(v));
            }
            Inst::Fsw { offset, src, base } => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if hart.mstatus & MSTATUS_FS == 0 {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 4, hart.fregs[src.0 as usize] & 0xffff_ffff);
            }
            Inst::Fsd { offset, src, base } => {
                if !hart_enabled {
                    illegal_or_unsupported!(i);
                }
                if hart.mstatus & MSTATUS_FS == 0 {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                let va = reg!(base).wrapping_add_signed(offset.as_i64());
                let pa = xlate!(va, Access::Store);
                storev!(pa, 8, hart.fregs[src.0 as usize]);
            }
            inst => {
                if !hart_enabled {
                    illegal_or_unsupported!(inst);
                }
                // FS gate: F/D instructions trap illegal when FP is off.
                if hart.mstatus & MSTATUS_FS == 0 {
                    let t = Trap::exception(CAUSE_ILLEGAL, code as u64);
                    return Ok(StepOutcome::Next(hart.raise(t, pc)));
                }
                match vane_riscv::fp::exec_fp(hart, regs, &inst) {
                    vane_riscv::fp::FpOutcome::NotFp => illegal_or_unsupported!(inst),
                    vane_riscv::fp::FpOutcome::Done => {}
                    vane_riscv::fp::FpOutcome::Writeback(rd, v) => {
                        set_reg!(rd => v);
                    }
                }
            }
        }
        done!()
    }

    /// Trace-dispatch hook for the privileged architecture (M3): when the
    /// hart is enabled and either translation is active or the instruction
    /// at `pc` is privileged-class, execute one instruction via
    /// [`Reactor::step_one`] and return the new pc. Returns `Ok(None)` when
    /// tracing `pc` is safe (hart disabled, or M/bare-mode ordinary code).
    /// Deliver a pending interrupt, if any, diverting to the trap handler.
    /// Cheap (no instruction peek): refreshes device lines and checks
    /// mip/mie. Runs every trace dispatch even when the trace is cached —
    /// the peek in [`privileged_step`](Self::privileged_step) is skipped for
    /// cached traces, so this is the interrupt-delivery fast path.
    pub fn deliver_pending_interrupt(&self, pc: u64) -> Result<Option<u64>, String> {
        let mut guard = self.core.lock();
        if !guard.hart.enabled {
            return Ok(None);
        }
        guard.hart.machine_step_irqs();
        if let Some(cause) = guard.hart.pending_interrupt() {
            let npc = guard.hart.raise(
                vane_riscv::hart::Trap {
                    cause,
                    tval: 0,
                    interrupt: true,
                },
                pc,
            );
            return Ok(Some(npc));
        }
        Ok(None)
    }

    pub fn privileged_step(&self, pc: u64, force: bool) -> Result<Option<u64>, String> {
        {
            let mut guard = self.core.lock();
            if !guard.hart.enabled {
                return Ok(None);
            }
            // Interrupt injection point for the trace engine: refresh device
            // lines and deliver any pending interrupt before tracing on.
            guard.hart.machine_step_irqs();
            if let Some(cause) = guard.hart.pending_interrupt() {
                let npc = guard.hart.raise(
                    vane_riscv::hart::Trap {
                        cause,
                        tval: 0,
                        interrupt: true,
                    },
                    pc,
                );
                return Ok(Some(npc));
            }
            // The Bail path calls with force=true: the stepper re-runs the
            // faulting instruction (a regular load/store) to raise the trap.
            if force {
                // Drop the core lock first — step_privileged_full locks it
                // (spin::Mutex is not reentrant).
                drop(guard);
                return self.step_privileged_full(pc).map(Some);
            }
            // Peek at the instruction (translating the fetch when Sv39 is
            // active); only privileged-class instructions need the stepper —
            // everything else traces normally (the StackOp port).
            let core = &mut *guard;
            let Core { mem, hart, .. } = core;
            let fetch_pa = if hart.translation_active() {
                match hart.translate_exec(&mut *mem, pc) {
                    Ok(p) => p,
                    // Fetch fault: the stepper raises it properly.
                    Err(_) => return self.step_privileged_full(pc).map(Some),
                }
            } else {
                pc
            };
            let code = unsafe { (mem.get_page(fetch_pa) as *mut u32).read_unaligned() };
            let needs_step = match Inst::decode(code, Xlen::Rv64) {
                Ok((inst, _)) => matches!(
                    inst,
                    Inst::Csrrw { .. }
                        | Inst::Csrrs { .. }
                        | Inst::Csrrc { .. }
                        | Inst::Csrrwi { .. }
                        | Inst::Csrrsi { .. }
                        | Inst::Csrrci { .. }
                        | Inst::Mret
                        | Inst::Sret
                        | Inst::Wfi
                        | Inst::SfenceVma { .. }
                        | Inst::Ecall
                        | Inst::Ebreak
                        // fence.i flushes the code-dirty flag (step_one
                        // handles it) — boundary it like the privileged ops.
                        | Inst::FenceI
                        // AMOs (lr/sc + atomic ops): not lowered to StackOps
                        // by rv_emit, so they boundary out of traces and the
                        // stepper handles them (single implementation, D5).
                        | Inst::LrW { .. }
                        | Inst::ScW { .. }
                        | Inst::AmoW { .. }
                        | Inst::LrD { .. }
                        | Inst::ScD { .. }
                        | Inst::AmoD { .. }
                        // FP ops rv_float_ops doesn't lower (moves, min/max,
                        // comparisons, conversions, classify, sign-inject,
                        // fused multiply-add) — boundary out of traces and
                        // step via step_one's exec_fp (D5: one semantics).
                        | Inst::FmaddS { .. }
                        | Inst::FmsubS { .. }
                        | Inst::FnmsubS { .. }
                        | Inst::FnmaddS { .. }
                        | Inst::FmaddD { .. }
                        | Inst::FmsubD { .. }
                        | Inst::FnmsubD { .. }
                        | Inst::FnmaddD { .. }
                        | Inst::FsgnjS { .. }
                        | Inst::FsgnjnS { .. }
                        | Inst::FsgnjxS { .. }
                        | Inst::FsgnjD { .. }
                        | Inst::FsgnjnD { .. }
                        | Inst::FsgnjxD { .. }
                        | Inst::FminS { .. }
                        | Inst::FmaxS { .. }
                        | Inst::FminD { .. }
                        | Inst::FmaxD { .. }
                        | Inst::FeqS { .. }
                        | Inst::FltS { .. }
                        | Inst::FleS { .. }
                        | Inst::FeqD { .. }
                        | Inst::FltD { .. }
                        | Inst::FleD { .. }
                        | Inst::FclassS { .. }
                        | Inst::FclassD { .. }
                        | Inst::FmvXW { .. }
                        | Inst::FmvXD { .. }
                        | Inst::FmvWX { .. }
                        | Inst::FmvDX { .. }
                        | Inst::FcvtWS { .. }
                        | Inst::FcvtWuS { .. }
                        | Inst::FcvtLS { .. }
                        | Inst::FcvtLuS { .. }
                        | Inst::FcvtWD { .. }
                        | Inst::FcvtWuD { .. }
                        | Inst::FcvtLD { .. }
                        | Inst::FcvtLuD { .. }
                        | Inst::FcvtSW { .. }
                        | Inst::FcvtSWu { .. }
                        | Inst::FcvtSL { .. }
                        | Inst::FcvtSLu { .. }
                        | Inst::FcvtDW { .. }
                        | Inst::FcvtDWu { .. }
                        | Inst::FcvtDL { .. }
                        | Inst::FcvtDLu { .. }
                        | Inst::FcvtSD { .. }
                        | Inst::FcvtDS { .. }
                ),
                // Decode failure: let the stepper raise the trap.
                Err(_) => true,
            };
            if !needs_step {
                return Ok(None);
            }
        }
        self.step_privileged_full(pc).map(Some)
    }

    /// Execute one instruction through `step_one` on the trace dispatch
    /// path (registers synced through the [`LocalReactor`] interface).
    fn step_privileged_full(&self, pc: u64) -> Result<u64, String> {
        use vane_arch::interp::LocalReactor;
        let mut regs = [0u64; 32];
        for i in 1..32u8 {
            regs[i as usize] = self.lr_load_reg(i);
        }
        match self.step_one(pc, &mut regs) {
            Ok(StepOutcome::Next(np)) => {
                for i in 1..32u8 {
                    self.lr_store_reg(i, regs[i as usize]);
                }
                Ok(np)
            }
            Ok(StepOutcome::LegacyEcall(_)) => {
                Err("internal: legacy ecall reached with hart enabled".into())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Fueled variant of [`Reactor::interp_core`]: when `fuel` is `Some`,
    /// returns `Ok(pc)` once the budget is exhausted (the caller may then
    /// service timers, poll `tohost`, etc. and resume).
    pub async fn interp_core_fueled<F>(
        &self,
        mut pc: u64,
        regs: &mut [u64; 32],
        on_ecall: &mut F,
        mut fuel: Option<u64>,
    ) -> Result<u64, InterpError>
    where
        F: AsyncFnMut(&mut [u64; 32]) -> bool,
    {
        loop {
            if let Some(f) = &mut fuel {
                if *f == 0 {
                    return Ok(pc);
                }
                *f -= 1;
            }
            match self.step_one(pc, regs)? {
                StepOutcome::Next(next) => pc = next,
                StepOutcome::LegacyEcall(ecall_pc) => {
                    if on_ecall(regs).await {
                        return Ok(ecall_pc);
                    }
                    pc = ecall_pc.wrapping_add(4);
                }
            }
        }
    }

    /// The host-independent RV64 interpreter loop (PLAN_LINUX_WASM.md §4:
    /// this is the always-available execution engine; JITs layer on top).
    ///
    /// Executes from `pc` with a caller-owned register file `regs` (so JS
    /// hosts can keep their register file wherever they like, and WASI /
    /// native hosts just hold a `[u64; 32]`). Runs until `on_ecall` reports
    /// stop or an instruction fails.
    ///
    /// `on_ecall` receives the live register file whenever the guest
    /// executes `ecall` *with the privileged architecture disabled*. When
    /// [`Reactor::set_hart_enabled`] is on, `ecall` raises exceptions
    /// through the in-guest trap engine instead and the hook never fires.
    /// The handler may mutate registers (e.g. set `a0` to the syscall
    /// result) and returns `true` to stop interpretation (the returned pc
    /// then points at the `ecall`) or `false` to continue at `pc + 4`.
    pub async fn interp_core<F>(
        &self,
        pc: u64,
        regs: &mut [u64; 32],
        on_ecall: &mut F,
    ) -> Result<u64, InterpError>
    where
        F: AsyncFnMut(&mut [u64; 32]) -> bool,
    {
        self.interp_core_fueled(pc, regs, on_ecall, None).await
    }
}

#[cfg(not(feature = "js-host"))]
impl Reactor {
    /// Construct a Reactor with empty guest memory. Load a guest image with
    /// repeated [`Reactor::write_byte`] calls before calling
    /// [`Reactor::interp_core`].
    pub fn new() -> Self {
        Self::new_with_mem(vane_riscv::Mem::default())
    }
}

/// A full machine snapshot (PLAN_VIRTIO_WEVAL workstream E / W1; spday
/// full-virtualization plan item C): sparse guest RAM (64KiB physical
/// pages, zero pages omitted), the register file, pc, and the hart/device
/// state. Snapshots form a lineage: `id` is a content hash over the state
/// and the parent's id, so a *derived* snapshot names its parent — the
/// substrate for trajectory forking (snapshot → restore into a second
/// Reactor → both runs diverge independently).
///
/// Trace caches and the compiled-trace registry are NOT snapshotted: they
/// are pure functions of guest memory and are re-derived on restore
/// (`code_dirty` is set, forcing revalidation).
///
/// Host-side device backends (open files, sockets behind virtio) are not
/// serialized either; the host re-binds them after restore.
#[derive(Clone)]
pub struct ReactorSnapshot {
    /// Content-derived id (FNV-1a over parent id, pc, regs, and pages).
    pub id: u64,
    /// The snapshot this one derives from, if any.
    pub parent: Option<u64>,
    /// Sparse guest RAM: physical page number (`addr >> 16`) → page bytes.
    pub pages: std::collections::BTreeMap<u64, Box<[u8; 65536]>>,
    /// Integer register file at snapshot time.
    pub regs: [u64; 32],
    /// Guest pc at snapshot time.
    pub pc: u64,
    /// Privileged hart + device state.
    pub hart: vane_riscv::hart::HartSnapshot,
}

/// FNV-1a over the snapshot's observable state, seeded with the parent id.
fn snapshot_id(
    parent: Option<u64>,
    pc: u64,
    regs: &[u64; 32],
    pages: &std::collections::BTreeMap<u64, Box<[u8; 65536]>>,
) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in parent.unwrap_or(0).to_le_bytes() {
        mix(b);
    }
    for b in pc.to_le_bytes() {
        mix(b);
    }
    for r in regs {
        for b in r.to_le_bytes() {
            mix(b);
        }
    }
    for (pn, page) in pages {
        for b in pn.to_le_bytes() {
            mix(b);
        }
        for b in page.iter() {
            mix(*b);
        }
    }
    h
}

impl Reactor {
    /// Snapshot the whole machine. With encrypted swap active, paged-out
    /// RAM is materialized through the swap layer (page-in on read), so the
    /// snapshot is the complete guest state either way.
    ///
    /// `pc` is the guest pc the caller will resume at: the fueled trace
    /// loop returns it to the driver without storing it in the reactor, so
    /// it must be supplied explicitly (pass the value the last
    /// `trace_run_fueled` returned).
    pub fn snapshot(&self, parent: Option<&ReactorSnapshot>, pc: u64) -> ReactorSnapshot {
        let lock = self.core.lock();
        let mut pages: std::collections::BTreeMap<u64, Box<[u8; 65536]>> =
            std::collections::BTreeMap::new();
        if lock.mem.swap.is_some() {
            // Swap-backed RAM: read every page of the region through the
            // page cache (forces page-in), omitting zero pages.
            let base = lock.mem.swap_base;
            let len = lock.mem.swap_len;
            let count = len >> 16;
            for pn in 0..count {
                let page = {
                    let swap = lock.mem.swap.as_ref().expect("checked above");
                    let mut g = swap.lock();
                    match g.read_page(pn) {
                        Ok(p) => *p,
                        Err(e) => {
                            g.record_error(e);
                            [0u8; 65536]
                        }
                    }
                };
                if !vane_swap::is_zero_page(&page) {
                    pages.insert((base >> 16) + pn, Box::new(page));
                }
            }
        } else {
            pages = lock.mem.pages.clone();
        }
        #[cfg(not(feature = "js-host"))]
        let regs = lock.regs;
        #[cfg(feature = "js-host")]
        let regs = self.save_regs();
        let hart = lock.hart.save();
        let parent_id = parent.map(|p| p.id);
        let id = snapshot_id(parent_id, pc, &regs, &pages);
        ReactorSnapshot {
            id,
            parent: parent_id,
            pages,
            regs,
            pc,
            hart,
        }
    }

    /// Restore a snapshot into this reactor, replacing all guest state.
    /// Trace caches are invalidated (the same pc may now hold different
    /// code under a different `satp`).
    ///
    /// With swap active, every page of the RAM region is rewritten
    /// (snapshot page or a zero page), so no stale bytes survive. Without
    /// swap, the page map is replaced outright.
    pub fn restore(&self, snap: &ReactorSnapshot) -> Result<(), String> {
        let mut lock = self.core.lock();
        if lock.mem.swap.is_some() {
            let len = lock.mem.swap_len;
            let base = lock.mem.swap_base;
            let count = len >> 16;
            let zero = [0u8; 65536];
            for pn in 0..count {
                let key = (base >> 16) + pn;
                let page: &[u8; 65536] = snap.pages.get(&key).map(|p| &**p).unwrap_or(&zero);
                let swap = lock.mem.swap.as_ref().expect("checked above");
                let mut g = swap.lock();
                match g.write_page(pn) {
                    Ok(dst) => *dst = *page,
                    Err(e) => {
                        g.record_error(e);
                        return Err(format!("swap restore page {pn}: {e:?}"));
                    }
                }
            }
        } else {
            lock.mem.pages = snap.pages.clone();
        }
        #[cfg(not(feature = "js-host"))]
        {
            lock.regs = snap.regs;
        }
        #[cfg(feature = "js-host")]
        {
            drop(lock);
            self.restore_regs(&snap.regs);
            lock = self.core.lock();
        }
        lock.pc = snap.pc;
        lock.hart
            .restore(&snap.hart)
            .map_err(|e| format!("{e:?}"))?;
        // Re-derive caches: same pc may map to different code now.
        lock.code_dirty = true;
        Ok(())
    }

    /// Fork the machine: snapshot and restore into a fresh reactor. The two
    /// reactors share no guest state; host-side device backends must be
    /// re-bound on the fork (see [`ReactorSnapshot`]).
    pub fn fork(&self, pc: u64) -> Result<Reactor, String> {
        let snap = self.snapshot(None, pc);
        let r = Reactor::new_with_mem(vane_riscv::Mem::default());
        r.restore(&snap)?;
        Ok(r)
    }

    /// Serialize a snapshot to a standalone, versioned, checksummed byte
    /// artifact (W1.5) — feedable to Weval, storable, transferable. The
    /// lineage (`id`/`parent`) and the sparse page set (zero pages omitted,
    /// swap-backed RAM materialized at capture) round-trip.
    pub fn snapshot_to_bytes(snap: &ReactorSnapshot) -> Vec<u8> {
        use vane_riscv::snapshot_ser::Writer;
        let mut w = Writer::new();
        w.u64(snap.id);
        match snap.parent {
            None => w.u8(0),
            Some(p) => {
                w.u8(1);
                w.u64(p);
            }
        }
        w.u64(snap.pc);
        for r in snap.regs {
            w.u64(r);
        }
        snap.hart.encode(&mut w);
        w.u32(snap.pages.len() as u32);
        for (pn, page) in &snap.pages {
            w.u64(*pn);
            w.bytes(&page[..]);
        }
        vane_riscv::snapshot_ser::seal(&w.b)
    }

    /// Deserialize a snapshot from [`snapshot_to_bytes`](Self::snapshot_to_bytes)
    /// bytes, verifying magic, version, and checksum.
    pub fn snapshot_from_bytes(data: &[u8]) -> Result<ReactorSnapshot, String> {
        use vane_riscv::snapshot_ser::{open, Reader};
        let payload = open(data).map_err(|e| format!("snapshot envelope: {e:?}"))?;
        let mut r = Reader::new(payload);
        let err = |m: &'static str| {
            move |e: vane_riscv::snapshot_ser::SerError| format!("snapshot {m}: {e:?}")
        };
        let id = r.u64().map_err(err("id"))?;
        let parent = match r.u8().map_err(err("parent"))? {
            0 => None,
            1 => Some(r.u64().map_err(err("parent"))?),
            _ => return Err("snapshot parent flag".into()),
        };
        let pc = r.u64().map_err(err("pc"))?;
        let mut regs = [0u64; 32];
        for reg in &mut regs {
            *reg = r.u64().map_err(err("regs"))?;
        }
        let hart =
            vane_riscv::hart::HartSnapshot::decode(&mut r).map_err(|e| format!("hart: {e:?}"))?;
        let mut pages = std::collections::BTreeMap::new();
        let count = r.u32().map_err(err("pages"))?;
        for _ in 0..count {
            let pn = r.u64().map_err(err("page num"))?;
            let bytes = r.bytes(65536).map_err(err("page"))?;
            let mut page = Box::new([0u8; 65536]);
            page.copy_from_slice(bytes);
            pages.insert(pn, page);
        }
        Ok(ReactorSnapshot {
            id,
            parent,
            pages,
            regs,
            pc,
            hart,
        })
    }

    /// Read one byte at a guest VIRTUAL address through the current hart's
    /// page tables (identity when translation is off or fails). The hook
    /// system uses this to follow argument pointers at hooked kernel
    /// entries — e.g. `struct filename *` in `do_execveat_common` — so
    /// captured arguments are read from real kernel memory at real symbol
    /// locations.
    pub fn read_guest_virtual_u8(&self, vaddr: u64) -> u8 {
        let mut lock = self.core.lock();
        let lock = &mut *lock;
        let pa = lock
            .hart
            .translate(&mut lock.mem, vaddr, vane_riscv::hart::Access::Load)
            .unwrap_or(vaddr);
        lock.mem.read_byte(pa)
    }

    /// Read a NUL-terminated string at a guest virtual address (capped at
    /// `max` bytes; a missing terminator truncates).
    pub fn read_guest_virtual_cstr(&self, vaddr: u64, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..max as u64 {
            let b = self.read_guest_virtual_u8(vaddr.wrapping_add(i));
            if b == 0 {
                break;
            }
            out.push(b);
        }
        out
    }

    /// Read `len` bytes at a guest virtual address.
    pub fn read_guest_virtual(&self, vaddr: u64, len: usize) -> Vec<u8> {
        (0..len as u64)
            .map(|i| self.read_guest_virtual_u8(vaddr.wrapping_add(i)))
            .collect()
    }
}

#[cfg(not(feature = "js-host"))]
impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "js-host")]
impl Reactor {
    fn save_regs(&self) -> [u64; 32] {
        std::array::from_fn(|a| Self::reg(self.clone(), a as u8))
    }
    fn restore_regs(&self, a: &[u64; 32]) {
        for (i, a) in a.iter().cloned().enumerate() {
            Self::set_reg(self.clone(), i as u8, a);
        }
    }
}

#[cfg(feature = "js-host")]
#[wasm_bindgen]
impl Reactor {
    /// Construct a Reactor with empty guest memory. Load a guest image with
    /// repeated `write_byte` calls (e.g. one per ELF PT_LOAD byte) before
    /// calling `interp`/`jit_run`.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::new_with_mem(vane_riscv::Mem::default())
    }

    /// Write one byte of guest memory. The counterpart to the reads
    /// `get_page` already exposes; there is no separately-exported `Mem`
    /// type, since every consumer only ever needs byte-at-a-time image
    /// loading before execution starts.
    #[wasm_bindgen(js_name = "write_byte")]
    pub fn write_byte_js(&self, addr: u64, value: u8) {
        self.core.lock().mem.write_byte(addr, value);
    }

    #[wasm_bindgen]
    pub async fn interp(&self, pc: u64) -> Result<JsValue, JsValue> {
        let mut regs = self.save_regs();
        let outcome_slot = std::cell::RefCell::new(None::<JsValue>);
        let err_slot = std::cell::RefCell::new(None::<JsValue>);
        let mut handler = async |regs: &mut [u64; 32]| -> bool {
            self.restore_regs(regs);
            match self._ecall().await {
                Ok(outcome) => {
                    if is_exit(&outcome) {
                        *outcome_slot.borrow_mut() = Some(outcome);
                        return true;
                    }
                }
                Err(e) => {
                    *err_slot.borrow_mut() = Some(e);
                    return true;
                }
            }
            *regs = self.save_regs();
            false
        };
        let result = self.interp_core(pc, &mut regs, &mut handler).await;
        match result {
            Ok(_) => {
                if let Some(e) = err_slot.into_inner() {
                    return Err(e);
                }
                Ok(outcome_slot.into_inner().unwrap_or(JsValue::UNDEFINED))
            }
            Err(e) => {
                self.restore_regs(&regs);
                Err(JsValue::from_str(&e.to_string()))
            }
        }
    }

    /// Select the execution engine: `"interp"` (StackOp trace interpreter,
    /// works without any runtime code generation) or `"jit"` (JS tracing
    /// JIT via `new Function`). Default: `"interp"`.
    #[wasm_bindgen(js_name = "set_engine")]
    pub fn set_engine_js(&self, engine: &str) {
        self.set_engine(match engine {
            "jit" | "js" | "js-jit" => ExecEngine::JsJit,
            _ => ExecEngine::Interp,
        });
    }

    /// The current execution engine (`"interp"` or `"jit"`).
    #[wasm_bindgen(js_name = "get_engine")]
    pub fn get_engine_js(&self) -> String {
        match self.engine() {
            ExecEngine::Interp => "interp".into(),
            ExecEngine::JsJit => "jit".into(),
        }
    }

    /// Engine-dispatching entry point (M1). Runs from `pc` on the engine
    /// selected with `set_engine`. `interp`/`jit_run` remain available for
    /// direct engine selection.
    ///
    /// With `set_engine("interp")` this touches no runtime codegen —
    /// `new Function` is never called — so it works under CSP
    /// `wasm-unsafe-eval`-less pages and other no-JIT environments.
    #[wasm_bindgen]
    pub async fn run(&self, pc: u64) -> Result<JsValue, JsValue> {
        match self.engine() {
            ExecEngine::JsJit => self.jit_run(pc).await,
            ExecEngine::Interp => {
                let outcome_slot = std::cell::RefCell::new(None::<JsValue>);
                let err_slot = std::cell::RefCell::new(None::<JsValue>);
                let mut handler =
                    async |_host: &mut vane_arch::interp::LocalStackHost<Reactor>| -> bool {
                        match self._ecall().await {
                            Ok(outcome) => {
                                if is_exit(&outcome) {
                                    *outcome_slot.borrow_mut() = Some(outcome);
                                    return true;
                                }
                            }
                            Err(e) => {
                                *err_slot.borrow_mut() = Some(e);
                                return true;
                            }
                        }
                        false
                    };
                let result = self.trace_run(pc, &mut handler).await;
                match result {
                    Ok(_) => {
                        if let Some(e) = err_slot.into_inner() {
                            return Err(e);
                        }
                        Ok(outcome_slot.into_inner().unwrap_or(JsValue::UNDEFINED))
                    }
                    Err(msg) => Err(JsValue::from_str(&msg)),
                }
            }
        }
    }

    #[wasm_bindgen(js_name = "ecall")]
    pub async fn _ecall(&self) -> Result<JsValue, JsValue> {
        let host_api = self
            .core
            .lock()
            .host_api
            .get()
            .cloned()
            .ok_or_else(|| JsValue::from_str("vane requires a configured host API"))?;
        let mut ctx = ReactorCtx {
            reactor: self.clone(),
        };
        let mut host = BrowserHost { host_api };
        match os_unix_emulation::dispatch_async(
            os_unix_emulation::Abi::LinuxRv64,
            &mut ctx,
            &mut host,
        )
        .await
        {
            os_unix_emulation::Outcome::Return(_) => Ok(JsValue::undefined()),
            os_unix_emulation::Outcome::Exit(code) => exit_outcome(code),
        }
    }
}

#[cfg(feature = "js-host")]
struct ReactorCtx {
    reactor: Reactor,
}

#[cfg(feature = "js-host")]
impl os_async::AsyncCtx for ReactorCtx {
    async fn read(&mut self, addr: u64, cell: &mut (dyn os_ctx::ArgCell + '_)) {
        let reactor = self.reactor.clone();
        let mut offset = 0u64;
        cell.fill(&mut core::iter::from_fn(|| {
            let value = reactor.core.lock().mem.read_byte(addr.wrapping_add(offset));
            offset = offset.wrapping_add(1);
            Some(value)
        }));
    }

    async fn write(&mut self, addr: u64, cell: &mut (dyn os_ctx::Arg + '_)) {
        let value = cell.as_addr().map_or(0, core::num::NonZeroU64::get);
        for offset in 0..cell.len() {
            self.reactor
                .core
                .lock()
                .mem
                .write_byte(addr.wrapping_add(offset), (value >> (offset * 8)) as u8);
        }
    }

    async fn reg(&mut self, idx: u8, cell: &mut (dyn os_ctx::ArgCell + '_)) {
        cell.fill(&mut self.reactor.clone().reg(idx).to_le_bytes().iter().copied());
    }

    async fn set_reg(&mut self, idx: u8, cell: &mut (dyn os_ctx::Arg + '_)) {
        self.reactor
            .clone()
            .set_reg(idx, cell.as_addr().map_or(0, core::num::NonZeroU64::get));
    }
}

#[cfg(feature = "js-host")]
struct BrowserHost {
    host_api: JsValue,
}

#[cfg(feature = "js-host")]
impl BrowserHost {
    /// Invoke a named async JavaScript host operation. A missing capability is
    /// ENOSYS; an exception/rejection is EIO. Filesystem methods may return a
    /// Uint8Array for an out buffer, so conversion to an errno/result is kept
    /// at the typed Rust boundary below.
    async fn call_value(&self, name: &str, args: &[JsValue]) -> Result<JsValue, i64> {
        let method = js_sys::Reflect::get(&self.host_api, &JsValue::from_str(name))
            .map_err(|_| -os_unix_emulation::ENOSYS)?
            .dyn_into::<js_sys::Function>()
            .map_err(|_| -os_unix_emulation::ENOSYS)?;
        let array = js_sys::Array::new();
        for arg in args {
            array.push(arg);
        }
        let value = method
            .apply(&self.host_api, &array)
            .map_err(|_| -os_unix_emulation::EIO)?;
        JsFuture::from(js_sys::Promise::resolve(&value))
            .await
            .map_err(|_| -os_unix_emulation::EIO)
    }

    async fn call(&self, name: &str, args: &[JsValue]) -> i64 {
        self.call_value(name, args)
            .await
            .ok()
            .and_then(|value| value.as_f64())
            .map(|value| value as i64)
            .unwrap_or(-os_unix_emulation::ENOSYS)
    }

    async fn stat_call(
        &self,
        name: &str,
        args: &[JsValue],
        stat: &mut os_unix_emulation::LinuxRv64Stat,
    ) -> i64 {
        let value = match self.call_value(name, args).await {
            Ok(value) => value,
            Err(errno) => return errno,
        };
        if let Some(errno) = value.as_f64() {
            return errno as i64;
        }
        let bytes = js_sys::Uint8Array::new(&value);
        if bytes.length() != os_unix_emulation::LINUX_RV64_STAT_LEN as u32 {
            return -os_unix_emulation::EIO;
        }
        bytes.copy_to(&mut stat.0);
        0
    }
}

#[cfg(feature = "js-host")]
impl os_unix_emulation::AsyncUnixHost for BrowserHost {
    async fn read(&mut self, fd: u32, bytes: &mut [u8]) -> i64 {
        let view = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        let result = self
            .call(
                "read",
                &[JsValue::from_f64(fd as f64), JsValue::from(view.clone())],
            )
            .await;
        if result > 0 {
            view.copy_to(bytes);
        }
        result
    }

    async fn write(&mut self, fd: u32, bytes: &[u8]) -> i64 {
        let view = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        view.copy_from(bytes);
        self.call(
            "write",
            &[JsValue::from_f64(fd as f64), JsValue::from(view)],
        )
        .await
    }

    async fn close(&mut self, fd: u32) -> i64 {
        self.call("close", &[JsValue::from_f64(fd as f64)]).await
    }

    async fn exit(&mut self, code: u32) -> i64 {
        self.call("exit", &[JsValue::from_f64(code as f64)]).await
    }

    async fn openat(&mut self, dirfd: i32, path: &[u8], flags: u32, mode: u32) -> i64 {
        let Ok(path) = core::str::from_utf8(path) else {
            return -os_unix_emulation::EINVAL;
        };
        self.call(
            "openat",
            &[
                JsValue::from_f64(dirfd as f64),
                JsValue::from_str(path),
                JsValue::from_f64(flags as f64),
                JsValue::from_f64(mode as f64),
            ],
        )
        .await
    }

    async fn lseek(&mut self, fd: u32, offset: i64, whence: u32) -> i64 {
        self.call(
            "lseek",
            &[
                JsValue::from_f64(fd as f64),
                js_sys::BigInt::from(offset).into(),
                JsValue::from_f64(whence as f64),
            ],
        )
        .await
    }

    async fn fstat(&mut self, fd: u32, stat: &mut os_unix_emulation::LinuxRv64Stat) -> i64 {
        self.stat_call("fstat", &[JsValue::from_f64(fd as f64)], stat)
            .await
    }

    async fn fstatat(
        &mut self,
        dirfd: i32,
        path: &[u8],
        flags: u32,
        stat: &mut os_unix_emulation::LinuxRv64Stat,
    ) -> i64 {
        let Ok(path) = core::str::from_utf8(path) else {
            return -os_unix_emulation::EINVAL;
        };
        self.stat_call(
            "fstatat",
            &[
                JsValue::from_f64(dirfd as f64),
                JsValue::from_str(path),
                JsValue::from_f64(flags as f64),
            ],
            stat,
        )
        .await
    }

    async fn getdents64(&mut self, fd: u32, bytes: &mut [u8]) -> i64 {
        let view = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        let result = self
            .call(
                "getdents64",
                &[JsValue::from_f64(fd as f64), JsValue::from(view.clone())],
            )
            .await;
        if result > 0 {
            let count = core::cmp::min(result as usize, bytes.len());
            view.copy_to(&mut bytes[..count]);
        }
        result
    }
}

#[cfg(feature = "js-host")]
fn exit_outcome(code: u32) -> Result<JsValue, JsValue> {
    let outcome = js_sys::Object::new();
    js_sys::Reflect::set(
        &outcome,
        &JsValue::from_str("kind"),
        &JsValue::from_str("exit"),
    )?;
    js_sys::Reflect::set(
        &outcome,
        &JsValue::from_str("code"),
        &JsValue::from_f64(code as f64),
    )?;
    Ok(outcome.into())
}

#[cfg(feature = "js-host")]
fn is_exit(value: &JsValue) -> bool {
    js_sys::Reflect::get(value, &JsValue::from_str("kind"))
        .ok()
        .and_then(|kind| kind.as_string())
        .as_deref()
        == Some("exit")
}

mod utils;

// Re-export Mem for use in tests
use rv_asm::{Inst, Reg, Xlen};
use std::{cell::OnceCell, rc::Rc, u64};
pub use vane_riscv::hint;
use vane_riscv::template::Params;
pub use vane_riscv::Mem;
use vane_riscv::ReleaseFlate;
use vane_riscv::Riscv;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[cfg(test)]
#[path = "../_tests/rv_corpus.rs"]
mod _tests;
vane_meta_gen::vane_meta!(Reactor, Core, Riscv, ReleaseFlate {});

/// wasm-bindgen calls this once when the module is instantiated.
/// Installs `console_error_panic_hook` so Rust panics surface as readable
/// `console.error` messages (with the panic location/message) instead of an
/// opaque WASM trap.
#[wasm_bindgen(start)]
fn start() {
    utils::set_panic_hook();
}

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

// Non-wasm impl block for Rust-only APIs
impl Reactor {
    /// Create a new Reactor with custom memory
    /// This is a safe constructor that allows initializing the reactor with pre-loaded memory
    pub fn new_with_mem(mem: vane_riscv::Mem) -> Self {
        Reactor {
            _handle: (),
            core: Rc::new(spin::Mutex::new(Core {
                mem,
                state: OnceCell::new(),
                regs: OnceCell::new(),
                host_api: OnceCell::new(),
                test_mode: false,
            })),
        }
    }
    fn save_regs(&self) -> [u64; 32] {
        std::array::from_fn(|a| Self::reg(self.clone(), a as u8))
    }
    fn restore_regs(&self, a: &[u64; 32]) {
        for (i, a) in a.iter().cloned().enumerate() {
            Self::set_reg(self.clone(), i as u8, a);
        }
    }
}
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
    pub async fn interp(&self, mut pc: u64) -> Result<JsValue, JsValue> {
        let test_mode = self.core.lock().test_mode;
        let mut regs = self.save_regs();
        loop {
            regs[0] = 0;
            let code = self.get_page(pc) as *mut u32;
            let code = unsafe { *code };
            let i = Inst::decode(code, Xlen::Rv64);
            let (i, b) = match i {
                Ok(a) => a,
                Err(e) => {
                    self.restore_regs(&regs);
                    return Err(JsValue::from_str(&format!("{e}")));
                }
            };

            // Check for test marker HINTs when test_mode is enabled
            if test_mode {
                if let Some(hint_value) = hint::detect_test_marker(&i) {
                    log_test_hint(pc, hint_value);
                }
            }

            let next = match b {
                rv_asm::IsCompressed::Yes => 2,
                rv_asm::IsCompressed::No => 4,
            } + pc;
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
                ($a:expr => $b:expr) => {
                    match $a {
                        Reg(r) => match $b {
                            v => {
                                regs[(r % 32) as usize] = v;
                                v
                            }
                        },
                    }
                };
            }
            macro_rules! set_reg32 {
                ($a:expr => $b:expr) => {
                    set_reg!($a => match $b{
                        val => (val as i32 as i64 as u64)
                    })
                }
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
                    pc = pc.wrapping_add_signed(offset.as_i64()) & (!1);
                    continue;
                }
                Inst::Jalr { offset, base, dest } => {
                    set_reg!(dest => next);
                    pc = reg!(base).wrapping_add_signed(offset.as_i64()) & (!1);
                    continue;
                }
                //Branches
                Inst::Beq { offset, src1, src2 } => {
                    if reg!(src1) == reg!(src2) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                Inst::Bne { offset, src1, src2 } => {
                    if reg!(src1) != reg!(src2) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                Inst::Bltu { offset, src1, src2 } => {
                    if reg!(src1) < reg!(src2) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                Inst::Bgeu { offset, src1, src2 } => {
                    if reg!(src1) >= reg!(src2) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                Inst::Blt { offset, src1, src2 } => {
                    if (reg!(src1) as i64) < (reg!(src2) as i64) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                Inst::Bge { offset, src1, src2 } => {
                    if (reg!(src1) as i64) >= (reg!(src2) as i64) {
                        pc = pc.wrapping_add_signed(offset.as_i64());
                        continue;
                    }
                }
                //Loads
                Inst::Lb { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut i8)
                        }
                    } as i64 as u64);
                }
                Inst::Lbu { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut u8)
                        }
                    } as u64);
                }
                Inst::Lh { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut i16)
                        }
                    } as i64 as u64);
                }
                Inst::Lhu { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut u16)
                        }
                    } as u64);
                }
                Inst::Lw { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut i32)
                        }
                    } as i64 as u64);
                }
                Inst::Lwu { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut u32)
                        }
                    } as u64);
                }
                Inst::Ld { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            *(p as *mut u64)
                        }
                    });
                }
                //Stores
                Inst::Sb { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        *(ptr as *mut u8) = (reg!(src) & 0xff) as u8;
                    }
                }
                Inst::Sh { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        *(ptr as *mut u16) = (reg!(src) & 0xffff) as u16;
                    }
                }
                Inst::Sw { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        *(ptr as *mut u32) = (reg!(src) & 0xffff_ffff) as u32;
                    }
                }
                Inst::Sd { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        *(ptr as *mut u64) = reg!(src);
                    }
                }
                //Fence
                Inst::Fence { fence: _ } => {}
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
                    set_reg32!(dest => ((reg32!(src1) as i64) >> (imm.as_u32() % 32)) as u32);
                }
                //Regs
                Inst::AddW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1).wrapping_add(reg32!(src2)));
                }
                Inst::SllW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1) << ((reg32!(src2) & 0xffff_ffff) as u32 % 32));
                }
                Inst::SrlW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1) >> ((reg32!(src2) & 0xffff_ffff) as u32 % 32));
                }
                Inst::SraW { src2, dest, src1 } => {
                    set_reg32!(dest => ((reg32!(src1) as i64) >> ((reg32!(src2) & 0xffff_ffff) as u32 % 32)) as u32);
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
                //Ecall
                Inst::Ecall => {
                    self.restore_regs(&regs);
                    let outcome = self._ecall().await?;
                    if is_exit(&outcome) {
                        return Ok(outcome);
                    }
                    regs = self.save_regs();
                }
                _ => {
                    self.restore_regs(&regs);
                    return Err(JsValue::from_str(&format!("iop:{i}")));
                }
            }
            pc = next;
        }
    }
    // #[wasm_bindgen()]
    // pub fn r(&self, a: usize) -> u64 {
    //     if a == 0 {
    //         return 0;
    //     }
    //     unsafe { &mut *self.core.get() }.regs[a - 1]
    // }
    // #[wasm_bindgen]
    // pub fn s(&self, a: usize, x: u64) {
    //     if a == 0 {
    //         return;
    //     }
    //     unsafe { &mut *self.core.get() }.regs[a - 1] = x;
    // }
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

struct ReactorCtx {
    reactor: Reactor,
}

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

struct BrowserHost {
    host_api: JsValue,
}

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
        for arg in args { array.push(arg); }
        let value = method.apply(&self.host_api, &array).map_err(|_| -os_unix_emulation::EIO)?;
        JsFuture::from(js_sys::Promise::resolve(&value)).await.map_err(|_| -os_unix_emulation::EIO)
    }

    async fn call(&self, name: &str, args: &[JsValue]) -> i64 {
        self.call_value(name, args).await.ok()
            .and_then(|value| value.as_f64())
            .map(|value| value as i64)
            .unwrap_or(-os_unix_emulation::ENOSYS)
    }

    async fn stat_call(&self, name: &str, args: &[JsValue], stat: &mut os_unix_emulation::LinuxRv64Stat) -> i64 {
        let value = match self.call_value(name, args).await { Ok(value) => value, Err(errno) => return errno };
        if let Some(errno) = value.as_f64() { return errno as i64; }
        let bytes = js_sys::Uint8Array::new(&value);
        if bytes.length() != os_unix_emulation::LINUX_RV64_STAT_LEN as u32 { return -os_unix_emulation::EIO; }
        bytes.copy_to(&mut stat.0);
        0
    }
}

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
        let Ok(path) = core::str::from_utf8(path) else { return -os_unix_emulation::EINVAL; };
        self.call("openat", &[
            JsValue::from_f64(dirfd as f64), JsValue::from_str(path),
            JsValue::from_f64(flags as f64), JsValue::from_f64(mode as f64),
        ]).await
    }

    async fn lseek(&mut self, fd: u32, offset: i64, whence: u32) -> i64 {
        self.call("lseek", &[
            JsValue::from_f64(fd as f64), js_sys::BigInt::from(offset).into(),
            JsValue::from_f64(whence as f64),
        ]).await
    }

    async fn fstat(&mut self, fd: u32, stat: &mut os_unix_emulation::LinuxRv64Stat) -> i64 {
        self.stat_call("fstat", &[JsValue::from_f64(fd as f64)], stat).await
    }

    async fn fstatat(&mut self, dirfd: i32, path: &[u8], flags: u32, stat: &mut os_unix_emulation::LinuxRv64Stat) -> i64 {
        let Ok(path) = core::str::from_utf8(path) else { return -os_unix_emulation::EINVAL; };
        self.stat_call("fstatat", &[
            JsValue::from_f64(dirfd as f64), JsValue::from_str(path), JsValue::from_f64(flags as f64),
        ], stat).await
    }

    async fn getdents64(&mut self, fd: u32, bytes: &mut [u8]) -> i64 {
        let view = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        let result = self.call("getdents64", &[JsValue::from_f64(fd as f64), JsValue::from(view.clone())]).await;
        if result > 0 {
            let count = core::cmp::min(result as usize, bytes.len());
            view.copy_to(&mut bytes[..count]);
        }
        result
    }
}

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

fn is_exit(value: &JsValue) -> bool {
    js_sys::Reflect::get(value, &JsValue::from_str("kind"))
        .ok()
        .and_then(|kind| kind.as_string())
        .as_deref()
        == Some("exit")
}

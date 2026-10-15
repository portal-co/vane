#[cfg(feature = "js-host")]
mod utils;

// Re-export Mem for use in tests
use rv_asm::{Inst, Reg, Xlen};
use std::rc::Rc;
pub use vane_riscv::hint;
use vane_riscv::template::Params;
pub use vane_riscv::Mem;
/// Engine selection for `run` (interpreter vs JS JIT); see PLAN_LINUX_WASM.md §4.
pub use vane_arch::ExecEngine;
use vane_riscv::ReleaseFlate;
use vane_riscv::Riscv;
#[cfg(feature = "js-host")]
use std::cell::OnceCell;
#[cfg(feature = "js-host")]
use wasm_bindgen::prelude::*;
#[cfg(feature = "js-host")]
use wasm_bindgen_futures::JsFuture;

#[cfg(all(test, feature = "js-host"))]
#[path = "../_tests/rv_corpus.rs"]
mod _tests;
vane_meta_gen::vane_meta!(Reactor, Core, Riscv, ReleaseFlate {}, vane_riscv::template::riscv::rv_emit);

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

// Non-wasm impl block for Rust-only APIs
impl Reactor {
    /// Bulk-write guest memory (e.g. loading an ELF segment). Chunks the
    /// copy across `Mem`'s 64 KiB pages via `get_page`.
    pub fn write_bytes(&self, mut addr: u64, mut data: &[u8]) {
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
                engine: vane_arch::ExecEngine::Interp,
                pc: 0,
                #[cfg(not(feature = "js-host"))]
                regs: [0; 32],
            })),
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
    /// executes `ecall`. It may mutate registers (e.g. set `a0` to the
    /// syscall result) and returns `true` to stop interpretation (the
    /// returned pc then points at the `ecall`) or `false` to continue at
    /// `pc + 4`.
    ///
    /// Note: this loop only terminates via `on_ecall` returning `true` or an
    /// error, matching the historical behavior of the JS-facing `interp`.
    pub async fn interp_core<F>(
        &self,
        mut pc: u64,
        regs: &mut [u64; 32],
        on_ecall: &mut F,
    ) -> Result<u64, InterpError>
    where
        F: AsyncFnMut(&mut [u64; 32]) -> bool,
    {
        #[cfg(feature = "js-host")]
        let test_mode = self.core.lock().test_mode;
        loop {
            regs[0] = 0;
            // Unaligned-safe fetch: with compressed instructions pc can be
            // 2 mod 4, and a raw aligned deref is both UB in Rust and broken
            // on backends that honor the alignment hint (wasm2js lowers an
            // align=4 load to HEAP32[addr >> 2], reading the wrong word).
            let code = self.get_page(pc) as *mut u32;
            let code = unsafe { code.read_unaligned() };
            let i = Inst::decode(code, Xlen::Rv64);
            let (i, b) = match i {
                Ok(a) => a,
                Err(e) => {
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
                            (p as *mut u16).read_unaligned()
                        }
                    } as u64);
                }
                Inst::Lw { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            (p as *mut i32).read_unaligned()
                        }
                    } as i64 as u64);
                }
                Inst::Lwu { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            (p as *mut u32).read_unaligned()
                        }
                    } as u64);
                }
                Inst::Ld { offset, dest, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    set_reg!(dest => match self.get_page(mem){
                        p => unsafe{
                            (p as *mut u64).read_unaligned()
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
                        (ptr as *mut u16).write_unaligned((reg!(src) & 0xffff) as u16);
                    }
                }
                Inst::Sw { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        (ptr as *mut u32).write_unaligned((reg!(src) & 0xffff_ffff) as u32);
                    }
                }
                Inst::Sd { offset, src, base } => {
                    let mem = reg!(base).wrapping_add_signed(offset.as_i64());
                    let ptr = self.get_page(mem);
                    unsafe {
                        (ptr as *mut u64).write_unaligned(reg!(src));
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
                    set_reg32!(dest => reg32!(src1) << ((reg32!(src2) & 0xffff_ffff) % 32));
                }
                Inst::SrlW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1) >> ((reg32!(src2) & 0xffff_ffff) % 32));
                }
                Inst::SraW { src2, dest, src1 } => {
                    set_reg32!(dest => ((reg32!(src1) as i64) >> ((reg32!(src2) & 0xffff_ffff) % 32)) as u32);
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
                    if on_ecall(regs).await {
                        return Ok(pc);
                    }
                }
                _ => {
                    return Err(InterpError::Unsupported(format!("iop:{i}")));
                }
            }
            pc = next;
        }
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

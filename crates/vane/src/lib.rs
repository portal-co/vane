mod utils;
use js_sys::Promise;

// Re-export Mem for use in tests
use rv_asm::{Inst, Reg, Xlen};
use std::{
    cell::{OnceCell, UnsafeCell},
    collections::BTreeMap,
    fmt::Display,
    mem::transmute,
    ptr::null_mut,
    rc::Rc,
    sync::Mutex,
    u64,
};
use vane_jit::{flate::ReleaseFlate, template::TemplateJit};
use vane_jit::template::{CoreJS, Params};
use vane_jit::Heat;
pub use vane_jit::Mem;
use vane_jit::{arch::Riscv, JitCtx};
use wasm_bindgen::prelude::*;

#[cfg(test)]
#[path = "../_tests/rv_corpus.rs"]
mod _tests;
vane_meta_gen::vane_meta!(Reactor, Core, Riscv, ReleaseFlate{});

#[wasm_bindgen(inline_js = r#"
            export function log_success(){
        console.log('success: exit');
        return {__success: true};
    }
    export function has_success(err){
    return '__success' in err;
    }"#)]
extern "C" {
    fn log_success() -> JsValue;
    fn has_success(a: JsValue) -> bool;
}

// Non-wasm impl block for Rust-only APIs
impl Reactor {
    /// Create a new Reactor with custom memory
    /// This is a safe constructor that allows initializing the reactor with pre-loaded memory
    pub fn new_with_mem(mem: vane_jit::Mem) -> Self {
        Reactor {
            _handle: (),
            core: Rc::new(spin::Mutex::new(Core {
                mem,
                state: OnceCell::new(),
                regs: OnceCell::new(),
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
    #[wasm_bindgen]
    pub async fn interp(&self, mut pc: u64) -> Result<JsValue, JsValue> {
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
                    set_reg!(dest => match ((reg!(src1) as i64) < imm.as_i64()){
                        true => 1,
                        false => 0,
                    });
                }
                Inst::Sltiu { imm, dest, src1 } => {
                    set_reg!(dest => match ((reg!(src1)) < imm.as_u64()){
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
                    set_reg!(dest => match ((reg!(src1) as i64) < (reg!(src2) as i64)){
                        true => 1,
                        false => 0,
                    });
                }
                Inst::Sltu { src2, dest, src1 } => {
                    set_reg!(dest => match ((reg!(src1)) < reg!(src2)){
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
                Inst::Div { src2, dest, src1 } => {
                    set_reg!(dest => ((reg!(src1) as i64) / (reg!(src2) as i64)) as u64);
                }
                Inst::Divu { src2, dest, src1 } => {
                    set_reg!(dest => reg!(src1) / reg!(src2));
                }
                Inst::Rem { src2, dest, src1 } => {
                    set_reg!(dest => ((reg!(src1) as i64) % (reg!(src2) as i64)) as u64);
                }
                Inst::Remu { src2, dest, src1 } => {
                    set_reg!(dest => reg!(src1) % reg!(src2));
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
                Inst::Fence { fence } => {}
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
                    set_reg32!(dest => ((reg32!(src1) as i32) / (reg32!(src2) as i32)) as u32);
                }
                Inst::DivuW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1) / reg32!(src2));
                }
                Inst::RemW { src2, dest, src1 } => {
                    set_reg32!(dest => ((reg32!(src1) as i32) % (reg32!(src2) as i32)) as u32);
                }
                Inst::RemuW { src2, dest, src1 } => {
                    set_reg32!(dest => reg32!(src1) % reg32!(src2));
                }
                //Ecall
                Inst::Ecall => {
                    self.restore_regs(&regs);
                    self._ecall().await?;
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
        if self.save_regs()[Reg::A7.0 as usize] == 93 {
            return Err(log_success());
        }
        Ok(JsValue::undefined())
    }
}

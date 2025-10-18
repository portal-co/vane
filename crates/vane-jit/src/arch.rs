use crate::*;
pub trait RiscVDisplay{
    fn riscv(&self, f: &mut Formatter) -> core::fmt::Result;
}
#[derive(Clone, Copy)]
pub struct Riscv<'a>(pub &'a (dyn RiscVDisplay + 'a));
impl Display for Riscv<'_>{
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        self.0.riscv(f)
    }
}
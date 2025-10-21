use core::array;

use crate::{
    arch::{Riscv, RiscvDisplay},
    *,
};
#[derive(Clone, Copy)]
pub struct Params<'a> {
    pub react: &'a (dyn JitCtx + 'a),
    pub trial: &'a (dyn Fn(u64) -> Heat + 'a),
    pub root: u64,
}
pub struct TemplateJit<'a> {
    pub params: Params<'a>,
    pub pc: u64,
    pub labels: &'a BTreeMap<u64, &'a (dyn Display + 'a)>,
}
struct TemplateReg<'a> {
    reg: &'a Reg,
    value: Option<&'a (dyn Display + 'a)>,
}
impl<'a> Display for TemplateReg<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let r = self.reg.0 % 32;
        if r != 0 {
            match self.value.as_deref() {
                None => write!(f, "(($._r??=$.r)[`x{r}`]??=0n)"),
                Some(a) => write!(f, "(($._r??=$.r)[`x{r}`]={a})"),
            }
        } else {
            match self.value.as_deref() {
                None => write!(f, "0n"),
                Some(a) => write!(f, "{a}"),
            }
        }
    }
}
pub mod riscv;
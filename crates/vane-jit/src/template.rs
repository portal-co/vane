use core::array;

use alloc::vec::Vec;
use wasmparser::Operator;

use crate::{
    arch::{Riscv, RiscvDisplay},
    flate::Flate,
    *,
};
#[derive(Clone)]
pub struct Label<'a> {
    ident_name: &'a (dyn Display + 'a),
    index: u32,
}
#[derive(Clone, Default)]
pub struct Labels<'a>(BTreeMap<u64, Label<'a>>);

#[derive(Clone, Copy)]
pub struct Params<'a> {
    pub react: &'a (dyn JitCtx + 'a),
    pub trial: &'a (dyn Fn(u64) -> Heat + 'a),
    pub flate: &'a (dyn Flate + 'a),
    pub root: u64,
    pub flags: Flags,
}

#[derive(Clone, Copy,Default)]
#[non_exhaustive]
pub struct Flags{
    pub test_mode: bool,
}
pub struct TemplateJit<'a> {
    pub params: Params<'a>,
    pub pc: u64,
    pub labels: &'a Labels<'a>,
    pub depth: u32,
}
pub trait TemplateJS {
    type Ty<'a>: Display;
    type Wasm<'a>: WasmJit;
    fn template_jit_js<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Ty<'a>;
    fn template_jit_wasm<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Wasm<'a>;
}

struct TemplateReg<'a, const N: usize = 32> {
    reg: &'a Reg,
    value: Option<&'a (dyn Display + 'a)>,
    n: [(); N],
    flate: &'a (dyn Flate + 'a),
}
impl<'a, const N: usize> Display for TemplateReg<'a, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let r = self.reg.0 & ((N - 1) & 0xff) as u8;
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
impl<'b> TemplateJit<'b> {
    pub fn jit_wasm<'a>(
        &'a self,
        go: impl for<'c> FnOnce(Labels<'c>, u32) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a>,
    ) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a> {
        let mut labels = self.labels.clone();
        match labels.0.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = format!("x{}", self.pc);
                vacant_entry.insert(Label {
                    ident_name: &label_name,
                    index: self.depth,
                });
                let nd = self.depth + 1;
                // let mut i: Vec<_> = Default::default();
                let i = go(labels, nd);
                Box::new(
                    [JitOpcode::Operator {
                        op: Operator::Loop {
                            blockty: wasmparser::BlockType::Empty,
                        },
                    }]
                    .into_iter()
                    .chain(i)
                    .chain([JitOpcode::Operator { op: Operator::End }]),
                )
            }
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => {
                let Label { index, ident_name } = occupied_entry.get();
                Box::new(
                    [JitOpcode::Operator {
                        op: Operator::Br {
                            relative_depth: self.depth - index,
                        },
                    }]
                    .into_iter(),
                )
            }
        }
    }
    pub fn jit_js(
        &self,
        f: &mut Formatter,
        render: impl FnOnce(&mut Formatter, &str, Labels<'_>, u32) -> core::fmt::Result,
    ) -> core::fmt::Result {
        match (self.params.trial)(self.pc) {
            Heat::New => {}
            Heat::Cached => {
                return write!(f, "return J({}n);", self.pc);
            }
        }

        let mut labels = self.labels.clone();
        match labels.0.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = format!("x{}", self.pc);
                vacant_entry.insert(Label {
                    ident_name: &label_name,
                    index: self.depth,
                });
                let nd = self.depth + 1;
                write!(f, "{label_name}: for(;;){{")?;
                render(f, &label_name, labels, nd)?;
                write!(f, "break {label_name};}}",)
            }
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => {
                let Label { index, ident_name } = occupied_entry.get();
                write!(f, "continue {};", &**ident_name)
            }
        }
    }
}
pub struct CoreJS<'a>(pub &'a (dyn Display + 'a), pub &'a (dyn Flate + 'a));
impl<'a> Display for CoreJS<'a> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> core::fmt::Result {
        let max64 = self.1.flate("max64");
        let max32 = self.1.flate("max32");
        let signed = self.1.flate("signed");
        let unsigned = self.1.flate("unsigned");
        let data = self.1.flate("data");
        write!(
            fmt,
            "return async function(){{let {max64}=$.f,{max32}=0xffff_ffffn,{signed}=(a=>BigInt.asIntN(64,a)),{unsigned}=(a=>BigInt.asUintN(64,a)),{data}=(p=>{{p=$.get_page(p);return new DataView($._sys(`memory`).buffer,p);}});{}}}",
            &self.0
        )
    }
}

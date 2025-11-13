use core::array;

use alloc::vec::Vec;
use wasmparser::Operator;

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
    pub labels: &'a BTreeMap<u64, (&'a (dyn Display + 'a), u32)>,
    pub depth: u32,
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
impl<'b> TemplateJit<'b> {
    pub(crate) fn jit_wasm<'a>(
        &'a self,
        go: impl FnOnce(&mut Vec<JitOpcode<'_>>,BTreeMap<u64, (&'_ (dyn Display + '_), u32)>,u32),
    ) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a> {
        let mut labels = self.labels.clone();
        match labels.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = format!("x{}", self.pc);
                vacant_entry.insert((&label_name, self.depth));
                let nd = self.depth + 1;
                let mut i: Vec<_> = Default::default();
                go(&mut i,labels,nd);
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
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => Box::new(
                [JitOpcode::Operator {
                    op: Operator::Br {
                        relative_depth: self.depth - occupied_entry.get().1,
                    },
                }]
                .into_iter(),
            ),
        }
    }
    pub(crate) fn jit_js(
        &self,
        f: &mut Formatter,
        render: impl FnOnce(&mut Formatter,&str,BTreeMap<u64, (&'_ (dyn Display + '_), u32)>,u32) -> core::fmt::Result,
    ) -> core::fmt::Result {
        match (self.params.trial)(self.pc) {
            Heat::New => {}
            Heat::Cached => {
                return write!(f, "return J({}n);", self.pc);
            }
        }

        let mut labels = self.labels.clone();
        match labels.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = format!("x{}", self.pc);
                vacant_entry.insert((&label_name, self.depth));
                let nd = self.depth + 1;
                write!(f, "{label_name}: for(;;){{")?;
                render(f,&label_name,labels,nd)?;
                write!(f, "break {label_name};}}",)
            }
            alloc::collections::btree_map::Entry::Occupied(occupied_entry) => {
                write!(f, "continue {};", occupied_entry.get().0)
            }
        }
    }
}

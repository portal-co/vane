#[cfg(feature = "target-wasm")]
use either::Either;
#[cfg(feature = "target-wasm")]
use wasmparser::Operator;

use crate::*;

#[cfg(feature = "target-js")]
use crate::{Flate, PagingMode};

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
    #[cfg(feature = "target-js")]
    pub flate: &'a (dyn Flate + 'a),
    pub root: u64,
    pub flags: Flags,
}

#[derive(Clone, Copy, Default)]
#[non_exhaustive]
pub struct Flags {
    pub test_mode: bool,
    #[cfg(feature = "target-js")]
    pub paging_mode: Option<PagingMode>,
    #[cfg(feature = "target-js")]
    pub shared_page_table_vaddr: Option<u64>,
    #[cfg(feature = "target-js")]
    pub shared_security_directory_vaddr: Option<u64>,
    #[cfg(feature = "target-js")]
    pub use_32bit_paging: bool,
    #[cfg(feature = "target-js")]
    pub use_multilevel_paging: bool,
}

impl Flags {
    /// Create a new Flags with the specified test_mode value.
    /// This is the preferred way to create Flags with non-default values
    /// since the struct is non-exhaustive.
    pub fn new_with_test_mode(test_mode: bool) -> Self {
        Self {
            test_mode,
            ..Default::default()
        }
    }

    /// Create Flags with paging configuration
    #[cfg(feature = "target-js")]
    pub fn with_paging(
        test_mode: bool,
        paging_mode: PagingMode,
        shared_page_table_vaddr: Option<u64>,
        shared_security_directory_vaddr: Option<u64>,
        use_32bit_paging: bool,
        use_multilevel_paging: bool,
    ) -> Self {
        Self {
            test_mode,
            paging_mode: Some(paging_mode),
            shared_page_table_vaddr,
            shared_security_directory_vaddr,
            use_32bit_paging,
            use_multilevel_paging,
        }
    }
}

pub struct TemplateJit<'a> {
    pub params: Params<'a>,
    pub pc: u64,
    pub labels: &'a Labels<'a>,
    pub depth: u32,
}

/// Arch-target dispatch trait.
///
/// This is an implementation detail used by the [`renders!`] macro to wire a
/// concrete architecture renderer (e.g. `TemplateRiscv`) to the active set of
/// codegen backends.  It is not part of the public API and its shape changes
/// with the enabled feature set.
#[doc(hidden)]
pub trait ArchTarget {
    #[cfg(feature = "target-js")]
    type Js<'a>: Display;
    #[cfg(feature = "target-wasm")]
    type Wasm<'a>: WasmJit;

    #[cfg(feature = "target-js")]
    fn target_js<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Js<'a>;
    #[cfg(feature = "target-wasm")]
    fn target_wasm<'a>(&self, j: &'a TemplateJit<'_>) -> Self::Wasm<'a>;
}

impl<'b> TemplateJit<'b> {
    #[cfg(feature = "target-wasm")]
    pub fn jit_wasm<'a, I: Iterator<Item = JitOpcode<'a>> + 'a>(
        &'a self,
        go: impl for<'c> FnOnce(Labels<'c>, u32) -> I,
    ) -> impl Iterator<Item = JitOpcode<'a>> + 'a {
        let mut labels = self.labels.clone();
        match labels.0.entry(self.pc) {
            alloc::collections::btree_map::Entry::Vacant(vacant_entry) => {
                let label_name = alloc::format!("x{}", self.pc);
                vacant_entry.insert(Label {
                    ident_name: &label_name,
                    index: self.depth,
                });
                let nd = self.depth + 1;
                let i = go(labels, nd);
                Either::Left(
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
                let Label { index, ident_name: _ } = occupied_entry.get();
                Either::Right(
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

    #[cfg(feature = "target-js")]
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
                let label_name = alloc::format!("x{}", self.pc);
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
                let Label { index: _, ident_name } = occupied_entry.get();
                write!(f, "continue {};", &**ident_name)
            }
        }
    }
}

/// Core JavaScript code generator with paging support.
///
/// Generates the outer JS wrapper that declares the arithmetic helpers and the
/// `data` memory-access function for a JIT-compiled block.
///
/// # Paging in JavaScript
/// The generated `data` function performs address translation based on paging_mode:
/// - Legacy: `data = (p => { p = $.get_page(p); return new DataView(...); })`
/// - Shared/Both: Uses inline page table translation
///
/// See PAGING.md for detailed documentation on the paging system.
#[cfg(feature = "target-js")]
pub struct CoreJS<'a> {
    pub content: &'a (dyn Display + 'a),
    pub flate: &'a (dyn Flate + 'a),
    pub flags: Flags,
}

#[cfg(feature = "target-js")]
impl<'a> Display for CoreJS<'a> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> core::fmt::Result {
        let max64 = self.flate.flate("max64");
        let max32 = self.flate.flate("max32");
        let signed = self.flate.flate("signed");
        let unsigned = self.flate.flate("unsigned");
        let data = self.flate.flate("data");

        write!(
            fmt,
            "return async function(){{let {max64}=$.f,{max32}=0xffff_ffffn,{signed}=(a=>BigInt.asIntN(64,a)),{unsigned}=(a=>BigInt.asUintN(64,a)),",
        )?;

        self.write_data_function(fmt, &data)?;

        write!(fmt, ";{}}}", &self.content)
    }
}

#[cfg(feature = "target-js")]
impl<'a> CoreJS<'a> {
    fn write_read_helpers(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "    let read_u64=(addr)=>{{")?;
        writeln!(
            f,
            "        let dv=new DataView($._sys('memory').buffer,$.get_page(addr),8);"
        )?;
        writeln!(f, "        return dv.getBigUint64(0,true);")?;
        writeln!(f, "    }};")?;

        if self.flags.use_32bit_paging {
            writeln!(f, "    let read_u32=(addr)=>{{")?;
            writeln!(
                f,
                "        let dv=new DataView($._sys('memory').buffer,$.get_page(addr),4);"
            )?;
            writeln!(f, "        return dv.getUint32(0,true);")?;
            writeln!(f, "    }};")?;
        }

        Ok(())
    }

    fn write_multilevel_traversal(
        &self,
        f: &mut Formatter<'_>,
        pt_vaddr: u64,
        sd_vaddr: u64,
    ) -> core::fmt::Result {
        if self.flags.use_32bit_paging {
            writeln!(f, "    let l3_idx=(v>>48n)&0xFFFFn;")?;
            writeln!(f, "    let l2_table_vaddr=BigInt(read_u32({pt_vaddr}n+(l3_idx<<2n)));")?;
            writeln!(f, "    let l2_idx=(v>>32n)&0xFFFFn;")?;
            writeln!(f, "    let l1_table_vaddr=BigInt(read_u32(l2_table_vaddr+(l2_idx<<2n)));")?;
            writeln!(f, "    let l1_idx=(v>>16n)&0xFFFFn;")?;
            writeln!(f, "    let page_pointer=read_u32(l1_table_vaddr+(l1_idx<<2n));")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFF;")?;
            writeln!(f, "    let page_base_low24=page_pointer>>8;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top8=sec_entry>>56n;")?;
            writeln!(f, "    let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));")?;
        } else {
            writeln!(f, "    let l3_idx=(v>>48n)&0xFFFFn;")?;
            writeln!(f, "    let l2_table_vaddr=read_u64({pt_vaddr}n+(l3_idx<<3n));")?;
            writeln!(f, "    let l2_idx=(v>>32n)&0xFFFFn;")?;
            writeln!(f, "    let l1_table_vaddr=read_u64(l2_table_vaddr+(l2_idx<<3n));")?;
            writeln!(f, "    let l1_idx=(v>>16n)&0xFFFFn;")?;
            writeln!(f, "    let page_pointer=read_u64(l1_table_vaddr+(l1_idx<<3n));")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFFFFn;")?;
            writeln!(f, "    let page_base_low48=page_pointer>>16n;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top16=sec_entry>>48n;")?;
            writeln!(f, "    let phys_page_base=(page_base_top16<<48n)|page_base_low48;")?;
        }
        Ok(())
    }

    fn write_single_level_lookup(
        &self,
        f: &mut Formatter<'_>,
        pt_vaddr: u64,
        sd_vaddr: u64,
    ) -> core::fmt::Result {
        writeln!(f, "    let page_num=v>>16n;")?;

        if self.flags.use_32bit_paging {
            writeln!(f, "    let entry_addr={pt_vaddr}n+(page_num<<2n);")?;
            writeln!(f, "    let dv=new DataView($._sys('memory').buffer,$.get_page(entry_addr),4);")?;
            writeln!(f, "    let page_pointer=dv.getUint32(0,true);")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFF;")?;
            writeln!(f, "    let page_base_low24=page_pointer>>8;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top8=sec_entry>>56n;")?;
            writeln!(f, "    let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));")?;
        } else {
            writeln!(f, "    let entry_addr={pt_vaddr}n+(page_num<<3n);")?;
            writeln!(f, "    let page_pointer=read_u64(entry_addr);")?;
            writeln!(f, "    let sec_idx=page_pointer&0xFFFFn;")?;
            writeln!(f, "    let page_base_low48=page_pointer>>16n;")?;
            writeln!(f, "    let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);")?;
            writeln!(f, "    let sec_entry=read_u64(sec_entry_addr);")?;
            writeln!(f, "    let page_base_top16=sec_entry>>48n;")?;
            writeln!(f, "    let phys_page_base=(page_base_top16<<48n)|page_base_low48;")?;
        }
        Ok(())
    }

    fn write_data_function(
        &self,
        f: &mut Formatter<'_>,
        data_var: &dyn Display,
    ) -> core::fmt::Result {
        match self.flags.paging_mode {
            Some(PagingMode::Shared) | Some(PagingMode::Both) => {
                let pt_vaddr = self.flags.shared_page_table_vaddr.unwrap_or(0);
                let sd_vaddr = self.flags.shared_security_directory_vaddr.unwrap_or(0);

                writeln!(f, "{data_var}=(v=>{{")?;
                self.write_read_helpers(f)?;

                if self.flags.use_multilevel_paging {
                    self.write_multilevel_traversal(f, pt_vaddr, sd_vaddr)?;
                } else {
                    self.write_single_level_lookup(f, pt_vaddr, sd_vaddr)?;
                }

                writeln!(f, "    let p=phys_page_base+(v&0xFFFFn);")?;
                writeln!(f, "    return new DataView($._sys(`memory`).buffer,$.get_page(p));")?;
                write!(f, "}})")
            }
            _ => {
                write!(
                    f,
                    "{data_var}=(p=>{{p=$.get_page(p);return new DataView($._sys(`memory`).buffer,p);}})"
                )
            }
        }
    }
}

use core::array;

use wasmparser::Operator;

use crate::{
    arch::RiscvDisplay,
    flate::Flate,
    PagingMode,
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

#[derive(Clone, Copy, Default)]
#[non_exhaustive]
pub struct Flags {
    pub test_mode: bool,
    pub paging_mode: Option<PagingMode>,
    pub shared_page_table_vaddr: Option<u64>,
    pub shared_security_directory_vaddr: Option<u64>,
    pub use_32bit_paging: bool,
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
/// Core JavaScript code generator with paging support
///
/// This struct generates JavaScript code that includes:
/// - Helper functions for 64-bit arithmetic
/// - Memory access through the paging system via `$.get_page()`
///
/// # Paging in JavaScript
/// The generated `data` function performs address translation based on paging_mode:
/// - Legacy: `data = (p => { p = $.get_page(p); return new DataView(...); })`
/// - Shared/Both: Uses inline page table translation
///
/// See PAGING.md for detailed documentation on the paging system.
pub struct CoreJS<'a> {
    pub content: &'a (dyn Display + 'a),
    pub flate: &'a (dyn Flate + 'a),
    pub flags: Flags,
}
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
        
        // Generate the data function based on paging mode
        self.write_data_function(fmt, &data)?;
        
        write!(fmt, ";{}}}", &self.content)
    }
}

impl<'a> CoreJS<'a> {
    /// Write the data function based on paging configuration
    fn write_data_function(&self, f: &mut Formatter<'_>, data_var: &dyn Display) -> core::fmt::Result {
        match self.flags.paging_mode {
            Some(PagingMode::Shared) | Some(PagingMode::Both) => {
                let pt_vaddr = self.flags.shared_page_table_vaddr.unwrap_or(0);
                let sd_vaddr = self.flags.shared_security_directory_vaddr.unwrap_or(0);

                if self.flags.use_multilevel_paging {
                    if self.flags.use_32bit_paging {
                        write!(f, "{data_var}=(v=>{{let read_u32=(addr)=>{{let val=0;for(let i=0;i<4;i++){{val|=(new Uint8Array($._sys('memory').buffer,$.get_page(addr+BigInt(i)),1)[0]<<(i*8));}}return val;}};let read_u64=(addr)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(addr+i),1)[0])<<(i*8n));}}return val;}};let l3_idx=(v>>48n)&0xFFFFn;let l2_table_vaddr=BigInt(read_u32({pt_vaddr}n+(l3_idx<<2n)));let l2_idx=(v>>32n)&0xFFFFn;let l1_table_vaddr=BigInt(read_u32(l2_table_vaddr+(l2_idx<<2n)));let l1_idx=(v>>16n)&0xFFFFn;let page_pointer=read_u32(l1_table_vaddr+(l1_idx<<2n));let sec_idx=page_pointer&0xFF;let page_base_low24=page_pointer>>8;let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);let sec_entry=read_u64(sec_entry_addr);let page_base_top8=sec_entry>>56n;let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));let p=phys_page_base+(v&0xFFFFn);return new DataView($._sys(`memory`).buffer,$.get_page(p));}})")
                    } else {
                        write!(f, "{data_var}=(v=>{{let read_u64=(addr)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(addr+i),1)[0])<<(i*8n));}}return val;}};let l3_idx=(v>>48n)&0xFFFFn;let l2_table_vaddr=read_u64({pt_vaddr}n+(l3_idx<<3n));let l2_idx=(v>>32n)&0xFFFFn;let l1_table_vaddr=read_u64(l2_table_vaddr+(l2_idx<<3n));let l1_idx=(v>>16n)&0xFFFFn;let page_pointer=read_u64(l1_table_vaddr+(l1_idx<<3n));let sec_idx=page_pointer&0xFFFFn;let page_base_low48=page_pointer>>16n;let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);let sec_entry=read_u64(sec_entry_addr);let page_base_top16=sec_entry>>48n;let phys_page_base=(page_base_top16<<48n)|page_base_low48;let p=phys_page_base+(v&0xFFFFn);return new DataView($._sys(`memory`).buffer,$.get_page(p));}})")
                    }
                } else {
                    // Single-level
                    if self.flags.use_32bit_paging {
                        write!(f, "{data_var}=(v=>{{let read_u64=(addr)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(addr+i),1)[0])<<(i*8n));}}return val;}};let page_num=v>>16n;let entry_addr={pt_vaddr}n+(page_num<<2n);let page_pointer=0;for(let i=0;i<4;i++){{page_pointer|=(new Uint8Array($._sys('memory').buffer,$.get_page(entry_addr+BigInt(i)),1)[0]<<(i*8));}}let sec_idx=page_pointer&0xFF;let page_base_low24=page_pointer>>8;let sec_entry_addr={sd_vaddr}n+BigInt(sec_idx<<3);let sec_entry=read_u64(sec_entry_addr);let page_base_top8=sec_entry>>56n;let phys_page_base=BigInt((page_base_top8<<24n)|BigInt(page_base_low24));let p=phys_page_base+(v&0xFFFFn);return new DataView($._sys(`memory`).buffer,$.get_page(p));}})")
                    } else {
                        write!(f, "{data_var}=(v=>{{let read_u64=(addr)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(addr+i),1)[0])<<(i*8n));}}return val;}};let page_num=v>>16n;let entry_addr={pt_vaddr}n+(page_num<<3n);let page_pointer=read_u64(entry_addr);let sec_idx=page_pointer&0xFFFFn;let page_base_low48=page_pointer>>16n;let sec_entry_addr={sd_vaddr}n+(sec_idx<<3n);let sec_entry=read_u64(sec_entry_addr);let page_base_top16=sec_entry>>48n;let phys_page_base=(page_base_top16<<48n)|page_base_low48;let p=phys_page_base+(v&0xFFFFn);return new DataView($._sys(`memory`).buffer,$.get_page(p));}})")
                    }
                }
            }
            _ => {
                // Legacy mode (default)
                write!(f, "{data_var}=(p=>{{p=$.get_page(p);return new DataView($._sys(`memory`).buffer,p);}})")
            }
        }
    }
}


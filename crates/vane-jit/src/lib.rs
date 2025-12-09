#![no_std]
use alloc::format;
use alloc::{boxed::Box, collections::btree_map::BTreeMap};
#[doc(hidden)]
pub use core;
use core::fmt::Display;
use core::fmt::Formatter;
#[doc(hidden)]
pub use paste;
use rv_asm::{Inst, Reg, Xlen};
#[doc(hidden)]
pub extern crate alloc;
pub mod flate;
pub mod hint;
/// Paging mode selector
///
/// Determines which paging system to use for memory translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingMode {
    /// Use the legacy on-demand page allocation system (default)
    /// - 64KB pages stored in BTreeMap
    /// - Automatic page allocation on access
    /// - Best for sparse address spaces
    Legacy,

    /// Use the shared page table system
    /// - Compatible with rift/r52x/speet paging
    /// - Explicit page table with 64KB pages
    /// - Can be inlined into JavaScript for performance
    /// - Supports multi-level paging
    Shared,

    /// Use both systems simultaneously (nested paging)
    /// - Shared page table stored IN legacy system's virtual memory
    /// - Legacy handles page allocation and basic access
    /// - Shared provides explicit address translation on top
    /// - Page table accessed via legacy get_page()
    /// - Allows controlled memory mapping within virtual space
    Both,
}

impl Default for PagingMode {
    fn default() -> Self {
        PagingMode::Legacy
    }
}

/// Memory manager with dual paging support
///
/// This structure provides a 64KB page-based memory system for RISC-V emulation
/// with support for both legacy and shared paging modes.
///
/// # Paging System
/// - **Page Size**: 64KB (65536 bytes)
/// - **Page Number**: bits [63:16] of virtual address
/// - **Page Offset**: bits [15:0] of virtual address
///
/// # Nested Paging (Both mode)
/// When using `PagingMode::Both`, the shared page table is stored within the
/// legacy system's virtual address space. This provides two levels of indirection:
/// 1. Legacy system maps page table storage to physical memory
/// 2. Shared system uses page table entries within that virtual memory
///
/// This allows the shared system to provide controlled address translation while
/// the legacy system handles actual page allocation.
///
/// See PAGING.md for detailed documentation on the paging system.
#[derive(Default)]
pub struct Mem {
    /// Legacy paging: on-demand allocated pages
    pub pages: BTreeMap<u64, Box<[u8; 65536]>>,

    /// Paging mode selection
    pub paging_mode: PagingMode,

    /// Shared paging: virtual address of page table base (in legacy address space)
    /// When using Both mode, this address is translated through legacy system
    pub shared_page_table_vaddr: Option<u64>,
    
    /// Shared paging: virtual address of security directory (in legacy address space)
    pub shared_security_directory_vaddr: Option<u64>,
    
    /// Use 32-bit page table entries instead of 64-bit
    pub use_32bit_paging: bool,
    
    /// Use multi-level (3-level) page tables instead of single-level
    pub use_multilevel_paging: bool,
}
impl Mem {
    /// Get a pointer to a specific address in memory (legacy system)
    ///
    /// This function implements the base paging system by:
    /// 1. Extracting the page number (bits 63:16)
    /// 2. Allocating the page if it doesn't exist
    /// 3. Returning a pointer to the offset within the page (bits 15:0)
    ///
    /// When using PagingMode::Both, this is the first level of translation.
    /// The shared page table itself is stored in this virtual address space.
    ///
    /// # Arguments
    /// * `a` - Virtual address (in legacy address space)
    ///
    /// # Returns
    /// Raw pointer to the byte at the virtual address
    pub fn get_page(&mut self, a: u64) -> *mut u8 {
        match self
            .pages
            .entry(a >> 16)
            .or_insert_with(|| Box::new([0u8; 65536]))
        {
            p => &raw mut p[(a & 0xffff) as usize],
        }
    }

    /// Translate address through shared page table (nested in legacy memory)
    pub fn translate_shared(&mut self, vaddr: u64) -> u64 {
        let pt_base = self.shared_page_table_vaddr.expect("shared_page_table_vaddr must be set");
        let sec_dir_base = self.shared_security_directory_vaddr.expect("shared_security_directory_vaddr must be set");

        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;

        let entry_vaddr = pt_base + (page_num * 8);
        let mut page_pointer_bytes = [0u8; 8];
        for i in 0u64..8 {
            page_pointer_bytes[i as usize] = self.read_byte(entry_vaddr + i);
        }
        let page_pointer = u64::from_le_bytes(page_pointer_bytes);

        let sec_idx = page_pointer & 0xFFFF;
        let page_base_low48 = page_pointer >> 16;

        let sec_entry_vaddr = sec_dir_base + (sec_idx * 8);
        let mut sec_entry_bytes = [0u8; 8];
        for i in 0u64..8 {
            sec_entry_bytes[i as usize] = self.read_byte(sec_entry_vaddr + i);
        }
        let sec_entry = u64::from_le_bytes(sec_entry_bytes);
        let page_base_top16 = sec_entry >> 48;

        let phys_page_base = (page_base_top16 << 48) | page_base_low48;
        phys_page_base + page_offset
    }

    /// Translate address using multi-level page table (nested in legacy memory)
    pub fn translate_shared_multilevel(&mut self, vaddr: u64) -> u64 {
        let l3_table_vaddr = self.shared_page_table_vaddr.expect("shared_page_table_vaddr must be set");
        let sec_dir_base = self.shared_security_directory_vaddr.expect("shared_security_directory_vaddr must be set");

        let read_u64 = |mem: &mut Self, addr: u64| -> u64 {
            let mut bytes = [0u8; 8];
            for i in 0u64..8 {
                bytes[i as usize] = mem.read_byte(addr + i);
            }
            u64::from_le_bytes(bytes)
        };

        // L3
        let l3_idx = (vaddr >> 48) & 0xFFFF;
        let l3_entry_addr = l3_table_vaddr + (l3_idx * 8);
        let l2_table_vaddr = read_u64(self, l3_entry_addr);

        // L2
        let l2_idx = (vaddr >> 32) & 0xFFFF;
        let l2_entry_addr = l2_table_vaddr + (l2_idx * 8);
        let l1_table_vaddr = read_u64(self, l2_entry_addr);

        // L1
        let l1_idx = (vaddr >> 16) & 0xFFFF;
        let l1_entry_addr = l1_table_vaddr + (l1_idx * 8);
        let page_pointer = read_u64(self, l1_entry_addr);

        let sec_idx = page_pointer & 0xFFFF;
        let page_base_low48 = page_pointer >> 16;
        
        let sec_entry_vaddr = sec_dir_base + (sec_idx * 4);
        let mut sec_entry_bytes = [0u8; 4];
        for i in 0u64..4 {
            sec_entry_bytes[i as usize] = self.read_byte(sec_entry_vaddr + i);
        }
        let sec_entry = u32::from_le_bytes(sec_entry_bytes);
        let page_base_top16 = (sec_entry >> 16) as u64;

        let phys_page_base = (page_base_top16 << 48) | page_base_low48;
        phys_page_base + (vaddr & 0xFFFF)
    }

    /// Translate a virtual address to a physical address for WASM memory (legacy mode)
    pub fn translate_to_wasm_legacy(&self, vaddr: u64, wasm_memory_base: u64) -> u64 {
        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;
        wasm_memory_base + (page_num * 65536) + page_offset
    }

    /// Generate JavaScript code for shared page table lookup (nested in legacy)
    pub fn generate_shared_paging_js<'a>(
        &'a self,
        vaddr_var: &'a (dyn Display + 'a),
        page_table_vaddr_var: &'a (dyn Display + 'a),
        security_directory_vaddr_var: &'a (dyn Display + 'a),
    ) -> impl Display + 'a {
        struct SharedPaging<'a> {
            vaddr: &'a (dyn Display + 'a),
            pt_base: &'a (dyn Display + 'a),
            sec_dir_base: &'a (dyn Display + 'a),
        }
        impl<'a> Display for SharedPaging<'a> {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                write!(
                    f,
                    "((v,pt,sd)=>{{let page_num=v>>16n;let entry_addr=pt+(page_num<<3n);let page_pointer=0n;for(let i=0n;i<8n;i++){{page_pointer|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(entry_addr+i),1)[0])<<(i*8n));}}let sec_idx=page_pointer&0xFFFFn;let page_base_low48=page_pointer>>16n;let sec_entry_addr=sd+(sec_idx<<2n);let sec_entry=0;for(let i=0;i<4;i++){{sec_entry|=(new Uint8Array($._sys('memory').buffer,$.get_page(sec_entry_addr+BigInt(i)),1)[0]<<(i*8));}}let page_base_top16=BigInt(sec_entry>>16);let phys_page_base=(page_base_top16<<48n)|page_base_low48;return phys_page_base+(v&0xFFFFn);}})({},{},{})",
                    self.vaddr, self.pt_base, self.sec_dir_base
                )
            }
        }
        SharedPaging {
            vaddr: vaddr_var,
            pt_base: page_table_vaddr_var,
            sec_dir_base: security_directory_vaddr_var,
        }
    }

    /// Generate JavaScript code for multi-level page table lookup (nested in legacy)
    pub fn generate_multilevel_paging_js<'a>(
        &'a self,
        vaddr_var: &'a (dyn Display + 'a),
        l3_table_vaddr_var: &'a (dyn Display + 'a),
        security_directory_vaddr_var: &'a (dyn Display + 'a),
    ) -> impl Display + 'a {
        struct MultilevelPaging<'a> {
            vaddr: &'a (dyn Display + 'a),
            l3_base: &'a (dyn Display + 'a),
            sec_dir_base: &'a (dyn Display + 'a),
        }
        impl<'a> Display for MultilevelPaging<'a> {
            fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
                write!(
                    f,
                    "((v,l3,sd)=>{{let read_u64=(addr)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(addr+i),1)[0])<<(i*8n));}}return val;}};let l3_idx=(v>>48n)&0xFFFFn;let l2_table_vaddr=read_u64(l3+(l3_idx<<3n));let l2_idx=(v>>32n)&0xFFFFn;let l1_table_vaddr=read_u64(l2_table_vaddr+(l2_idx<<3n));let l1_idx=(v>>16n)&0xFFFFn;let page_pointer=read_u64(l1_table_vaddr+(l1_idx<<3n));let sec_idx=page_pointer&0xFFFFn;let page_base_low48=page_pointer>>16n;let sec_entry_addr=sd+(sec_idx<<2n);let sec_entry=0;for(let i=0;i<4;i++){{sec_entry|=(new Uint8Array($._sys('memory').buffer,$.get_page(sec_entry_addr+BigInt(i)),1)[0]<<(i*8));}}let page_base_top16=BigInt(sec_entry>>16);let phys_page_base=(page_base_top16<<48n)|page_base_low48;return phys_page_base+(v&0xFFFFn);}})({},{},{})",
                    self.vaddr, self.l3_base, self.sec_dir_base
                )
            }
        }
        MultilevelPaging {
            vaddr: vaddr_var,
            l3_base: l3_table_vaddr_var,
            sec_dir_base: security_directory_vaddr_var,
        }
    }

    /// Translate address through shared page table with 32-bit physical addresses
    pub fn translate_shared_32(&mut self, vaddr: u64) -> u64 {
        let pt_base = self.shared_page_table_vaddr.expect("shared_page_table_vaddr must be set");
        let sec_dir_base = self.shared_security_directory_vaddr.expect("shared_security_directory_vaddr must be set");

        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;

        let entry_vaddr = pt_base + (page_num * 4);
        let mut page_pointer_bytes = [0u8; 4];
        for i in 0u64..4 {
            page_pointer_bytes[i as usize] = self.read_byte(entry_vaddr + i);
        }
        let page_pointer = u32::from_le_bytes(page_pointer_bytes);

        let sec_idx = (page_pointer & 0xFF) as u64;
        let page_base_low24 = (page_pointer >> 8) as u64;
        
        let sec_entry_vaddr = sec_dir_base + (sec_idx * 4);
        let mut sec_entry_bytes = [0u8; 4];
        for i in 0u64..4 {
            sec_entry_bytes[i as usize] = self.read_byte(sec_entry_vaddr + i);
        }
        let sec_entry = u32::from_le_bytes(sec_entry_bytes);
        let page_base_top8 = (sec_entry >> 24) as u64;

        let phys_page_base = (page_base_top8 << 24) | page_base_low24;
        phys_page_base + page_offset
    }

    /// Translate address using multi-level page table with 32-bit physical addresses
    pub fn translate_shared_multilevel_32(&mut self, vaddr: u64) -> u64 {
        let l3_table_vaddr = self.shared_page_table_vaddr.expect("shared_page_table_vaddr must be set");
        let sec_dir_base = self.shared_security_directory_vaddr.expect("shared_security_directory_vaddr must be set");
        
        let read_u32 = |mem: &mut Self, addr: u64| -> u32 {
            let mut bytes = [0u8; 4];
            for i in 0u64..4 {
                bytes[i as usize] = mem.read_byte(addr + i);
            }
            u32::from_le_bytes(bytes)
        };

        // L3
        let l3_idx = (vaddr >> 48) & 0xFFFF;
        let l3_entry_addr = l3_table_vaddr + (l3_idx * 4);
        let l2_table_vaddr = read_u32(self, l3_entry_addr) as u64;

        // L2
        let l2_idx = (vaddr >> 32) & 0xFFFF;
        let l2_entry_addr = l2_table_vaddr + (l2_idx * 4);
        let l1_table_vaddr = read_u32(self, l2_entry_addr) as u64;

        // L1
        let l1_idx = (vaddr >> 16) & 0xFFFF;
        let l1_entry_addr = l1_table_vaddr + (l1_idx * 4);
        let page_pointer = read_u32(self, l1_entry_addr);

        let sec_idx = (page_pointer & 0xFF) as u64;
        let page_base_low24 = (page_pointer >> 8) as u64;

        let sec_entry_vaddr = sec_dir_base + (sec_idx * 4);
        let sec_entry = read_u32(self, sec_entry_vaddr);
        let page_base_top8 = (sec_entry >> 24) as u64;

        let phys_page_base = (page_base_top8 << 24) | page_base_low24;
        phys_page_base + (vaddr & 0xFFFF)
    }


    /// Safe interface to write a byte to memory
    pub fn write_byte(&mut self, addr: u64, value: u8) {
        let page = self
            .pages
            .entry(addr >> 16)
            .or_insert_with(|| Box::new([0u8; 65536]));
        page[(addr & 0xffff) as usize] = value;
    }

    /// Safe interface to read a byte from memory
    pub fn read_byte(&self, addr: u64) -> u8 {
        self.pages
            .get(&(addr >> 16))
            .map(|page| page[(addr & 0xffff) as usize])
            .unwrap_or(0)
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Heat {
    New,
    Cached,
}
pub trait JitCtx {
    fn bytes(&self, a: u64) -> Box<dyn Iterator<Item = u8> + '_>;
}
impl JitCtx for Mem {
    fn bytes(&self, a: u64) -> Box<dyn Iterator<Item = u8> + '_> {
        Box::new((a..).map(|a| match self.pages.get(&(a >> 16)) {
            None => 0u8,
            Some(i) => i[(a & 0xffff) as usize],
        }))
    }
}
pub trait WasmJitCtx {}
#[derive(Clone)]
pub enum JitOpcode<'a> {
    Operator { op: wasmparser::Operator<'a> },
}
pub trait WasmJit {
    fn jit<'a>(
        &'a self,
        ctx: &'a (dyn WasmJitCtx + 'a),
    ) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a>;
}
pub mod arch;
pub mod template;

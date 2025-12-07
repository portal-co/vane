#![no_std]
use alloc::format;
use alloc::{boxed::Box, collections::btree_map::BTreeMap};
use core::fmt::Formatter;
use core::fmt::Display;
use rv_asm::{Inst, Reg, Xlen};
#[doc(hidden)]
pub use core;
#[doc(hidden)]
pub use paste;
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
    ///
    /// This performs two-level translation when using PagingMode::Both:
    /// 1. Use shared page table (stored at shared_page_table_vaddr in legacy space)
    /// 2. Look up physical page base using shared table
    /// 3. Return final address
    ///
    /// The page table is accessed through get_page(), so it benefits from
    /// legacy on-demand allocation.
    ///
    /// # Arguments
    /// * `vaddr` - Virtual address to translate via shared system
    ///
    /// # Returns
    /// Physical address after shared page table translation
    ///
    /// # Panics
    /// Panics if paging_mode is not Both or shared_page_table_vaddr is None
    pub fn translate_shared(&mut self, vaddr: u64) -> u64 {
        let pt_base = self.shared_page_table_vaddr
            .expect("shared_page_table_vaddr must be set for shared translation");
        
        // Extract page number from virtual address
        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;
        
        // Calculate page table entry address (in legacy virtual space)
        // Each entry is 8 bytes (u64), so: pt_base + (page_num * 8)
        let entry_vaddr = pt_base + (page_num * 8);
        
        // Read physical page base from page table (via legacy system)
        // This will automatically allocate pages as needed via get_page()
        let mut phys_page_bytes = [0u8; 8];
        for i in 0..8 {
            phys_page_bytes[i] = self.read_byte(entry_vaddr + i);
        }
        let phys_page = u64::from_le_bytes(phys_page_bytes);
        
        // Combine physical page base with offset
        phys_page + page_offset
    }
    
    /// Translate address using multi-level page table (nested in legacy memory)
    ///
    /// This performs two-level translation with a 3-level page table structure:
    /// 1. Page table stored at l3_table_vaddr in legacy address space
    /// 2. Walk through L3 → L2 → L1 tables
    /// 3. Return final physical address
    ///
    /// All table accesses go through legacy system for on-demand allocation.
    ///
    /// # Arguments
    /// * `vaddr` - Virtual address to translate
    /// * `l3_table_vaddr` - Virtual address of level 3 table (in legacy space)
    ///
    /// # Returns
    /// Physical address after multi-level translation
    pub fn translate_shared_multilevel(&mut self, vaddr: u64, l3_table_vaddr: u64) -> u64 {
        // Helper to read u64 from legacy virtual memory
        let read_u64 = |mem: &mut Self, addr: u64| -> u64 {
            let mut bytes = [0u8; 8];
            for i in 0..8 {
                bytes[i] = mem.read_byte(addr + i);
            }
            u64::from_le_bytes(bytes)
        };
        
        // Level 3: bits [63:48]
        let l3_idx = (vaddr >> 48) & 0xFFFF;
        let l3_entry_addr = l3_table_vaddr + (l3_idx * 8);
        let l2_table_vaddr = read_u64(self, l3_entry_addr);
        
        // Level 2: bits [47:32]
        let l2_idx = (vaddr >> 32) & 0xFFFF;
        let l2_entry_addr = l2_table_vaddr + (l2_idx * 8);
        let l1_table_vaddr = read_u64(self, l2_entry_addr);
        
        // Level 1: bits [31:16]
        let l1_idx = (vaddr >> 16) & 0xFFFF;
        let l1_entry_addr = l1_table_vaddr + (l1_idx * 8);
        let phys_page = read_u64(self, l1_entry_addr);
        
        // Page offset: bits [15:0]
        let page_offset = vaddr & 0xFFFF;
        
        phys_page + page_offset
    }

    /// Translate a virtual address to a physical address for WASM memory (legacy mode)
    ///
    /// This function provides address translation for targeting WebAssembly linear memory
    /// using identity mapping (no page table lookup).
    ///
    /// # Arguments
    /// * `vaddr` - Virtual address to translate
    /// * `wasm_memory_base` - Base offset in WASM linear memory where pages are mapped
    ///
    /// # Returns
    /// Physical offset in WASM linear memory
    ///
    /// # Page Mapping
    /// The physical address is computed as:
    /// ```text
    /// physical = wasm_memory_base + (page_number * 65536) + page_offset
    /// ```
    ///
    /// Where:
    /// - `page_number = vaddr >> 16`
    /// - `page_offset = vaddr & 0xFFFF`
    pub fn translate_to_wasm_legacy(&self, vaddr: u64, wasm_memory_base: u64) -> u64 {
        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;
        wasm_memory_base + (page_num * 65536) + page_offset
    }
    
    /// Generate JavaScript code for shared page table lookup (nested in legacy)
    ///
    /// This generates inline JavaScript code that performs page table translation
    /// where the page table is stored in the legacy system's virtual memory.
    /// Each page table access goes through $.get_page() for proper nesting.
    ///
    /// # Arguments
    /// * `vaddr_var` - Name of JavaScript variable containing virtual address
    /// * `page_table_vaddr_var` - Virtual address of page table (in legacy space)
    ///
    /// # Returns
    /// JavaScript expression string that evaluates to the physical address
    ///
    /// # Example
    /// ```ignore
    /// let js = mem.generate_shared_paging_js("vaddr", "pt_base");
    /// // Reads page table entry via $.get_page(), then translates
    /// ```
    pub fn generate_shared_paging_js(&self, vaddr_var: &str, page_table_vaddr_var: &str) -> alloc::string::String {
        format!(
            "((v,pt)=>{{let pn=(v>>16n),po=(v&0xFFFFn),entry_vaddr=pt+(pn<<3n),phys_page=0n;for(let i=0n;i<8n;i++){{phys_page|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(entry_vaddr+i),1)[0])<<(i*8n));}}return phys_page+po;}})({v},{pt})",
            v = vaddr_var,
            pt = page_table_vaddr_var
        )
    }
    
    /// Generate JavaScript code for multi-level page table lookup (nested in legacy)
    ///
    /// This generates inline JavaScript code that performs 3-level page table translation
    /// where all page tables are stored in legacy system's virtual memory.
    /// All table accesses go through $.get_page() for proper nesting.
    ///
    /// # Arguments
    /// * `vaddr_var` - Name of JavaScript variable containing virtual address
    /// * `l3_table_vaddr_var` - Virtual address of level 3 table (in legacy space)
    ///
    /// # Returns
    /// JavaScript expression string that evaluates to the physical address
    pub fn generate_multilevel_paging_js(&self, vaddr_var: &str, l3_table_vaddr_var: &str) -> alloc::string::String {
        format!(
            "((v,l3)=>{{let read_u64=(a)=>{{let val=0n;for(let i=0n;i<8n;i++){{val|=(BigInt(new Uint8Array($._sys('memory').buffer,$.get_page(a+i),1)[0])<<(i*8n));}}return val;}};let l3i=(v>>48n)&0xFFFFn,l2=read_u64(l3+(l3i<<3n)),l2i=(v>>32n)&0xFFFFn,l1=read_u64(l2+(l2i<<3n)),l1i=(v>>16n)&0xFFFFn,pg=read_u64(l1+(l1i<<3n)),po=v&0xFFFFn;return pg+po;}})({v},{l3})",
            v = vaddr_var,
            l3 = l3_table_vaddr_var
        )
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
pub trait WasmJitCtx{

}
#[derive(Clone)]
pub enum JitOpcode<'a> {
    Operator{
        op: wasmparser::Operator<'a>,
    }
}
pub trait WasmJit {
    fn jit<'a>(&'a self, ctx: &'a (dyn WasmJitCtx + 'a)) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a>;
}
pub mod arch;
pub mod template;

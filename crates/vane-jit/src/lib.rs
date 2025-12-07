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
/// Memory manager with paging support
///
/// This structure provides a 64KB page-based memory system for RISC-V emulation.
/// Pages are allocated on-demand and stored in a BTree for efficient lookup.
///
/// # Paging System
/// - **Page Size**: 64KB (65536 bytes)
/// - **Page Number**: bits [63:16] of virtual address
/// - **Page Offset**: bits [15:0] of virtual address
///
/// See PAGING.md for detailed documentation on the paging system.
#[derive(Default)]
pub struct Mem {
    pub pages: BTreeMap<u64, Box<[u8; 65536]>>,
}
impl Mem {
    /// Get a pointer to a specific address in memory
    ///
    /// This function implements the paging system by:
    /// 1. Extracting the page number (bits 63:16)
    /// 2. Allocating the page if it doesn't exist
    /// 3. Returning a pointer to the offset within the page (bits 15:0)
    ///
    /// # Arguments
    /// * `a` - Virtual address
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

    /// Translate a virtual address to a physical address for WASM memory
    ///
    /// This function provides address translation for targeting WebAssembly linear memory.
    /// It maps virtual addresses through the page table to physical offsets in WASM memory.
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
    pub fn translate_to_wasm(&self, vaddr: u64, wasm_memory_base: u64) -> u64 {
        let page_num = vaddr >> 16;
        let page_offset = vaddr & 0xFFFF;
        wasm_memory_base + (page_num * 65536) + page_offset
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

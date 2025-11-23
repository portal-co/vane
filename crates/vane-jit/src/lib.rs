#![no_std]
use alloc::format;
use alloc::{boxed::Box, collections::btree_map::BTreeMap};
use core::fmt::Formatter;
use core::{cell::UnsafeCell, fmt::Display};
use rv_asm::{Inst, Reg, Xlen};
#[doc(hidden)]
pub use core;
#[doc(hidden)]
pub extern crate alloc;
pub mod flate;
#[derive(Default)]
pub struct Mem {
    pub pages: BTreeMap<u64, Box<[u8; 65536]>>,
}
impl Mem {
    pub fn get_page(&mut self, a: u64) -> *mut u8 {
        match self
            .pages
            .entry(a >> 16)
            .or_insert_with(|| Box::new([0u8; 65536]))
        {
            p => &raw mut p[(a & 0xffff) as usize],
        }
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
#[derive(Clone)]
pub enum JitOpcode<'a> {
    Operator{
        op: wasmparser::Operator<'a>,
    }
}
pub trait WasmJit {
    fn jit<'a>(&'a self) -> Box<dyn Iterator<Item = JitOpcode<'a>> + 'a>;
}
pub mod arch;
pub mod template;

#![no_std]

use core::{cell::UnsafeCell, fmt::Display};

use alloc::format;
use alloc::{boxed::Box, collections::btree_map::BTreeMap};
use rv_asm::{Inst, Reg, Xlen};
extern crate alloc;
#[derive(Default)]
pub struct Mem {
    pub pages: BTreeMap<u64, Box<[u8; 65536]>>,
}
impl Mem {
    pub fn get_page(&mut self, a: u64) -> *mut u8 {
        match self
            .pages
            .entry((a >> 16))
            .or_insert_with(|| Box::new([0u8; 65536]))
        {
            p => &raw mut p[(a & 0xffff) as usize],
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Heat {
    New,
    Cached,
}
pub mod template;
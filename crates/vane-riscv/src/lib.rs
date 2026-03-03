#![no_std]
extern crate alloc;

// Re-export everything from vane-arch so downstream users get the full stack
pub use vane_arch::*;

pub mod hint;
pub mod template;

vane_arch::renders!(Riscv);

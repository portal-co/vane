#![no_std]
extern crate alloc;

// Re-export everything from vane-arch so downstream users get the full stack
pub use vane_arch::*;

pub mod state {
    use vane_arch::StateSlot;

    /// Raw scalar F/D register state. F32 values occupy the low 32 bits,
    /// matching the shared StackOp floating-point representation.
    pub fn f(reg: u8) -> StateSlot {
        assert!(reg < 32, "RISC-V has F0 through F31");
        StateSlot::new(alloc::format!("rv:f{reg}"))
    }
}

pub mod hint;
pub mod template;

vane_arch::renders!(Riscv);

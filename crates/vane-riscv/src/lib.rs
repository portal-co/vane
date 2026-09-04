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

#[cfg(feature = "fuzz-traces")]
#[doc(hidden)]
pub mod fuzz {
    //! Test-oriented re-export of the RV64 `StackOp` trace producer.
    //!
    //! Used by `crates/vane-cross-fuzz` to drive `rv_emit`'s traces against
    //! Unicorn; see `CROSS_FUZZ_PLAN.md`. Gated behind `fuzz-traces` so it
    //! never leaks into the `wasm32` cdylib.
    pub use crate::template::riscv::rv_emit;
}

vane_arch::renders!(Riscv);

#![no_std]
#[allow(hidden_glob_reexports)]
extern crate alloc;

pub use vane_arch::*;
pub use vane_target_core::{StackOp, StateSlot};

pub mod reactor;
pub mod template;
pub use reactor::Aarch64Reactor;

vane_arch::renders!(Aarch64);

/// AArch64 architectural state slots used by the first integer/control-flow
/// frontend. The names are stable host-ABI keys, not JavaScript identifiers.
pub mod state {
    use vane_target_core::StateSlot;

    pub fn x(reg: u8) -> StateSlot {
        assert!(
            reg <= 30,
            "AArch64 has X0 through X30; X31 is context-sensitive"
        );
        StateSlot::new(alloc::format!("a64:x{reg}"))
    }

    pub fn sp() -> StateSlot {
        StateSlot::new("a64:sp")
    }

    pub fn scratch(name: char) -> StateSlot {
        assert!(matches!(name, 'a' | 'b' | 'r'));
        StateSlot::new(alloc::format!("a64:scratch:{name}"))
    }

    pub fn nzcv(flag: char) -> StateSlot {
        assert!(matches!(flag, 'n' | 'z' | 'c' | 'v'));
        StateSlot::new(alloc::format!("a64:nzcv:{flag}"))
    }

    /// Raw scalar S/D bits for V0–V31. S values occupy the low 32 bits.
    pub fn v(reg: u8) -> StateSlot {
        assert!(reg <= 31, "AArch64 has V0 through V31");
        StateSlot::new(alloc::format!("a64:v{reg}"))
    }
}

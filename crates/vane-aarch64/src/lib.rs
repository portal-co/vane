#![no_std]
#[allow(hidden_glob_reexports)]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub use vane_arch::*;
pub use vane_target_core::{StackOp, StateSlot};

pub mod reactor;
pub mod template;
pub use reactor::{Aarch64Ctx, Aarch64Reactor, NativeExit};

#[cfg(feature = "fuzz-traces")]
#[doc(hidden)]
pub mod fuzz {
    //! Test-oriented re-export of the AArch64 `StackOp` trace producer.
    //!
    //! Used by `crates/vane-cross-fuzz` to drive `a64_emit`'s traces against
    //! Unicorn; see `CROSS_FUZZ_PLAN.md`. Gated behind `fuzz-traces` so it
    //! never leaks into the `wasm32` cdylib.
    pub use crate::template::aarch64::a64_emit;
}

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
        assert!(matches!(name, 'a' | 'b' | 'r' | 'f' | 'g'));
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

/// Canonical name→index table for every [`state`] slot, in the order
/// `x0..x30`, `sp`, `scratch:{a,b,r,f,g}`, `nzcv:{n,z,c,v}`, `v0..v31` (31 +
/// 1 + 5 + 4 + 32 = 73 entries). This is the single source of truth for both
/// the WASM embedder's `WasmAbiConfig::state_slots` and any test asserting
/// against a specific slot's index.
///
/// AArch64 has no `LoadReg`/`StoreReg`-indexed registers at all — every one
/// of its 73 GPR/flag/vector slots lives here instead, so the canonical
/// `WasmAbiConfig` for this ISA uses `reg_base = 0, num_regs = 73` (this
/// table's indices span the full register/param/result range, not extra
/// locals beyond it — see `WasmAbiConfig::state_slots`'s doc comment).
pub const AARCH64_STATE_SLOTS: [(&str, u32); 73] = [
    ("a64:x0", 0),
    ("a64:x1", 1),
    ("a64:x2", 2),
    ("a64:x3", 3),
    ("a64:x4", 4),
    ("a64:x5", 5),
    ("a64:x6", 6),
    ("a64:x7", 7),
    ("a64:x8", 8),
    ("a64:x9", 9),
    ("a64:x10", 10),
    ("a64:x11", 11),
    ("a64:x12", 12),
    ("a64:x13", 13),
    ("a64:x14", 14),
    ("a64:x15", 15),
    ("a64:x16", 16),
    ("a64:x17", 17),
    ("a64:x18", 18),
    ("a64:x19", 19),
    ("a64:x20", 20),
    ("a64:x21", 21),
    ("a64:x22", 22),
    ("a64:x23", 23),
    ("a64:x24", 24),
    ("a64:x25", 25),
    ("a64:x26", 26),
    ("a64:x27", 27),
    ("a64:x28", 28),
    ("a64:x29", 29),
    ("a64:x30", 30),
    ("a64:sp", 31),
    ("a64:scratch:a", 32),
    ("a64:scratch:b", 33),
    ("a64:scratch:r", 34),
    ("a64:scratch:f", 35),
    ("a64:scratch:g", 36),
    ("a64:nzcv:n", 37),
    ("a64:nzcv:z", 38),
    ("a64:nzcv:c", 39),
    ("a64:nzcv:v", 40),
    ("a64:v0", 41),
    ("a64:v1", 42),
    ("a64:v2", 43),
    ("a64:v3", 44),
    ("a64:v4", 45),
    ("a64:v5", 46),
    ("a64:v6", 47),
    ("a64:v7", 48),
    ("a64:v8", 49),
    ("a64:v9", 50),
    ("a64:v10", 51),
    ("a64:v11", 52),
    ("a64:v12", 53),
    ("a64:v13", 54),
    ("a64:v14", 55),
    ("a64:v15", 56),
    ("a64:v16", 57),
    ("a64:v17", 58),
    ("a64:v18", 59),
    ("a64:v19", 60),
    ("a64:v20", 61),
    ("a64:v21", 62),
    ("a64:v22", 63),
    ("a64:v23", 64),
    ("a64:v24", 65),
    ("a64:v25", 66),
    ("a64:v26", 67),
    ("a64:v27", 68),
    ("a64:v28", 69),
    ("a64:v29", 70),
    ("a64:v30", 71),
    ("a64:v31", 72),
];

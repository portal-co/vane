//! `vane-cross-fuzz` — differential test harness executing Vane's
//! architecture frontends (RV64, AArch64) against Unicorn 2.x on random
//! instruction streams.
//!
//! **Host-only:** links the native Unicorn C library; never compiled for
//! `wasm32`. See `CROSS_FUZZ_PLAN.md` for the full design.
//!
//! # Layout
//!
//! - [`skip`] — which Vane traps mean "skip" vs "fail" (single source of truth).
//! - [`state`] — `StackHost` over `Mem` + a snapshot-able register image.
//! - [`compare`] — field-by-field state diffing and failure reports.
//! - [`runner`] — the step-differential engine owning Unicorn + `Mem`.
//! - [`rv64`] / [`aarch64`] — per-frontend glue (trace emission + profiles).
//!
//! Feature-gated: `rv64` enables the RISC-V target, `aarch64` the AArch64
//! target (both pull the matching frontend crate's `fuzz-traces` feature).

#![no_std]
extern crate alloc;

pub mod compare;
pub mod runner;
pub mod skip;
pub mod state;

// `interp` is re-exported through vane-arch's public API; alias the path the
// runner uses so the reference compiles regardless of frontend features.
pub use vane_arch::interp;

#[cfg(feature = "rv64")]
pub mod rv64;

#[cfg(feature = "aarch64")]
pub mod aarch64;

/// Scoped single-instruction runner helpers shared by the smoke and
/// proptest targets (M1 layout: stop-pad composition + control-flow
/// filter that keeps `interpret` terminating on random inputs).
#[cfg(any(feature = "rv64", feature = "aarch64"))]
pub mod probe {
    use alloc::vec::Vec;
    use crate::runner::{CODE_BASE, Step};
    use crate::skip::Skip;
    #[cfg(feature = "rv64")]
    use crate::rv64::{Rv64Runner, step_rv64};
    

    #[cfg(feature = "rv64")]
    pub fn run(word: u32, seed: u64) -> Step {
        run_scoped(word, seed)
    }

    #[cfg(feature = "rv64")]
    pub fn run_scoped(word: u32, seed: u64) -> Step {
        let mut regs = [0u64; 32];
        for (i, r) in regs.iter_mut().enumerate().skip(1) {
            *r = seed
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(i as u64)
                .rotate_left((i as u32) % 64);
        }
        let data: [u8; 64] = core::array::from_fn(|i| (seed >> (i % 8)) as u8);
        run_with(word, &regs, &data)
    }

    #[cfg(feature = "rv64")]
    pub fn run_with(word: u32, regs: &[u64; 32], data: &[u8]) -> Step {
        // Backward branch/jump targets recurse `rv_emit` into an
        // already-open loop label, which `interpret` cycles in forever.
        // Covers both 4-byte and compressed (c.beqz/c.bnez/c.j) forms
        // because `Inst::decode` dispatches on the low bits.
        let decode_word = if (word & 0b11) != 0b11 {
            word & 0xffff // compressed: decode the low half only
        } else {
            word
        };
        if let Ok((inst, _)) = rv_asm::Inst::decode(decode_word, rv_asm::Xlen::Rv64) {
            use rv_asm::Inst;
            let backward = match inst {
                Inst::Beq { offset, .. }
                | Inst::Bne { offset, .. }
                | Inst::Blt { offset, .. }
                | Inst::Bge { offset, .. }
                | Inst::Bltu { offset, .. }
                | Inst::Bgeu { offset, .. }
                | Inst::Jal { offset, .. } => offset.as_i64() <= 0,
                _ => false,
            };
            if backward {
                return Step::Skipped(Skip::Unsupported);
            }
        }
        let compressed = (word & 0b11) != 0b11;
        let mut vane_code = Vec::new();
        let mut unicorn_code = Vec::new();
        if compressed {
            vane_code.extend_from_slice(&word.to_le_bytes()[..2]);
            vane_code.extend_from_slice(&0x9002u16.to_le_bytes());
            unicorn_code.extend_from_slice(&word.to_le_bytes()[..2]);
        } else {
            vane_code.extend_from_slice(&word.to_le_bytes());
            vane_code.extend_from_slice(&0x0010_0073u32.to_le_bytes());
            unicorn_code.extend_from_slice(&word.to_le_bytes());
        }
        let mut runner = Rv64Runner::new();
        runner.setup_raw(&vane_code, &unicorn_code, regs, data);
        step_rv64(&mut runner, CODE_BASE)
    }

    /// AArch64 variant of [`run_scoped`]: no compressed encodings, so the
    /// stop-pad is a plain BRK #0 in the next word slot; the only
    /// termination hazard (backward branches) is filtered by the caller.
    pub fn run_a64(word: u32, seed: u64) -> Step {
        let mut regs = [0u64; 32];
        for (i, r) in regs.iter_mut().enumerate().skip(1) {
            *r = seed
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(i as u64)
                .rotate_left((i as u32) % 64);
        }
        let data: [u8; 64] = core::array::from_fn(|i| (seed >> (i % 8)) as u8);
        run_a64_with(word, &regs, &data, crate::runner::CODE_BASE)
    }

    pub fn run_a64_with(word: u32, regs: &[u64; 32], data: &[u8], pc: u64) -> Step {
        #[cfg(feature = "aarch64")]
        {
            let mut vane_code = Vec::new();
            vane_code.extend_from_slice(&word.to_le_bytes());
            vane_code.extend_from_slice(&crate::runner::STOP_PAD_A64.to_le_bytes());
            let mut runner = crate::aarch64::A64Runner::new();
            runner.setup_raw(&vane_code, &word.to_le_bytes(), regs, data);
            crate::aarch64::step_aarch64(&mut runner, pc)
        }
        #[cfg(not(feature = "aarch64"))]
        {
            let _ = (word, regs, data, pc);
            unimplemented!("enable the aarch64 feature")
        }
    }
}

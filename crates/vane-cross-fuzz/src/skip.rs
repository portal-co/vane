//! Skip / trap classification for the Vane-vs-Unicorn differential harness.
//!
//! Random binaries constantly hit instructions Vane does not implement, and
//! instructions that trap *by design* (invalid encodings, `ebreak`, `brk`).
//! Every such case must skip silently rather than fail, while a genuine
//! semantic mismatch must fail loudly. This module is the single source of
//! truth for which Vane `Exit::Trap` messages mean "skip" (CROSS_FUZZ_PLAN.md
//! §5).
//!
//! Classification is prefix-based on the trap messages produced by
//! `rv_body` (`vane-riscv/src/template/riscv.rs`) and `a64_body`
//! (`vane-aarch64/src/template/aarch64.rs`). Each prefix has a unit test
//! cross-referencing the emitting match arm, so a message-format change
//! breaks these tests visibly instead of silently over-skipping.
//!
//! Anything not matching a known skip prefix fails closed (row 6 of the
//! plan's table): a new trap category must be registered here explicitly.

use alloc::string::{String, ToString};

/// Why a differential step could not be compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Vane could not decode the instruction word at all (row 1). For RV64
    /// the word is outside `rv-asm`'s grammar; for AArch64 `disarm64`
    /// returned `None`. Decode disagreements between Vane's decoder and
    /// Unicorn are upstream decoder issues, not Vane frontend bugs.
    Undecodable,
    /// Vane decoded the instruction but has no lowering for it (row 2), or
    /// traps on a documented FP limitation (row 4).
    Unsupported,
    /// The guest explicitly trapped (`ebreak` / `brk`) — both engines agree
    /// this is not a normal architectural exit (row 3).
    GuestBreak,
    /// A syscall boundary: semantics are a host concern, not architectural
    /// (row 5).
    Ecall,
}

/// Outcome of classifying a Vane `Exit` for one differential step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Vane executed the step architecturally; compare state against
    /// Unicorn (rows 7–8).
    Compare,
    /// Skip this input; `reason` says why (rows 1–5).
    Skip(Skip),
    /// An unknown trap kind: fail closed (row 6). New trap messages must be
    /// classified here explicitly before they can ever be skipped.
    UnknownTrap(String),
}

/// Classify the `Exit::Trap` message produced by a Vane frontend.
///
/// The prefixes below mirror the exact `StackOp::Trap(alloc::format!(...))`
/// call sites:
///
/// | Prefix | Emitting site | Plan row |
/// |---|---|---|
/// | `decoding: ` | `rv_body`, `Inst::decode` failure | 1 |
/// | `op:` | `rv_body`'s catch-all arm (`ebreak` + unimplemented RV ops) | 2 |
/// | `aarch64: undefined ` | `a64_body`, `disarm64::decoder::decode` `None` | 1 |
/// | `aarch64: unsupported ` | `a64_body`'s `unsupported()` helper | 2 |
/// | `aarch64: brk #` | `a64_body`, `EXCEPTION::BRK_EXCEPTION` | 3 |
pub fn classify_trap(msg: &str) -> Verdict {
    if let Some(rest) = msg.strip_prefix("op:") {
        // `rv_body`'s catch-all emits the `rv_asm::Inst`'s Display form.
        // `ebreak` is the only *deliberate* guest trap in that arm; every
        // other message is an unimplemented-but-valid instruction (row 2).
        if rest == "ebreak" {
            return Verdict::Skip(Skip::GuestBreak);
        }
        return Verdict::Skip(Skip::Unsupported);
    }
    if msg.starts_with("decoding: ") {
        return Verdict::Skip(Skip::Undecodable);
    }
    if msg.starts_with("aarch64: undefined ") {
        return Verdict::Skip(Skip::Undecodable);
    }
    if msg.starts_with("aarch64: unsupported ") {
        return Verdict::Skip(Skip::Unsupported);
    }
    if msg.starts_with("aarch64: brk #") {
        return Verdict::Skip(Skip::GuestBreak);
    }
    Verdict::UnknownTrap(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test cross-references the emitting match arm so format drift in
    // `rv_body`/`a64_body` breaks the fuzz build loudly (plan §5).

    /// `rv_body`: `ops.push(StackOp::Trap(alloc::format!("decoding: {e}")))`
    #[test]
    fn rv_decode_failure_skips() {
        assert_eq!(
            classify_trap("decoding: failed to decode instruction '0' because of field 'opcode'"),
            Verdict::Skip(Skip::Undecodable)
        );
    }

    /// `rv_body`'s catch-all: `ops.push(StackOp::Trap(alloc::format!("op:{inst}")))`
    #[test]
    fn rv_unimplemented_op_skips_and_ebreak_is_guest_break() {
        // An unimplemented-but-valid instruction (e.g. a CSR op).
        assert_eq!(
            classify_trap("op:csrrw rd=1, rs1=2, csr=0x300"),
            Verdict::Skip(Skip::Unsupported)
        );
        // `ebreak` is a deliberate guest trap (row 3).
        assert_eq!(classify_trap("op:ebreak"), Verdict::Skip(Skip::GuestBreak));
    }

    /// `a64_body`: `disarm64` returning `None`
    #[test]
    fn aarch64_undefined_skips() {
        assert_eq!(
            classify_trap("aarch64: undefined ffffffff"),
            Verdict::Skip(Skip::Undecodable)
        );
    }

    /// `a64_body`'s `unsupported()` helper
    #[test]
    fn aarch64_unsupported_family_skips() {
        assert_eq!(
            classify_trap("aarch64: unsupported ADDSUB_IMM 31420c00"),
            Verdict::Skip(Skip::Unsupported)
        );
    }

    /// `a64_body`: `EXCEPTION::BRK_EXCEPTION`
    #[test]
    fn aarch64_brk_is_guest_break() {
        assert_eq!(
            classify_trap("aarch64: brk #1234"),
            Verdict::Skip(Skip::GuestBreak)
        );
    }

    /// Row 6: unknown trap messages fail closed.
    #[test]
    fn unknown_trap_fails_closed() {
        assert_eq!(
            classify_trap("something brand new"),
            Verdict::UnknownTrap("something brand new".to_string())
        );
        // A format drift in an existing message must NOT be silently
        // absorbed by a prefix that happens to still match: e.g. if
        // `aarch64: unsupported` were renamed, the old prefix stops
        // matching and we fail closed rather than over-skip.
        assert_eq!(
            classify_trap("aarch64: unimplemented ADDSUB_IMM 31420c00"),
            Verdict::UnknownTrap("aarch64: unimplemented ADDSUB_IMM 31420c00".to_string())
        );
    }
}

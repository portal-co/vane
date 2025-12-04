//! HINT instruction detection and handling for rv-corpus test support.
//!
//! In RISC-V, HINT instructions are valid instructions that write to x0 (the zero register).
//! Since writes to x0 are discarded, these instructions essentially act as NOPs but can carry
//! metadata. The rv-corpus test suite uses `addi x0, x0, N` as test case markers where N
//! identifies the test case number.
//!
//! This module provides utilities to detect HINT instructions and extract their values,
//! gated behind a runtime `test_mode` flag.

use rv_asm::Inst;

/// Represents a detected HINT instruction with its associated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    /// The hint value extracted from the instruction (e.g., the immediate in `addi x0, x0, N`).
    pub value: i64,
    /// The type of hint instruction that was detected.
    pub kind: HintKind,
}

/// The different kinds of HINT instructions recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    /// `addi x0, rs1, imm` - Used by rv-corpus for test case markers.
    Addi,
    /// `andi x0, rs1, imm`
    Andi,
    /// `ori x0, rs1, imm`
    Ori,
    /// `xori x0, rs1, imm`
    Xori,
    /// `slli x0, rs1, imm`
    Slli,
    /// `srli x0, rs1, imm`
    Srli,
    /// `srai x0, rs1, imm`
    Srai,
    /// `lui x0, uimm`
    Lui,
    /// `add x0, rs1, rs2` and similar register-register ops
    RegReg,
}

/// Check if an instruction is a HINT (writes to x0) and extract its value if so.
///
/// Returns `Some(Hint)` if the instruction writes to x0, `None` otherwise.
///
/// # Examples
///
/// ```ignore
/// use rv_asm::{Inst, Reg, Imm12};
/// use vane_jit::hint::detect_hint;
///
/// let inst = Inst::Addi { dest: Reg(0), src1: Reg(0), imm: Imm12::new(100) };
/// if let Some(hint) = detect_hint(&inst) {
///     assert_eq!(hint.value, 100);
/// }
/// ```
pub fn detect_hint(inst: &Inst) -> Option<Hint> {
    match inst {
        // Immediate instructions with x0 as destination
        Inst::Addi { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Addi,
        }),
        Inst::Andi { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Andi,
        }),
        Inst::Ori { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Ori,
        }),
        Inst::Xori { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Xori,
        }),
        Inst::Slli { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Slli,
        }),
        Inst::Srli { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Srli,
        }),
        Inst::Srai { dest, imm, .. } if dest.0 == 0 => Some(Hint {
            value: imm.as_i64(),
            kind: HintKind::Srai,
        }),
        Inst::Lui { dest, uimm, .. } if dest.0 == 0 => Some(Hint {
            value: uimm.as_i64(),
            kind: HintKind::Lui,
        }),
        // Register-register instructions with x0 as destination
        Inst::Add { dest, .. }
        | Inst::Sub { dest, .. }
        | Inst::And { dest, .. }
        | Inst::Or { dest, .. }
        | Inst::Xor { dest, .. }
        | Inst::Sll { dest, .. }
        | Inst::Srl { dest, .. }
        | Inst::Sra { dest, .. }
        | Inst::Slt { dest, .. }
        | Inst::Sltu { dest, .. }
            if dest.0 == 0 =>
        {
            Some(Hint {
                value: 0, // No immediate value for register-register ops
                kind: HintKind::RegReg,
            })
        }
        _ => None,
    }
}

/// Check if an instruction is specifically an rv-corpus test marker.
///
/// The rv-corpus uses `addi x0, x0, N` where both dest and src are x0 to mark test cases.
/// This is a stricter check than `detect_hint` as it requires src1 to also be x0.
///
/// Returns `Some(test_case_number)` if this is a test marker, `None` otherwise.
pub fn detect_test_marker(inst: &Inst) -> Option<i64> {
    match inst {
        Inst::Addi { dest, src1, imm } if dest.0 == 0 && src1.0 == 0 && imm.as_i64() != 0 => {
            Some(imm.as_i64())
        }
        _ => None,
    }
}

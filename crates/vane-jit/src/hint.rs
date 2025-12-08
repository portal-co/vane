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

#[cfg(test)]
mod tests {
    use super::*;
    use rv_asm::{Imm, Reg};

    #[test]
    fn test_detect_hint_addi() {
        let inst = Inst::Addi {
            dest: Reg(0),
            src1: Reg(1),
            imm: Imm::new_i32(42),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        let hint = hint.unwrap();
        assert_eq!(hint.value, 42);
        assert_eq!(hint.kind, HintKind::Addi);
    }

    #[test]
    fn test_detect_hint_not_x0() {
        let inst = Inst::Addi {
            dest: Reg(1),
            src1: Reg(2),
            imm: Imm::new_i32(10),
        };
        assert!(detect_hint(&inst).is_none());
    }

    #[test]
    fn test_detect_hint_andi() {
        let inst = Inst::Andi {
            dest: Reg(0),
            src1: Reg(5),
            imm: Imm::new_u32(255),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::Andi);
    }

    #[test]
    fn test_detect_hint_ori() {
        let inst = Inst::Ori {
            dest: Reg(0),
            src1: Reg(3),
            imm: Imm::new_u32(16),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::Ori);
    }

    #[test]
    fn test_detect_hint_xori() {
        let inst = Inst::Xori {
            dest: Reg(0),
            src1: Reg(4),
            imm: Imm::new_i32(-1),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::Xori);
    }

    #[test]
    fn test_detect_hint_lui() {
        let inst = Inst::Lui {
            dest: Reg(0),
            uimm: Imm::new_u32(0x12345),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::Lui);
    }

    #[test]
    fn test_detect_hint_add() {
        let inst = Inst::Add {
            dest: Reg(0),
            src1: Reg(2),
            src2: Reg(3),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::RegReg);
        assert_eq!(hint.unwrap().value, 0);
    }

    #[test]
    fn test_detect_hint_sub() {
        let inst = Inst::Sub {
            dest: Reg(0),
            src1: Reg(1),
            src2: Reg(2),
        };
        let hint = detect_hint(&inst);
        assert!(hint.is_some());
        assert_eq!(hint.unwrap().kind, HintKind::RegReg);
    }

    #[test]
    fn test_detect_test_marker_valid() {
        let inst = Inst::Addi {
            dest: Reg(0),
            src1: Reg(0),
            imm: Imm::new_i32(100),
        };
        let marker = detect_test_marker(&inst);
        assert!(marker.is_some());
        assert_eq!(marker.unwrap(), 100);
    }

    #[test]
    fn test_detect_test_marker_zero_imm() {
        let inst = Inst::Addi {
            dest: Reg(0),
            src1: Reg(0),
            imm: Imm::ZERO,
        };
        assert!(detect_test_marker(&inst).is_none());
    }

    #[test]
    fn test_detect_test_marker_wrong_src() {
        let inst = Inst::Addi {
            dest: Reg(0),
            src1: Reg(1),
            imm: Imm::new_i32(50),
        };
        assert!(detect_test_marker(&inst).is_none());
    }

    #[test]
    fn test_detect_test_marker_wrong_dest() {
        let inst = Inst::Addi {
            dest: Reg(1),
            src1: Reg(0),
            imm: Imm::new_i32(50),
        };
        assert!(detect_test_marker(&inst).is_none());
    }

    #[test]
    fn test_detect_test_marker_negative() {
        let inst = Inst::Addi {
            dest: Reg(0),
            src1: Reg(0),
            imm: Imm::new_i32(-5),
        };
        let marker = detect_test_marker(&inst);
        assert!(marker.is_some());
        assert_eq!(marker.unwrap(), -5);
    }

    #[test]
    fn test_detect_hint_non_hint_instruction() {
        let inst = Inst::Add {
            dest: Reg(5),
            src1: Reg(1),
            src2: Reg(2),
        };
        assert!(detect_hint(&inst).is_none());
    }

    #[test]
    fn test_hint_eq() {
        let hint1 = Hint {
            value: 42,
            kind: HintKind::Addi,
        };
        let hint2 = Hint {
            value: 42,
            kind: HintKind::Addi,
        };
        let hint3 = Hint {
            value: 43,
            kind: HintKind::Addi,
        };
        
        assert_eq!(hint1, hint2);
        assert_ne!(hint1, hint3);
    }
}

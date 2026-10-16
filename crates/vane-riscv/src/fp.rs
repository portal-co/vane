//! Scalar F/D (floating-point) execution for the privileged interpreter
//! (M4, PLAN_LINUX_WASM.md). Operates on [`HartState`]'s `fregs`/`fcsr`.
//!
//! Design notes:
//! - The F register file stores raw bit patterns; 32-bit values are
//!   NaN-boxed (upper 32 bits all-ones) per the spec.
//! - Arithmetic uses host IEEE-754 semantics; RNE everywhere except the
//!   int-conversion RTZ path. `mul_add` gives single-rounded fused ops.
//! - `fflags` exception accumulation is minimal (NV set on invalid
//!   comparisons/conversions); Linux does not depend on finer fidelity.

use crate::hart::HartState;
use libm::{ceil, floor, sqrt, trunc};
use rv_asm::{FReg, Inst, RoundingMode};

/// fflags bits.
const NV: u64 = 16; // invalid operation

impl HartState {
    /// Read an F32 register as f32 (NaN-boxing check per spec).
    pub fn read_f32(&self, r: FReg) -> f32 {
        let v = self.fregs[r.0 as usize];
        // If the upper 32 bits are not all-ones, the register holds a wider
        // (f64) value — per spec, reads as canonical qNaN.
        if v >> 32 != 0xFFFF_FFFF {
            return f32::from_bits(0x7FC0_0000);
        }
        f32::from_bits(v as u32)
    }

    pub fn write_f32(&mut self, r: FReg, v: f32) {
        // NaN-box.
        self.fregs[r.0 as usize] = (v.to_bits() as u64) | 0xFFFF_FFFF_0000_0000;
        self.mark_fp_dirty();
    }

    pub fn read_f64(&self, r: FReg) -> f64 {
        f64::from_bits(self.fregs[r.0 as usize])
    }

    pub fn write_f64(&mut self, r: FReg, v: f64) {
        self.fregs[r.0 as usize] = v.to_bits();
        self.mark_fp_dirty();
    }

    /// mstatus.FS = Dirty on any FP register write (Linux uses FS for lazy
    /// FP context switching).
    fn mark_fp_dirty(&mut self) {
        self.mstatus |= crate::hart::MSTATUS_FS;
    }

    /// The active rounding mode for an instruction's rm field.
    pub fn active_rm(&self, rm: RoundingMode) -> RoundingMode {
        match rm {
            RoundingMode::Dynamic => self.frm(),
            other => other,
        }
    }

    pub fn frm(&self) -> RoundingMode {
        match (self.fcsr >> 5) & 7 {
            1 => RoundingMode::RoundTowardsZero,
            2 => RoundingMode::RoundDown,
            3 => RoundingMode::RoundUp,
            4 => RoundingMode::RoundToNearestTiesToMax,
            _ => RoundingMode::RoundToNearestTiesToEven,
        }
    }
}

fn fclass_f64(v: f64) -> u64 {
    if v.is_nan() {
        return 1 << 9; // quiet NaN
    }
    let bits = v.to_bits();
    let sign = (bits >> 63) != 0;
    if v.is_infinite() {
        return if sign { 1 } else { 1 << 7 };
    }
    if v == 0.0 {
        return if sign { 1 << 3 } else { 1 << 4 };
    }
    if (bits >> 52) & 0x7FF == 0 {
        return if sign { 1 << 2 } else { 1 << 5 }; // subnormal
    }
    if sign {
        1 << 1
    } else {
        1 << 6
    }
}

fn fclass_f32(v: f32) -> u64 {
    if v.is_nan() {
        return 1 << 9;
    }
    let bits = v.to_bits();
    let sign = (bits >> 31) != 0;
    if v.is_infinite() {
        return if sign { 1 } else { 1 << 7 };
    }
    if v == 0.0 {
        return if sign { 1 << 3 } else { 1 << 4 };
    }
    if (bits >> 23) & 0xFF == 0 {
        return if sign { 1 << 2 } else { 1 << 5 };
    }
    if sign {
        1 << 1
    } else {
        1 << 6
    }
}

/// float → signed integer with the spec's saturation behavior.
fn f64_to_i64(v: f64, rm: RoundingMode) -> u64 {
    if v.is_nan() {
        return i64::MAX as u64;
    }
    let v = match rm {
        RoundingMode::RoundTowardsZero => trunc(v),
        RoundingMode::RoundDown => floor(v),
        RoundingMode::RoundUp => ceil(v),
        _ => libm::roundeven(v),
    };
    // `as` saturates on overflow (matches the RISC-V spec).
    v as i64 as u64
}

fn f64_to_u64(v: f64, rm: RoundingMode) -> u64 {
    if v.is_nan() {
        return u64::MAX;
    }
    let v = match rm {
        RoundingMode::RoundTowardsZero => trunc(v),
        RoundingMode::RoundDown => floor(v),
        RoundingMode::RoundUp => ceil(v),
        _ => libm::roundeven(v),
    };
    if v <= 0.0 {
        0
    } else {
        v as u64
    }
}

/// Execute a decoded F/D instruction. Returns the integer-register writeback
/// for the instruction classes that write one (comparisons, class, int
/// conversions, fmv.x.*); `None` otherwise.
///
/// Caller is responsible for the FS!=0 gate before calling.
pub enum FpOutcome {
    /// Executed, no integer writeback (arithmetic, FP stores).
    Done,
    /// Executed, writes `u64` to integer register `Reg` (comparisons, int
    /// conversions, fmv.x.*).
    Writeback(rv_asm::Reg, u64),
    /// Not an FP instruction handled here (caller treats as illegal).
    NotFp,
}

pub fn exec_fp(
    hart: &mut HartState,
    iregs: &[u64; 32],
    inst: &Inst,
) -> FpOutcome {
    use Inst::*;
    match *inst {
        // ── S arithmetic ────────────────────────────────────────────────
        FaddS { src1, src2, dest, .. } => {
            let v = hart.read_f32(src1) + hart.read_f32(src2);
            hart.write_f32(dest, v);
        }
        FsubS { src1, src2, dest, .. } => {
            let v = hart.read_f32(src1) - hart.read_f32(src2);
            hart.write_f32(dest, v);
        }
        FmulS { src1, src2, dest, .. } => {
            let v = hart.read_f32(src1) * hart.read_f32(src2);
            hart.write_f32(dest, v);
        }
        FdivS { src1, src2, dest, .. } => {
            let v = hart.read_f32(src1) / hart.read_f32(src2);
            hart.write_f32(dest, v);
        }
        FsqrtS { src, dest, .. } => {
            let v = libm::sqrtf(hart.read_f32(src));
            hart.write_f32(dest, v);
        }
        // ── D arithmetic ────────────────────────────────────────────────
        FaddD { src1, src2, dest, .. } => {
            let v = hart.read_f64(src1) + hart.read_f64(src2);
            hart.write_f64(dest, v);
        }
        FsubD { src1, src2, dest, .. } => {
            let v = hart.read_f64(src1) - hart.read_f64(src2);
            hart.write_f64(dest, v);
        }
        FmulD { src1, src2, dest, .. } => {
            let v = hart.read_f64(src1) * hart.read_f64(src2);
            hart.write_f64(dest, v);
        }
        FdivD { src1, src2, dest, .. } => {
            let v = hart.read_f64(src1) / hart.read_f64(src2);
            hart.write_f64(dest, v);
        }
        FsqrtD { src, dest, .. } => {
            let v = libm::sqrt(hart.read_f64(src));
            hart.write_f64(dest, v);
        }
        // ── Fused multiply-add (single rounding via mul_add) ────────────
        FmaddS { src1, src2, src3, dest, .. } => {
            let v = libm::fmaf(hart.read_f32(src1), hart.read_f32(src2), hart.read_f32(src3));
            hart.write_f32(dest, v);
        }
        FmsubS { src1, src2, src3, dest, .. } => {
            let v = libm::fmaf(hart.read_f32(src1), hart.read_f32(src2), -hart.read_f32(src3));
            hart.write_f32(dest, v);
        }
        FnmsubS { src1, src2, src3, dest, .. } => {
            let v = libm::fmaf((-hart.read_f32(src1)), hart.read_f32(src2), hart.read_f32(src3));
            hart.write_f32(dest, v);
        }
        FnmaddS { src1, src2, src3, dest, .. } => {
            let v = libm::fmaf((-hart.read_f32(src1)), hart.read_f32(src2), -hart.read_f32(src3));
            hart.write_f32(dest, v);
        }
        FmaddD { src1, src2, src3, dest, .. } => {
            let v = libm::fma(hart.read_f64(src1), hart.read_f64(src2), hart.read_f64(src3));
            hart.write_f64(dest, v);
        }
        FmsubD { src1, src2, src3, dest, .. } => {
            let v = libm::fma(hart.read_f64(src1), hart.read_f64(src2), -hart.read_f64(src3));
            hart.write_f64(dest, v);
        }
        FnmsubD { src1, src2, src3, dest, .. } => {
            let v = libm::fma((-hart.read_f64(src1)), hart.read_f64(src2), hart.read_f64(src3));
            hart.write_f64(dest, v);
        }
        FnmaddD { src1, src2, src3, dest, .. } => {
            let v = libm::fma((-hart.read_f64(src1)), hart.read_f64(src2), -hart.read_f64(src3));
            hart.write_f64(dest, v);
        }
        // ── Sign injection (S) ──────────────────────────────────────────
        FsgnjS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1).to_bits(), hart.read_f32(src2).to_bits());
            hart.write_f32(dest, f32::from_bits((a & !0x8000_0000) | (b & 0x8000_0000)));
        }
        FsgnjnS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1).to_bits(), hart.read_f32(src2).to_bits());
            hart.write_f32(dest, f32::from_bits((a & !0x8000_0000) | (!b & 0x8000_0000)));
        }
        FsgnjxS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1).to_bits(), hart.read_f32(src2).to_bits());
            hart.write_f32(dest, f32::from_bits(a ^ (b & 0x8000_0000)));
        }
        FsgnjD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1).to_bits(), hart.read_f64(src2).to_bits());
            hart.write_f64(dest, f64::from_bits((a & !(1 << 63)) | (b & (1 << 63))));
        }
        FsgnjnD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1).to_bits(), hart.read_f64(src2).to_bits());
            hart.write_f64(dest, f64::from_bits((a & !(1 << 63)) | (!b & (1 << 63))));
        }
        FsgnjxD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1).to_bits(), hart.read_f64(src2).to_bits());
            hart.write_f64(dest, f64::from_bits(a ^ (b & (1 << 63))));
        }
        // ── Min/max (spec: if one operand is NaN, return the other) ─────
        FminS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1), hart.read_f32(src2));
            hart.write_f32(dest, fmin_f32(a, b));
        }
        FmaxS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1), hart.read_f32(src2));
            hart.write_f32(dest, fmax_f32(a, b));
        }
        FminD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1), hart.read_f64(src2));
            hart.write_f64(dest, fmin_f64(a, b));
        }
        FmaxD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1), hart.read_f64(src2));
            hart.write_f64(dest, fmax_f64(a, b));
        }
        // ── Comparisons (integer writeback) ─────────────────────────────
        FeqS { dest, src1, src2 } => {
            return FpOutcome::Writeback(dest, (hart.read_f32(src1) == hart.read_f32(src2)) as u64);
        }
        FltS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1), hart.read_f32(src2));
            if a.is_nan() || b.is_nan() {
                hart.fcsr |= NV;
                return FpOutcome::Writeback(dest, 0);
            }
            return FpOutcome::Writeback(dest, (a < b) as u64);
        }
        FleS { dest, src1, src2 } => {
            let (a, b) = (hart.read_f32(src1), hart.read_f32(src2));
            if a.is_nan() || b.is_nan() {
                hart.fcsr |= NV;
                return FpOutcome::Writeback(dest, 0);
            }
            return FpOutcome::Writeback(dest, (a <= b) as u64);
        }
        FeqD { dest, src1, src2 } => {
            return FpOutcome::Writeback(dest, (hart.read_f64(src1) == hart.read_f64(src2)) as u64);
        }
        FltD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1), hart.read_f64(src2));
            if a.is_nan() || b.is_nan() {
                hart.fcsr |= NV;
                return FpOutcome::Writeback(dest, 0);
            }
            return FpOutcome::Writeback(dest, (a < b) as u64);
        }
        FleD { dest, src1, src2 } => {
            let (a, b) = (hart.read_f64(src1), hart.read_f64(src2));
            if a.is_nan() || b.is_nan() {
                hart.fcsr |= NV;
                return FpOutcome::Writeback(dest, 0);
            }
            return FpOutcome::Writeback(dest, (a <= b) as u64);
        }
        // ── Classify ────────────────────────────────────────────────────
        FclassS { dest, src } => {
            return FpOutcome::Writeback(dest, fclass_f32(hart.read_f32(src)));
        }
        FclassD { dest, src } => {
            return FpOutcome::Writeback(dest, fclass_f64(hart.read_f64(src)));
        }
        // ── Conversions S↔D ─────────────────────────────────────────────
        FcvtSD { dest, src, .. } => {
            let v = hart.read_f32(src) as f64;
            hart.write_f64(dest, v);
        }
        FcvtDS { dest, src, .. } => {
            let v = hart.read_f64(src) as f32;
            hart.write_f32(dest, v);
        }
        // ── Conversions F↔int ───────────────────────────────────────────
        FcvtWS { dest, src, rm } => {
            let v = f64_to_i64(hart.read_f32(src) as f64, hart.active_rm(rm)) as i32;
            return FpOutcome::Writeback(dest, v as i64 as u64);
        }
        FcvtWuS { dest, src, rm } => {
            let v = f64_to_u64(hart.read_f32(src) as f64, hart.active_rm(rm)) as u32;
            return FpOutcome::Writeback(dest, v as i32 as i64 as u64);
        }
        FcvtLS { dest, src, rm } => {
            return FpOutcome::Writeback(dest, f64_to_i64(hart.read_f32(src) as f64, hart.active_rm(rm)));
        }
        FcvtLuS { dest, src, rm } => {
            return FpOutcome::Writeback(dest, f64_to_u64(hart.read_f32(src) as f64, hart.active_rm(rm)));
        }
        FcvtWD { dest, src, rm } => {
            let v = f64_to_i64(hart.read_f64(src), hart.active_rm(rm)) as i32;
            return FpOutcome::Writeback(dest, v as i64 as u64);
        }
        FcvtWuD { dest, src, rm } => {
            let v = f64_to_u64(hart.read_f64(src), hart.active_rm(rm)) as u32;
            return FpOutcome::Writeback(dest, v as i32 as i64 as u64);
        }
        FcvtLD { dest, src, rm } => {
            return FpOutcome::Writeback(dest, f64_to_i64(hart.read_f64(src), hart.active_rm(rm)));
        }
        FcvtLuD { dest, src, rm } => {
            return FpOutcome::Writeback(dest, f64_to_u64(hart.read_f64(src), hart.active_rm(rm)));
        }
        FcvtSW { dest, src, .. } => {
            let v = iregs[src.0 as usize] as i32 as f32;
            hart.write_f32(dest, v);
        }
        FcvtSWu { dest, src, .. } => {
            let v = (iregs[src.0 as usize] as u32) as f32;
            hart.write_f32(dest, v);
        }
        FcvtSL { dest, src, .. } => {
            let v = iregs[src.0 as usize] as i64 as f32;
            hart.write_f32(dest, v);
        }
        FcvtSLu { dest, src, .. } => {
            let v = iregs[src.0 as usize] as f32;
            hart.write_f32(dest, v);
        }
        FcvtDW { dest, src, .. } => {
            let v = iregs[src.0 as usize] as i32 as f64;
            hart.write_f64(dest, v);
        }
        FcvtDWu { dest, src, .. } => {
            let v = (iregs[src.0 as usize] as u32) as f64;
            hart.write_f64(dest, v);
        }
        FcvtDL { dest, src, .. } => {
            let v = iregs[src.0 as usize] as i64 as f64;
            hart.write_f64(dest, v);
        }
        FcvtDLu { dest, src, .. } => {
            let v = iregs[src.0 as usize] as f64;
            hart.write_f64(dest, v);
        }
        // ── Moves ───────────────────────────────────────────────────────
        FmvXW { dest, src } => {
            return FpOutcome::Writeback(dest, hart.fregs[src.0 as usize] as u32 as i32 as i64 as u64);
        }
        FmvXD { dest, src } => {
            return FpOutcome::Writeback(dest, hart.fregs[src.0 as usize]);
        }
        FmvWX { dest, src } => {
            // NaN-box the low 32 bits.
            hart.write_f32(dest, f32::from_bits(iregs[src.0 as usize] as u32));
        }
        FmvDX { dest, src } => {
            hart.write_f64(dest, f64::from_bits(iregs[src.0 as usize]));
        }
        _ => return FpOutcome::NotFp,
    }
    FpOutcome::Done
}

/// Spec-correct fmin: NaN propagates the *other* operand (canonical NaN if
/// both); -0.0 < +0.0.
macro_rules! float_min_max {
    ($fmin:ident, $fmax:ident, $t:ty) => {
        fn $fmin(a: $t, b: $t) -> $t {
            if a.is_nan() && b.is_nan() {
                return <$t>::NAN;
            }
            if a.is_nan() {
                return b;
            }
            if b.is_nan() {
                return a;
            }
            if a == b {
                // ±0: min takes the negative zero
                return if a.is_sign_negative() { a } else { b };
            }
            if a < b { a } else { b }
        }
        fn $fmax(a: $t, b: $t) -> $t {
            if a.is_nan() && b.is_nan() {
                return <$t>::NAN;
            }
            if a.is_nan() {
                return b;
            }
            if b.is_nan() {
                return a;
            }
            if a == b {
                return if a.is_sign_negative() { b } else { a };
            }
            if a < b { b } else { a }
        }
    };
}
float_min_max!(fmin_f32, fmax_f32, f32);
float_min_max!(fmin_f64, fmax_f64, f64);

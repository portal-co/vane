//! Initial AArch64 trace lowering.
//!
//! This deliberately starts with the architectural substrate shared by every
//! later parity family: fixed-width fetch, `disarm64` decode, move-wide
//! integer materialisation, ADD/SUB immediate, and direct/indirect branches.
//! Further `disarm64::decoder_full::Operation` families are added here rather
//! than to Vane's RV64 renderer.

use alloc::{boxed::Box, vec::Vec};
use core::{array, fmt::Formatter};

use disarm64::decoder_full::{
    ADDSUB_EXT, ADDSUB_IMM, ADDSUB_SHIFT, BITFIELD, BRANCH_IMM, BRANCH_REG, COMPBRANCH, CONDBRANCH,
    CONDSEL, DP_2SRC, DP_3SRC, EXCEPTION, FLOAT2INT, FLOATCMP, FLOATDP1, FLOATDP2, FLOATDP3,
    FLOATIMM, FLOATSEL, IC_SYSTEM, LDST_IMM9, LDST_POS, LDST_REGOFF, LDST_UNSCALED,
    LDSTPAIR_INDEXED, LDSTPAIR_OFF, LOG_IMM, LOG_SHIFT, MOVEWIDE, Operation, PCRELADDR,
};
use vane_arch::template::TemplateJit;
use vane_arch::{FloatBinOp, FloatUnaryOp, FloatWidth, Heat, StackOp};

use crate::{Aarch64Display, state};

#[cfg(all(test, feature = "target-js", not(target_arch = "wasm32")))]
#[path = "aarch64/aarch64_js_tests.rs"]
mod aarch64_js_tests;

#[cfg(all(test, feature = "target-js", target_arch = "wasm32"))]
#[path = "aarch64/aarch64_browser_tests.rs"]
mod aarch64_browser_tests;

#[cfg(all(test, feature = "target-wasm", not(target_arch = "wasm32")))]
#[path = "aarch64_wasm_tests.rs"]
mod aarch64_wasm_tests;

fn word_at(this: &TemplateJit<'_>) -> u32 {
    u32::from_le_bytes(array::from_fn(|i| {
        this.params.react.byte(this.pc + i as u64)
    }))
}

#[inline]
fn rd(word: u32) -> u8 {
    (word & 0x1f) as u8
}
fn ra(word: u32) -> u8 {
    ((word >> 10) & 0x1f) as u8
}

fn rt2(word: u32) -> u8 {
    ((word >> 10) & 0x1f) as u8
}

#[inline]
fn rn(word: u32) -> u8 {
    ((word >> 5) & 0x1f) as u8
}

fn read_gpr(ops: &mut Vec<StackOp>, reg: u8, allow_sp: bool) {
    if reg == 31 {
        if allow_sp {
            ops.push(StackOp::LoadState(state::sp()));
        } else {
            ops.push(StackOp::PushImm(0));
        }
    } else {
        ops.push(StackOp::LoadState(state::x(reg)));
    }
}

fn write_gpr(ops: &mut Vec<StackOp>, reg: u8, allow_sp: bool) {
    if reg == 31 {
        if allow_sp {
            ops.push(StackOp::StoreState(state::sp()));
        } else {
            ops.push(StackOp::Drop);
        }
    } else {
        ops.push(StackOp::StoreState(state::x(reg)));
    }
}

fn emit_to(
    ops: &mut Vec<StackOp>,
    this: &TemplateJit<'_>,
    pc: u64,
    labels: vane_arch::template::Labels,
    depth: u32,
) {
    ops.extend(a64_emit(&TemplateJit {
        params: this.params,
        pc,
        labels,
        depth,
    }));
}

fn signed_bits(word: u32, bits: u32) -> i64 {
    ((word << (32 - bits)) as i32 >> (32 - bits)) as i64
}

fn bool_not(ops: &mut Vec<StackOp>) {
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::Eq64);
}

fn bool_and(ops: &mut Vec<StackOp>) {
    // Booleans are normalized to 0/1, so multiplication is logical AND.
    ops.push(StackOp::Mul64);
}

fn bool_xor(ops: &mut Vec<StackOp>) {
    ops.push(StackOp::Xor64);
}

fn condition_test(ops: &mut Vec<StackOp>, cond: u8) -> bool {
    let flag = |ops: &mut Vec<StackOp>, flag| ops.push(StackOp::LoadState(state::nzcv(flag)));
    let invert = cond & 1 != 0;
    match cond & !1 {
        0 => flag(ops, 'z'), // EQ / NE
        2 => flag(ops, 'c'), // CS / CC
        4 => flag(ops, 'n'), // MI / PL
        6 => flag(ops, 'v'), // VS / VC
        8 => {
            // HI / LS: C && !Z
            flag(ops, 'c');
            flag(ops, 'z');
            bool_not(ops);
            bool_and(ops);
        }
        10 => {
            // GE / LT: !(N ^ V)
            flag(ops, 'n');
            flag(ops, 'v');
            bool_xor(ops);
            bool_not(ops);
        }
        12 => {
            // GT / LE: !Z && (N == V)
            flag(ops, 'z');
            bool_not(ops);
            flag(ops, 'n');
            flag(ops, 'v');
            bool_xor(ops);
            bool_not(ops);
            bool_and(ops);
        }
        14 => {
            ops.push(StackOp::PushImm(1)); // AL / NV are unconditional.
            return true;
        }
        _ => unreachable!(),
    }
    if invert {
        bool_not(ops);
    }
    true
}

fn branch(
    ops: &mut Vec<StackOp>,
    this: &TemplateJit<'_>,
    target: u64,
    next: u64,
    labels: vane_arch::template::Labels,
    depth: u32,
) {
    ops.push(StackOp::IfBegin);
    emit_to(ops, this, target, labels.clone(), depth);
    ops.push(StackOp::IfElse);
    emit_to(ops, this, next, labels, depth);
    ops.push(StackOp::IfEnd);
}

fn load_scratch(ops: &mut Vec<StackOp>, name: char, is_32bit: bool) {
    ops.push(StackOp::LoadState(state::scratch(name)));
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
}

fn load_scratch_signed(ops: &mut Vec<StackOp>, name: char, is_32bit: bool) {
    load_scratch(ops, name, is_32bit);
    if is_32bit {
        ops.push(StackOp::SignExt32);
    }
}

fn emit_shifted_gpr(ops: &mut Vec<StackOp>, reg: u8, shift_type: u8, shift: u8, is_32bit: bool) {
    read_gpr(ops, reg, false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if shift != 0 {
        if shift_type == 2 && is_32bit {
            ops.push(StackOp::SignExt32);
        }
        ops.push(StackOp::PushImm(shift as u64));
        ops.push(match shift_type {
            0 => StackOp::Shl64,
            1 => StackOp::ShrU64,
            2 => StackOp::ShrS64,
            _ => unreachable!("invalid shifted-register shift type"),
        });
    }
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
}

fn logical_imm(word: u32) -> u64 {
    let n = (word >> 22) & 1;
    let immr = (word >> 16) & 0x3f;
    let imms = (word >> 10) & 0x3f;
    let ni = (n << 6) | ((!imms) & 0x3f);
    let len = 31 - ni.leading_zeros();
    let esize = 1u32 << len;
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    let ones = if s == 63 {
        u64::MAX
    } else {
        (1u64 << (s + 1)) - 1
    };
    let element = if esize == 64 {
        ones.rotate_right(r)
    } else if r == 0 {
        ones
    } else {
        let mask = (1u64 << esize) - 1;
        ((ones >> r) | (ones << (esize - r))) & mask
    };
    let mut result = 0;
    let mut shift = 0;
    while shift < 64 {
        result |= element << shift;
        shift += esize;
    }
    result
}

fn emit_logical_flags(ops: &mut Vec<StackOp>, dst: u8, is_32bit: bool) {
    ops.push(StackOp::StoreState(state::scratch('r')));
    load_scratch_signed(ops, 'r', is_32bit);
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::LtS64);
    ops.push(StackOp::StoreState(state::nzcv('n')));
    load_scratch(ops, 'r', is_32bit);
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::Eq64);
    ops.push(StackOp::StoreState(state::nzcv('z')));
    for flag in ['c', 'v'] {
        ops.push(StackOp::PushImm(0));
        ops.push(StackOp::StoreState(state::nzcv(flag)));
    }
    load_scratch(ops, 'r', is_32bit);
    write_gpr(ops, dst, false);
}

fn emit_logical_imm(ops: &mut Vec<StackOp>, word: u32, kind: u8, set_flags: bool) {
    let is_32bit = word >> 31 == 0;
    read_gpr(ops, rn(word), false);
    ops.push(StackOp::PushImm(logical_imm(word)));
    ops.push(match kind {
        0 => StackOp::And64,
        1 => StackOp::Or64,
        2 => StackOp::Xor64,
        _ => unreachable!(),
    });
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if set_flags {
        emit_logical_flags(ops, rd(word), is_32bit);
    } else {
        write_gpr(ops, rd(word), false);
    }
}

fn replicate_bits(element: u64, width: u32) -> u64 {
    let mut result = 0;
    let mut shift = 0;
    while shift < 64 {
        result |= element << shift;
        shift += width;
    }
    result
}

fn bitfield_masks(word: u32) -> (u64, u64) {
    let n = (word >> 22) & 1;
    let immr = (word >> 16) & 0x3f;
    let imms = (word >> 10) & 0x3f;
    let ni = (n << 6) | ((!imms) & 0x3f);
    let len = 31 - ni.leading_zeros();
    let esize = 1u32 << len;
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    let diff = s.wrapping_sub(r) & levels;
    let ones = if s == 63 {
        u64::MAX
    } else {
        (1u64 << (s + 1)) - 1
    };
    let top = if diff == 63 {
        u64::MAX
    } else {
        (1u64 << (diff + 1)) - 1
    };
    let rotated = if esize == 64 {
        ones.rotate_right(r)
    } else if r == 0 {
        ones
    } else {
        let mask = (1u64 << esize) - 1;
        ((ones >> r) | (ones << (esize - r))) & mask
    };
    (replicate_bits(rotated, esize), replicate_bits(top, esize))
}

fn emit_bitfield(ops: &mut Vec<StackOp>, word: u32, kind: u8) {
    let is_32bit = word >> 31 == 0;
    let width = if is_32bit { 32 } else { 64 };
    let immr = ((word >> 16) & if is_32bit { 31 } else { 63 }) as u64;
    let (wmask, tmask) = bitfield_masks(word);
    // bot = ROR(Rn, immr) & wmask. ROR #0 is intentionally a no-op.
    read_gpr(ops, rn(word), false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    ops.push(StackOp::StoreState(state::scratch('a')));
    load_scratch(ops, 'a', is_32bit);
    if immr != 0 {
        ops.push(StackOp::PushImm(immr));
        ops.push(StackOp::ShrU64);
        load_scratch(ops, 'a', is_32bit);
        ops.push(StackOp::PushImm(width - immr));
        ops.push(StackOp::Shl64);
        ops.push(StackOp::Or64);
    }
    ops.push(StackOp::PushImm(wmask));
    ops.push(StackOp::And64);
    ops.push(StackOp::PushImm(tmask));
    ops.push(StackOp::And64);
    if kind == 2 {
        // BFM: preserve destination bits outside tmask.
        ops.push(StackOp::StoreState(state::scratch('b')));
        read_gpr(ops, rd(word), false);
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        ops.push(StackOp::PushImm(!tmask));
        ops.push(StackOp::And64);
        load_scratch(ops, 'b', is_32bit);
        ops.push(StackOp::Or64);
    } else if kind == 1 {
        // SBFM: fill the high part selected by ~tmask.
        ops.push(StackOp::StoreState(state::scratch('b')));
        read_gpr(ops, rn(word), false);
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        let imms = ((word >> 10) & if is_32bit { 31 } else { 63 }) as u64;
        ops.push(StackOp::PushImm(imms));
        ops.push(StackOp::ShrU64);
        ops.push(StackOp::PushImm(1));
        ops.push(StackOp::And64);
        ops.push(StackOp::IfBegin);
        ops.push(StackOp::PushImm(!tmask));
        ops.push(StackOp::IfElse);
        ops.push(StackOp::PushImm(0));
        ops.push(StackOp::IfEnd);
        load_scratch(ops, 'b', is_32bit);
        ops.push(StackOp::Or64);
    }
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    write_gpr(ops, rd(word), false);
}

fn emit_extended_gpr(ops: &mut Vec<StackOp>, word: u32, is_32bit: bool) {
    let option = ((word >> 13) & 7) as u8;
    let shift = ((word >> 10) & 7) as u8;
    read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
    match option {
        0 => {
            ops.push(StackOp::PushImm(0xff));
            ops.push(StackOp::And64);
        }
        1 => {
            ops.push(StackOp::PushImm(0xffff));
            ops.push(StackOp::And64);
        }
        2 => ops.push(StackOp::Trunc32),
        4 => {
            ops.push(StackOp::PushImm(56));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::PushImm(56));
            ops.push(StackOp::ShrS64);
        }
        5 => {
            ops.push(StackOp::PushImm(48));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::PushImm(48));
            ops.push(StackOp::ShrS64);
        }
        6 => ops.push(StackOp::SignExt32),
        3 | 7 => {}
        _ => unreachable!(),
    }
    if shift != 0 {
        ops.push(StackOp::PushImm(shift as u64));
        ops.push(StackOp::Shl64);
    }
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
}

fn emit_addsub_ext(ops: &mut Vec<StackOp>, word: u32, is_sub: bool, set_flags: bool) {
    let is_32bit = word >> 31 == 0;
    read_gpr(ops, rn(word), true);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('a')));
        load_scratch(ops, 'a', is_32bit);
    }
    emit_extended_gpr(ops, word, is_32bit);
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('b')));
        load_scratch(ops, 'b', is_32bit);
    }
    ops.push(if is_sub {
        StackOp::Sub64
    } else {
        StackOp::Add64
    });
    if set_flags {
        emit_result_and_flags(ops, rd(word), false, is_32bit, is_sub);
    } else {
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), true);
    }
}

fn emit_dp2(ops: &mut Vec<StackOp>, word: u32, kind: u8) {
    let is_32bit = word >> 31 == 0;
    read_gpr(ops, rn(word), false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if kind == 1 || kind == 4 {
        // signed divide/arithmetic shift
        if is_32bit {
            ops.push(StackOp::SignExt32);
        }
    }
    if kind == 5 {
        // RORV: a >> r | a << (width-r)
        ops.push(StackOp::StoreState(state::scratch('a')));
        read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
        ops.push(StackOp::PushImm(if is_32bit { 31 } else { 63 }));
        ops.push(StackOp::And64);
        ops.push(StackOp::StoreState(state::scratch('b')));
        load_scratch(ops, 'a', is_32bit);
        load_scratch(ops, 'b', false);
        ops.push(StackOp::ShrU64);
        load_scratch(ops, 'a', is_32bit);
        ops.push(StackOp::PushImm(if is_32bit { 32 } else { 64 }));
        load_scratch(ops, 'b', false);
        ops.push(StackOp::Sub64);
        ops.push(StackOp::Shl64);
        ops.push(StackOp::Or64);
    } else {
        read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        ops.push(match kind {
            0 => StackOp::DivU64,
            1 => StackOp::DivS64,
            2 => StackOp::Shl64,
            3 => StackOp::ShrU64,
            4 => StackOp::ShrS64,
            _ => unreachable!(),
        });
    }
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    write_gpr(ops, rd(word), false);
}

fn emit_dp3(ops: &mut Vec<StackOp>, word: u32, kind: u8) {
    let is_32bit = word >> 31 == 0;
    let read_widened = |ops: &mut Vec<StackOp>, reg: u8, signed: bool| {
        read_gpr(ops, reg, false);
        if signed {
            ops.push(StackOp::SignExt32);
        } else {
            ops.push(StackOp::Trunc32);
        }
    };
    match kind {
        0 | 1 => {
            // MADD/MSUB
            read_gpr(ops, ra(word), false);
            read_gpr(ops, rn(word), false);
            read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
            if is_32bit {
                ops.push(StackOp::Trunc32);
                ops.push(StackOp::StoreState(state::scratch('b')));
                ops.push(StackOp::Trunc32);
                load_scratch(ops, 'b', true);
            }
            ops.push(StackOp::Mul64);
            ops.push(if kind == 0 {
                StackOp::Add64
            } else {
                StackOp::Sub64
            });
            if is_32bit {
                ops.push(StackOp::Trunc32);
            }
        }
        2..=5 => {
            // signed/unsigned long MADD/MSUB
            read_gpr(ops, ra(word), false);
            read_widened(ops, rn(word), kind == 2 || kind == 3);
            read_widened(ops, ((word >> 16) & 0x1f) as u8, kind == 2 || kind == 3);
            ops.push(StackOp::Mul64);
            ops.push(if kind == 2 || kind == 4 {
                StackOp::Add64
            } else {
                StackOp::Sub64
            });
        }
        6 | 7 => {
            read_gpr(ops, rn(word), false);
            read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
            ops.push(if kind == 6 {
                StackOp::MulHS64
            } else {
                StackOp::MulHU64
            });
        }
        _ => unreachable!(),
    }
    write_gpr(ops, rd(word), false);
}

fn emit_result_and_flags(
    ops: &mut Vec<StackOp>,
    dst: u8,
    allow_sp: bool,
    is_32bit: bool,
    is_sub: bool,
) {
    // The arithmetic producer leaves its result on stack. Preserve it, then
    // derive all NZCV bits from explicitly saved operands and result.
    ops.push(StackOp::StoreState(state::scratch('r')));
    load_scratch(ops, 'r', is_32bit);
    write_gpr(ops, dst, allow_sp);

    load_scratch_signed(ops, 'r', is_32bit);
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::LtS64);
    ops.push(StackOp::StoreState(state::nzcv('n')));
    load_scratch(ops, 'r', is_32bit);
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::Eq64);
    ops.push(StackOp::StoreState(state::nzcv('z')));

    // C is carry out for addition and no-borrow for subtraction.
    load_scratch(ops, if is_sub { 'a' } else { 'r' }, is_32bit);
    load_scratch(ops, if is_sub { 'b' } else { 'a' }, is_32bit);
    ops.push(if is_sub {
        StackOp::GeU64
    } else {
        StackOp::LtU64
    });
    ops.push(StackOp::StoreState(state::nzcv('c')));

    // V: ADD => sign(a)==sign(b) && sign(r)!=sign(a);
    //    SUB => sign(a)!=sign(b) && sign(r)!=sign(a).
    for name in ['a', 'b', 'r'] {
        load_scratch_signed(ops, name, is_32bit);
        ops.push(StackOp::PushImm(0));
        ops.push(StackOp::LtS64);
        ops.push(StackOp::StoreState(state::scratch(name)));
    }
    load_scratch(ops, 'a', false);
    load_scratch(ops, 'b', false);
    bool_xor(ops);
    if !is_sub {
        bool_not(ops);
    }
    load_scratch(ops, 'r', false);
    load_scratch(ops, 'a', false);
    bool_xor(ops);
    bool_and(ops);
    ops.push(StackOp::StoreState(state::nzcv('v')));
}

fn emit_addsub(
    ops: &mut Vec<StackOp>,
    word: u32,
    is_sub: bool,
    allow_sp: bool,
    set_flags: bool,
    shift_type: u8,
    shift: u8,
) {
    let is_32bit = word >> 31 == 0;
    read_gpr(ops, rn(word), allow_sp);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('a')));
        load_scratch(ops, 'a', is_32bit);
    }
    emit_shifted_gpr(
        ops,
        ((word >> 16) & 0x1f) as u8,
        shift_type,
        shift,
        is_32bit,
    );
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('b')));
        load_scratch(ops, 'b', is_32bit);
    }
    ops.push(if is_sub {
        StackOp::Sub64
    } else {
        StackOp::Add64
    });
    if set_flags {
        // See the matching comment in `emit_addsub_imm`: flag-setting
        // ADDS/SUBS never write SP as Rd (31 there means XZR).
        emit_result_and_flags(ops, rd(word), false, is_32bit, is_sub);
    } else {
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), allow_sp);
    }
}

fn emit_mrs_nzcv(ops: &mut Vec<StackOp>, word: u32) {
    for (flag, shift) in [('n', 31), ('z', 30), ('c', 29), ('v', 28)] {
        ops.push(StackOp::LoadState(state::nzcv(flag)));
        ops.push(StackOp::PushImm(shift));
        ops.push(StackOp::Shl64);
        if shift != 31 {
            ops.push(StackOp::Or64);
        }
    }
    write_gpr(ops, rd(word), false);
}

fn emit_msr_nzcv(ops: &mut Vec<StackOp>, word: u32) {
    for (flag, shift) in [('n', 31), ('z', 30), ('c', 29), ('v', 28)] {
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::PushImm(shift));
        ops.push(StackOp::ShrU64);
        ops.push(StackOp::PushImm(1));
        ops.push(StackOp::And64);
        ops.push(StackOp::StoreState(state::nzcv(flag)));
    }
}

fn emit_condsel(ops: &mut Vec<StackOp>, word: u32, kind: u8) {
    let is_32bit = word >> 31 == 0;
    let cond = ((word >> 12) & 0xf) as u8;
    // StackOp's IfBegin only evaluates one branch, exactly matching CSEL's
    // conditionally selected operand behavior.
    condition_test(ops, cond);
    ops.push(StackOp::IfBegin);
    read_gpr(ops, rn(word), false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    ops.push(StackOp::IfElse);
    read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    match kind {
        1 => {
            ops.push(StackOp::PushImm(1));
            ops.push(StackOp::Add64);
        }
        2 => {
            ops.push(StackOp::PushImm(if is_32bit {
                0xffff_ffff
            } else {
                u64::MAX
            }));
            ops.push(StackOp::Xor64);
        }
        3 => {
            ops.push(StackOp::StoreState(state::scratch('b')));
            ops.push(StackOp::PushImm(0));
            ops.push(StackOp::LoadState(state::scratch('b')));
            ops.push(StackOp::Sub64);
        }
        _ => {}
    }
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    ops.push(StackOp::IfEnd);
    write_gpr(ops, rd(word), false);
}

fn emit_float_fma(ops: &mut Vec<StackOp>, word: u32, subtract: bool, negate: bool) {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    // StackOp deliberately rounds after multiply and add. This is a defined
    // non-fused policy for the initial JIT/interpreter implementation.
    read_fp(ops, rn(word));
    read_fp(ops, ((word >> 16) & 0x1f) as u8);
    ops.push(StackOp::FloatBinary {
        width,
        op: FloatBinOp::Mul,
    });
    if subtract {
        ops.push(StackOp::FloatUnary {
            width,
            op: FloatUnaryOp::Neg,
        });
    }
    read_fp(ops, ra(word));
    ops.push(StackOp::FloatBinary {
        width,
        op: FloatBinOp::Add,
    });
    if negate {
        ops.push(StackOp::FloatUnary {
            width,
            op: FloatUnaryOp::Neg,
        });
    }
    write_fp(ops, rd(word), width);
}

fn emit_float_imm(ops: &mut Vec<StackOp>, word: u32) {
    let imm8 = (word >> 13) & 0xff;
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    let sign = imm8 >> 7;
    let b6 = (imm8 >> 6) & 1;
    let e_lo = (imm8 >> 4) & 3;
    let frac = imm8 & 0xf;
    let bits = match width {
        FloatWidth::F32 => {
            let exponent = ((!b6 & 1) << 7) | (b6 * 0x7c) | e_lo;
            ((sign << 31) | (exponent << 23) | (frac << 19)) as u64
        }
        FloatWidth::F64 => {
            let exponent = ((!b6 & 1) << 10) | (b6 * 0x3fc) | e_lo;
            ((sign as u64) << 63) | ((exponent as u64) << 52) | ((frac as u64) << 48)
        }
    };
    ops.push(StackOp::PushImm(bits));
    write_fp(ops, rd(word), width);
}

fn emit_float2int(ops: &mut Vec<StackOp>, word: u32, kind: u8) {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    let int_width = if word >> 31 != 0 { 64 } else { 32 };
    match kind {
        0 => {
            read_gpr(ops, rn(word), false);
            if width == FloatWidth::F32 {
                ops.push(StackOp::Trunc32);
            }
            write_fp(ops, rd(word), width);
        }
        1 => {
            read_fp(ops, rn(word));
            if width == FloatWidth::F32 {
                ops.push(StackOp::Trunc32);
            }
            write_gpr(ops, rd(word), false);
        }
        2 | 3 => {
            read_gpr(ops, rn(word), false);
            ops.push(StackOp::IntToFloat {
                width,
                signed: kind == 2,
                int_width,
            });
            write_fp(ops, rd(word), width);
        }
        4 | 5 => {
            read_fp(ops, rn(word));
            ops.push(StackOp::FloatToInt {
                width,
                signed: kind == 4,
                int_width,
            });
            if int_width == 32 {
                ops.push(StackOp::Trunc32);
            }
            write_gpr(ops, rd(word), false);
        }
        _ => unreachable!(),
    }
}

fn emit_float_compare(ops: &mut Vec<StackOp>, word: u32, compare_zero: bool) {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    let load_pair = |ops: &mut Vec<StackOp>| {
        read_fp(ops, rn(word));
        if compare_zero {
            ops.push(StackOp::PushImm(0));
        } else {
            read_fp(ops, ((word >> 16) & 0x1f) as u8);
        }
    };
    // ARM: N=less, Z=equal, V=unordered, C=greater-or-equal or unordered.
    load_pair(ops);
    ops.push(StackOp::FloatLt { width });
    ops.push(StackOp::StoreState(state::nzcv('n')));
    load_pair(ops);
    ops.push(StackOp::FloatEq { width });
    ops.push(StackOp::StoreState(state::nzcv('z')));
    load_pair(ops);
    ops.push(StackOp::FloatUnordered { width });
    ops.push(StackOp::StoreState(state::nzcv('v')));
    load_pair(ops);
    ops.push(StackOp::FloatLt { width });
    ops.push(StackOp::PushImm(0));
    ops.push(StackOp::Eq64);
    ops.push(StackOp::StoreState(state::nzcv('c')));
}

fn emit_float_select(ops: &mut Vec<StackOp>, word: u32) -> bool {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    if !condition_test(ops, ((word >> 12) & 0xf) as u8) {
        return false;
    }
    ops.push(StackOp::IfBegin);
    read_fp(ops, rn(word));
    write_fp(ops, rd(word), width);
    ops.push(StackOp::IfElse);
    read_fp(ops, ((word >> 16) & 0x1f) as u8);
    write_fp(ops, rd(word), width);
    ops.push(StackOp::IfEnd);
    true
}

fn emit_float_dp2(ops: &mut Vec<StackOp>, word: u32, op: FloatBinOp) {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    let rn = rn(word);
    let rm = ((word >> 16) & 0x1f) as u8;
    read_fp(ops, rn);
    read_fp(ops, rm);
    ops.push(StackOp::FloatBinary { width, op });
    write_fp(ops, rd(word), width);
}

fn read_fp(ops: &mut Vec<StackOp>, reg: u8) {
    ops.push(StackOp::LoadState(state::v(reg)));
}

fn write_fp(ops: &mut Vec<StackOp>, reg: u8, width: FloatWidth) {
    if width == FloatWidth::F32 {
        ops.push(StackOp::Trunc32);
    }
    ops.push(StackOp::StoreState(state::v(reg)));
}

fn emit_float_dp1(ops: &mut Vec<StackOp>, word: u32, op: FloatUnaryOp) {
    let width = if word & (1 << 22) == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    };
    read_fp(ops, rn(word));
    ops.push(StackOp::FloatUnary { width, op });
    write_fp(ops, rd(word), width);
}

fn emit_pcreladdr(ops: &mut Vec<StackOp>, this: &TemplateJit<'_>, word: u32, page: bool) {
    let immhi = ((word >> 5) & 0x7ffff) << 2;
    let immlo = (word >> 29) & 3;
    let offset = signed_bits(immhi | immlo, 21);
    let base = if page { this.pc & !0xfff } else { this.pc };
    ops.push(StackOp::PushImm(
        base.wrapping_add_signed(offset << if page { 12 } else { 0 }),
    ));
    write_gpr(ops, rd(word), false);
}

fn emit_log_shift(
    ops: &mut Vec<StackOp>,
    word: u32,
    op: StackOp,
    invert_rhs: bool,
    shift_type: u8,
    shift: u8,
) {
    let is_32bit = word >> 31 == 0;
    read_gpr(ops, rn(word), false);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    emit_shifted_gpr(
        ops,
        ((word >> 16) & 0x1f) as u8,
        shift_type,
        shift,
        is_32bit,
    );
    if invert_rhs {
        ops.push(StackOp::PushImm(if is_32bit {
            0xffff_ffff
        } else {
            u64::MAX
        }));
        ops.push(StackOp::Xor64);
    }
    ops.push(op);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    write_gpr(ops, rd(word), false);
}

fn emit_addsub_imm(
    ops: &mut Vec<StackOp>,
    word: u32,
    is_sub: bool,
    allow_sp: bool,
    set_flags: bool,
) {
    let is_32bit = word >> 31 == 0;
    let imm = (((word >> 10) & 0xfff) as u64) << if word & (1 << 22) != 0 { 12 } else { 0 };
    read_gpr(ops, rn(word), allow_sp);
    if is_32bit {
        ops.push(StackOp::Trunc32);
    }
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('a')));
        load_scratch(ops, 'a', is_32bit);
    }
    ops.push(StackOp::PushImm(imm));
    if set_flags {
        ops.push(StackOp::StoreState(state::scratch('b')));
        load_scratch(ops, 'b', is_32bit);
    }
    ops.push(if is_sub {
        StackOp::Sub64
    } else {
        StackOp::Add64
    });
    if set_flags {
        // The flag-setting forms (ADDS/SUBS, e.g. `cmp`/`cmn`) never allow SP
        // as the destination -- Rd=31 there means XZR (discard), unlike the
        // non-flag ADD/SUB immediate forms where Rd=31 legitimately means SP.
        // Passing `allow_sp` through unconditionally here previously routed
        // e.g. `cmp x9, #imm`'s discarded result into the SP state slot.
        emit_result_and_flags(ops, rd(word), false, is_32bit, is_sub);
    } else {
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), allow_sp);
    }
}

fn emit_regoff_mem(
    ops: &mut Vec<StackOp>,
    word: u32,
    load: bool,
    width: vane_arch::MemWidth,
    signed: bool,
) {
    let scale = match width {
        vane_arch::MemWidth::B1 => 0,
        vane_arch::MemWidth::B2 => 1,
        vane_arch::MemWidth::B4 => 2,
        vane_arch::MemWidth::B8 => 3,
    };
    read_gpr(ops, rn(word), true);
    read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
    match (word >> 13) & 7 {
        2 => {
            ops.push(StackOp::PushImm(0xffff_ffff));
            ops.push(StackOp::And64);
        }
        6 => ops.push(StackOp::SignExt32),
        3 | 7 => {}
        _ => unreachable!("invalid A64 register-offset extension"),
    }
    if word & (1 << 12) != 0 && scale != 0 {
        ops.push(StackOp::PushImm(scale));
        ops.push(StackOp::Shl64);
    }
    ops.push(StackOp::Add64);
    if load {
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), false);
    } else {
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::StoreMem { width });
    }
}

fn emit_imm9_mem(
    ops: &mut Vec<StackOp>,
    word: u32,
    load: bool,
    width: vane_arch::MemWidth,
    signed: bool,
) {
    let offset = signed_bits((word >> 12) & 0x1ff, 9) as u64;
    let mode = (word >> 10) & 3; // 00 offset, 01 post-index, 11 pre-index
    read_gpr(ops, rn(word), true);
    ops.push(StackOp::StoreState(state::scratch('a')));
    if mode == 3 {
        ops.push(StackOp::LoadState(state::scratch('a')));
        ops.push(StackOp::PushImm(offset));
        ops.push(StackOp::Add64);
        ops.push(StackOp::StoreState(state::scratch('a')));
        ops.push(StackOp::LoadState(state::scratch('a')));
        write_gpr(ops, rn(word), true);
    }
    ops.push(StackOp::LoadState(state::scratch('a')));
    if load {
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), false);
    } else {
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::StoreMem { width });
    }
    if mode == 1 {
        ops.push(StackOp::LoadState(state::scratch('a')));
        ops.push(StackOp::PushImm(offset));
        ops.push(StackOp::Add64);
        write_gpr(ops, rn(word), true);
    }
}

fn emit_pair_mem(
    ops: &mut Vec<StackOp>,
    word: u32,
    load: bool,
    width: vane_arch::MemWidth,
    signed: bool,
    writeback: bool,
    pre_index: bool,
) {
    let scale = match width {
        vane_arch::MemWidth::B4 => 4,
        vane_arch::MemWidth::B8 => 8,
        _ => unreachable!("A64 integer pairs are word or doubleword"),
    };
    let offset = signed_bits((word >> 15) & 0x7f, 7) * scale;
    read_gpr(ops, rn(word), true);
    ops.push(StackOp::StoreState(state::scratch('b')));
    if writeback && !pre_index {
        // Post-index accesses the old base, then applies the displacement.
        ops.push(StackOp::LoadState(state::scratch('b')));
    } else {
        ops.push(StackOp::LoadState(state::scratch('b')));
        ops.push(StackOp::PushImm(offset as u64));
        ops.push(StackOp::Add64);
    }
    ops.push(StackOp::StoreState(state::scratch('a')));
    if writeback && pre_index {
        ops.push(StackOp::LoadState(state::scratch('a')));
        write_gpr(ops, rn(word), true);
    }
    if load {
        ops.push(StackOp::LoadState(state::scratch('a')));
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), false);
        ops.push(StackOp::LoadState(state::scratch('a')));
        ops.push(StackOp::PushImm(scale as u64));
        ops.push(StackOp::Add64);
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rt2(word), false);
    } else {
        ops.push(StackOp::LoadState(state::scratch('a')));
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::StoreMem { width });
        ops.push(StackOp::LoadState(state::scratch('a')));
        ops.push(StackOp::PushImm(scale as u64));
        ops.push(StackOp::Add64);
        read_gpr(ops, rt2(word), false);
        ops.push(StackOp::StoreMem { width });
    }
    if writeback && !pre_index {
        ops.push(StackOp::LoadState(state::scratch('b')));
        ops.push(StackOp::PushImm(offset as u64));
        ops.push(StackOp::Add64);
        write_gpr(ops, rn(word), true);
    }
}

fn emit_signed_mem(
    ops: &mut Vec<StackOp>,
    word: u32,
    load: bool,
    width: vane_arch::MemWidth,
    signed: bool,
) {
    read_gpr(ops, rn(word), true);
    ops.push(StackOp::PushImm(signed_bits((word >> 12) & 0x1ff, 9) as u64));
    ops.push(StackOp::Add64);
    if load {
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), false);
    } else {
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::StoreMem { width });
    }
}

fn emit_pos_mem(
    ops: &mut Vec<StackOp>,
    word: u32,
    load: bool,
    width: vane_arch::MemWidth,
    signed: bool,
    scale: u64,
) {
    // Integer scalar load/store base register 31 denotes SP.
    read_gpr(ops, rn(word), true);
    ops.push(StackOp::PushImm(((word >> 10) & 0xfff) as u64 * scale));
    ops.push(StackOp::Add64);
    if load {
        ops.push(StackOp::LoadMem { width, signed });
        if width == vane_arch::MemWidth::B4 && !signed {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), false);
    } else {
        read_gpr(ops, rd(word), false);
        ops.push(StackOp::StoreMem { width });
    }
}

pub fn a64_emit(this: &TemplateJit<'_>) -> Vec<StackOp> {
    if (this.params.trial)(this.pc) == Heat::Cached {
        return alloc::vec![StackOp::PushImm(this.pc), StackOp::TailCall];
    }

    let mut labels = this.labels.clone();
    let mut ops = Vec::new();
    match labels.0.entry(this.pc) {
        alloc::collections::btree_map::Entry::Vacant(entry) => {
            let label = alloc::format!("a64_{:x}", this.pc);
            entry.insert(vane_arch::template::Label {
                ident_name: label.clone(),
                index: this.depth,
            });
            ops.push(StackOp::LoopBegin { label });
            ops.extend(a64_body(this, labels, this.depth + 1));
            ops.push(StackOp::LoopEnd);
        }
        alloc::collections::btree_map::Entry::Occupied(entry) => {
            let label = entry.get().clone();
            ops.push(StackOp::Br {
                relative_depth: this.depth - label.index,
                label: label.ident_name,
            });
        }
    }
    ops
}

fn a64_body(
    this: &TemplateJit<'_>,
    labels: vane_arch::template::Labels,
    depth: u32,
) -> Vec<StackOp> {
    let word = word_at(this);
    let bytes = word.to_le_bytes();
    let mut ops = alloc::vec![StackOp::CheckCode {
        addr: this.pc,
        expected: Box::from(bytes),
        miss_label: alloc::format!("{}", this.params.root),
    }];
    let next = this.pc.wrapping_add(4);
    let Some(opcode) = disarm64::decoder::decode(word) else {
        ops.push(StackOp::Trap(alloc::format!(
            "aarch64: undefined {word:08x}"
        )));
        return ops;
    };

    match opcode.operation {
        Operation::MOVEWIDE(inner) => {
            let (kind, word) = match inner {
                MOVEWIDE::MOVZ_Rd_HALF(v) => (0, v.0),
                MOVEWIDE::MOVN_Rd_HALF(v) => (1, v.0),
                MOVEWIDE::MOVK_Rd_HALF(v) => (2, v.0),
            };
            let dst = rd(word);
            let shift = ((word >> 21) & 3) * 16;
            let imm = ((word >> 5) & 0xffff) as u64;
            match kind {
                0 => ops.push(StackOp::PushImm(imm << shift)),
                1 => ops.push(StackOp::PushImm(!(imm << shift))),
                _ => {
                    read_gpr(&mut ops, dst, false);
                    ops.push(StackOp::PushImm(!(0xffffu64 << shift)));
                    ops.push(StackOp::And64);
                    ops.push(StackOp::PushImm(imm << shift));
                    ops.push(StackOp::Or64);
                }
            }
            // W forms zero the high half. MOVEWIDE's sf is bit 31.
            if word >> 31 == 0 {
                ops.push(StackOp::Trunc32);
            }
            write_gpr(&mut ops, dst, false);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::ADDSUB_IMM(inner) => {
            let (is_sub, set_flags, word) = match inner {
                ADDSUB_IMM::ADD_Rd_SP_Rn_SP_AIMM(v) => (false, false, v.0),
                ADDSUB_IMM::ADDS_Rd_Rn_SP_AIMM(v) => (false, true, v.0),
                ADDSUB_IMM::SUB_Rd_SP_Rn_SP_AIMM(v) => (true, false, v.0),
                ADDSUB_IMM::SUBS_Rd_Rn_SP_AIMM(v) => (true, true, v.0),
                _ => return unsupported(&mut ops, word, "ADDSUB_IMM"),
            };
            emit_addsub_imm(&mut ops, word, is_sub, true, set_flags);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATDP2(inner) => {
            let (word, op, negate) = match inner {
                FLOATDP2::FADD_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Add, false),
                FLOATDP2::FADD_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Add, false),
                FLOATDP2::FSUB_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Sub, false),
                FLOATDP2::FSUB_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Sub, false),
                FLOATDP2::FMUL_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Mul, false),
                FLOATDP2::FMUL_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Mul, false),
                FLOATDP2::FDIV_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Div, false),
                FLOATDP2::FDIV_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Div, false),
                FLOATDP2::FNMUL_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Mul, true),
                FLOATDP2::FNMUL_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Mul, true),
                FLOATDP2::FMIN_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Min, false),
                FLOATDP2::FMIN_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Min, false),
                FLOATDP2::FMINNM_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Min, false),
                FLOATDP2::FMINNM_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Min, false),
                FLOATDP2::FMAX_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Max, false),
                FLOATDP2::FMAX_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Max, false),
                FLOATDP2::FMAXNM_Fd_Fn_Fm(v) => (v.0, FloatBinOp::Max, false),
                FLOATDP2::FMAXNM_Fd_S_S_Fn_S_S_Fm_S_S(v) => (v.0, FloatBinOp::Max, false),
            };
            emit_float_dp2(&mut ops, word, op);
            if negate {
                // Re-read the just-written result so the width-specific negation
                // preserves scalar S register bits.
                let width = if word & (1 << 22) == 0 {
                    FloatWidth::F32
                } else {
                    FloatWidth::F64
                };
                read_fp(&mut ops, rd(word));
                ops.push(StackOp::FloatUnary {
                    width,
                    op: FloatUnaryOp::Neg,
                });
                write_fp(&mut ops, rd(word), width);
            }
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATDP3(inner) => {
            let (word, subtract, negate) = match inner {
                FLOATDP3::FMADD_Fd_Fn_Fm_Fa(v) => (v.0, false, false),
                FLOATDP3::FMADD_Fd_S_S_Fn_S_S_Fm_S_S_Fa_S_S(v) => (v.0, false, false),
                FLOATDP3::FMSUB_Fd_Fn_Fm_Fa(v) => (v.0, true, false),
                FLOATDP3::FMSUB_Fd_S_S_Fn_S_S_Fm_S_S_Fa_S_S(v) => (v.0, true, false),
                FLOATDP3::FNMADD_Fd_Fn_Fm_Fa(v) => (v.0, false, true),
                FLOATDP3::FNMADD_Fd_S_S_Fn_S_S_Fm_S_S_Fa_S_S(v) => (v.0, false, true),
                FLOATDP3::FNMSUB_Fd_Fn_Fm_Fa(v) => (v.0, true, true),
                FLOATDP3::FNMSUB_Fd_S_S_Fn_S_S_Fm_S_S_Fa_S_S(v) => (v.0, true, true),
            };
            emit_float_fma(&mut ops, word, subtract, negate);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATIMM(inner) => {
            let word = match inner {
                FLOATIMM::FMOV_Fd_FPIMM(v) => v.0,
                FLOATIMM::FMOV_Fd_S_S_FPIMM(v) => v.0,
            };
            emit_float_imm(&mut ops, word);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOAT2INT(inner) => {
            let (word, kind) = match inner {
                FLOAT2INT::FMOV_Fd_Rn(v) => (v.0, 0),
                FLOAT2INT::FMOV_Fd_S_S_Rn_W(v) => (v.0, 0),
                FLOAT2INT::FMOV_Rd_Fn(v) => (v.0, 1),
                FLOAT2INT::FMOV_Rd_W_Fn_S_S(v) => (v.0, 1),
                FLOAT2INT::SCVTF_Fd_S_D_Rn_W(v) => (v.0, 2),
                FLOAT2INT::UCVTF_Fd_S_D_Rn_W(v) => (v.0, 3),
                FLOAT2INT::FCVTZS_Rd_W_Fn_S_D(v) => (v.0, 4),
                FLOAT2INT::FCVTZU_Rd_W_Fn_S_D(v) => (v.0, 5),
                _ => return unsupported(&mut ops, word, "FLOAT2INT"),
            };
            emit_float2int(&mut ops, word, kind);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATCMP(inner) => {
            let (word, zero) = match inner {
                FLOATCMP::FCMP_Fn_Fm(v) => (v.0, false),
                FLOATCMP::FCMPE_Fn_Fm(v) => (v.0, false),
                FLOATCMP::FCMP_Fn_S_S_Fm_S_S(v) => (v.0, false),
                FLOATCMP::FCMPE_Fn_S_S_Fm_S_S(v) => (v.0, false),
                FLOATCMP::FCMP_Fn_FPIMM0(v) => (v.0, true),
                FLOATCMP::FCMPE_Fn_FPIMM0(v) => (v.0, true),
                FLOATCMP::FCMP_Fn_S_S_FPIMM0(v) => (v.0, true),
                FLOATCMP::FCMPE_Fn_S_S_FPIMM0(v) => (v.0, true),
            };
            emit_float_compare(&mut ops, word, zero);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATSEL(inner) => {
            let word = match inner {
                FLOATSEL::FCSEL_Fd_Fn_Fm_COND(v) => v.0,
                FLOATSEL::FCSEL_Fd_S_S_Fn_S_S_Fm_S_S_COND(v) => v.0,
            };
            if !emit_float_select(&mut ops, word) {
                return unsupported(&mut ops, word, "FLOATSEL");
            }
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::FLOATDP1(inner) => {
            let word = match inner {
                FLOATDP1::FABS_Fd_Fn(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Abs);
                    v.0
                }
                FLOATDP1::FABS_Fd_S_S_Fn_S_S(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Abs);
                    v.0
                }
                FLOATDP1::FNEG_Fd_Fn(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Neg);
                    v.0
                }
                FLOATDP1::FNEG_Fd_S_S_Fn_S_S(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Neg);
                    v.0
                }
                FLOATDP1::FSQRT_Fd_Fn(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Sqrt);
                    v.0
                }
                FLOATDP1::FSQRT_Fd_S_S_Fn_S_S(v) => {
                    emit_float_dp1(&mut ops, v.0, FloatUnaryOp::Sqrt);
                    v.0
                }
                FLOATDP1::FMOV_Fd_Fn(v) => {
                    let width = if v.0 & (1 << 22) == 0 {
                        FloatWidth::F32
                    } else {
                        FloatWidth::F64
                    };
                    read_fp(&mut ops, rn(v.0));
                    write_fp(&mut ops, rd(v.0), width);
                    v.0
                }
                FLOATDP1::FMOV_Fd_S_S_Fn_S_S(v) => {
                    let width = if v.0 & (1 << 22) == 0 {
                        FloatWidth::F32
                    } else {
                        FloatWidth::F64
                    };
                    read_fp(&mut ops, rn(v.0));
                    write_fp(&mut ops, rd(v.0), width);
                    v.0
                }
                FLOATDP1::FCVT_Fd_Fn(v) => {
                    let from = if v.0 & (1 << 22) == 0 {
                        FloatWidth::F32
                    } else {
                        FloatWidth::F64
                    };
                    let to = if (v.0 >> 15) & 3 == 0 {
                        FloatWidth::F32
                    } else {
                        FloatWidth::F64
                    };
                    read_fp(&mut ops, rn(v.0));
                    ops.push(StackOp::FloatConvert { from, to });
                    write_fp(&mut ops, rd(v.0), to);
                    v.0
                }
                _ => return unsupported(&mut ops, word, "FLOATDP1"),
            };
            let _ = word;
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::IC_SYSTEM(inner) => {
            // Only the architectural NZCV system register belongs to the
            // integer execution state currently modeled by Vane.
            const NZCV: u32 = 0x5a10;
            match inner {
                IC_SYSTEM::MRS_Rt_SYSREG(v) if ((v.0 >> 5) & 0xffff) == NZCV => {
                    emit_mrs_nzcv(&mut ops, v.0);
                }
                IC_SYSTEM::MSR_SYSREG_Rt(v) if ((v.0 >> 5) & 0xffff) == NZCV => {
                    emit_msr_nzcv(&mut ops, v.0);
                }
                _ => return unsupported(&mut ops, word, "IC_SYSTEM"),
            }
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::CONDSEL(inner) => {
            let (word, kind) = match inner {
                CONDSEL::CSEL_Rd_Rn_Rm_COND(v) => (v.0, 0),
                CONDSEL::CSINC_Rd_Rn_Rm_COND(v) => (v.0, 1),
                CONDSEL::CSINV_Rd_Rn_Rm_COND(v) => (v.0, 2),
                CONDSEL::CSNEG_Rd_Rn_Rm_COND(v) => (v.0, 3),
            };
            emit_condsel(&mut ops, word, kind);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::PCRELADDR(inner) => {
            let (word, page) = match inner {
                PCRELADDR::ADR_Rd_ADDR_PCREL21(v) => (v.0, false),
                PCRELADDR::ADRP_Rd_ADDR_ADRP(v) => (v.0, true),
            };
            emit_pcreladdr(&mut ops, this, word, page);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LOG_SHIFT(inner) => {
            let (word, op, invert_rhs) = match inner {
                LOG_SHIFT::AND_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::And64, false),
                LOG_SHIFT::BIC_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::And64, true),
                LOG_SHIFT::ORR_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::Or64, false),
                LOG_SHIFT::ORN_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::Or64, true),
                LOG_SHIFT::EOR_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::Xor64, false),
                LOG_SHIFT::EON_Rd_Rn_Rm_SFT(v) => (v.0, StackOp::Xor64, true),
                LOG_SHIFT::ANDS_Rd_Rn_Rm_SFT(v) => {
                    return unsupported(&mut ops, v.0, "LOG_SHIFT flags pending");
                }
                LOG_SHIFT::BICS_Rd_Rn_Rm_SFT(v) => {
                    return unsupported(&mut ops, v.0, "LOG_SHIFT flags pending");
                }
            };
            emit_log_shift(
                &mut ops,
                word,
                op,
                invert_rhs,
                ((word >> 22) & 3) as u8,
                ((word >> 10) & 0x3f) as u8,
            );
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDST_REGOFF(inner) => {
            use vane_arch::MemWidth::{B1, B2, B4, B8};
            let (word, load, width, signed) = match inner {
                LDST_REGOFF::LDR_Rt_ADDR_REGOFF(v) => {
                    (v.0, true, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_REGOFF::STR_Rt_ADDR_REGOFF(v) => {
                    (v.0, false, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_REGOFF::LDRB_Rt_ADDR_REGOFF(v) => (v.0, true, B1, false),
                LDST_REGOFF::STRB_Rt_ADDR_REGOFF(v) => (v.0, false, B1, false),
                LDST_REGOFF::LDRH_Rt_ADDR_REGOFF(v) => (v.0, true, B2, false),
                LDST_REGOFF::STRH_Rt_ADDR_REGOFF(v) => (v.0, false, B2, false),
                LDST_REGOFF::LDRSB_Rt_ADDR_REGOFF(v) => (v.0, true, B1, true),
                LDST_REGOFF::LDRSH_Rt_ADDR_REGOFF(v) => (v.0, true, B2, true),
                LDST_REGOFF::LDRSW_Rt_ADDR_REGOFF(v) => (v.0, true, B4, true),
                _ => return unsupported(&mut ops, word, "LDST_REGOFF"),
            };
            emit_regoff_mem(&mut ops, word, load, width, signed);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDST_IMM9(inner) => {
            use vane_arch::MemWidth::{B1, B2, B4, B8};
            let (word, load, width, signed) = match inner {
                LDST_IMM9::LDR_Rt_ADDR_SIMM9(v) => {
                    (v.0, true, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_IMM9::STR_Rt_ADDR_SIMM9(v) => {
                    (v.0, false, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_IMM9::LDRB_Rt_ADDR_SIMM9(v) => (v.0, true, B1, false),
                LDST_IMM9::STRB_Rt_ADDR_SIMM9(v) => (v.0, false, B1, false),
                LDST_IMM9::LDRH_Rt_ADDR_SIMM9(v) => (v.0, true, B2, false),
                LDST_IMM9::STRH_Rt_ADDR_SIMM9(v) => (v.0, false, B2, false),
                LDST_IMM9::LDRSB_Rt_ADDR_SIMM9(v) => (v.0, true, B1, true),
                LDST_IMM9::LDRSH_Rt_ADDR_SIMM9(v) => (v.0, true, B2, true),
                LDST_IMM9::LDRSW_Rt_ADDR_SIMM9(v) => (v.0, true, B4, true),
                _ => return unsupported(&mut ops, word, "LDST_IMM9"),
            };
            emit_imm9_mem(&mut ops, word, load, width, signed);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDSTPAIR_INDEXED(inner) => {
            use vane_arch::MemWidth::B4;
            let (word, load, width, signed) = match inner {
                LDSTPAIR_INDEXED::LDP_Rt_W_Rt2_W_ADDR_SIMM7_S_S(v) => (v.0, true, B4, false),
                LDSTPAIR_INDEXED::STP_Rt_W_Rt2_W_ADDR_SIMM7_S_S(v) => (v.0, false, B4, false),
                LDSTPAIR_INDEXED::LDPSW_Rt_X_Rt2_X_ADDR_SIMM7_S_S(v) => (v.0, true, B4, true),
                _ => return unsupported(&mut ops, word, "LDSTPAIR_INDEXED"),
            };
            emit_pair_mem(
                &mut ops,
                word,
                load,
                width,
                signed,
                true,
                word & (1 << 24) != 0,
            );
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDSTPAIR_OFF(inner) => {
            use vane_arch::MemWidth::{B4, B8};
            let (word, load, width, signed) = match inner {
                LDSTPAIR_OFF::LDP_Rt_Rt2_ADDR_SIMM7(v) => {
                    (v.0, true, if v.0 >> 31 == 0 { B4 } else { B8 }, false)
                }
                LDSTPAIR_OFF::STP_Rt_Rt2_ADDR_SIMM7(v) => {
                    (v.0, false, if v.0 >> 31 == 0 { B4 } else { B8 }, false)
                }
                LDSTPAIR_OFF::LDPSW_Rt_Rt2_ADDR_SIMM7(v) => (v.0, true, B4, true),
                _ => return unsupported(&mut ops, word, "LDSTPAIR_OFF"),
            };
            emit_pair_mem(&mut ops, word, load, width, signed, false, false);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDST_UNSCALED(inner) => {
            use vane_arch::MemWidth::{B1, B2, B4, B8};
            let (word, load, width, signed) = match inner {
                LDST_UNSCALED::LDUR_Rt_ADDR_SIMM9(v) => {
                    (v.0, true, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_UNSCALED::STUR_Rt_ADDR_SIMM9(v) => {
                    (v.0, false, if v.0 >> 30 == 0b11 { B8 } else { B4 }, false)
                }
                LDST_UNSCALED::LDURB_Rt_ADDR_SIMM9(v) => (v.0, true, B1, false),
                LDST_UNSCALED::STURB_Rt_ADDR_SIMM9(v) => (v.0, false, B1, false),
                LDST_UNSCALED::LDURH_Rt_ADDR_SIMM9(v) => (v.0, true, B2, false),
                LDST_UNSCALED::STURH_Rt_ADDR_SIMM9(v) => (v.0, false, B2, false),
                LDST_UNSCALED::LDURSB_Rt_ADDR_SIMM9(v) => (v.0, true, B1, true),
                LDST_UNSCALED::LDURSH_Rt_ADDR_SIMM9(v) => (v.0, true, B2, true),
                LDST_UNSCALED::LDURSW_Rt_ADDR_SIMM9(v) => (v.0, true, B4, true),
                _ => return unsupported(&mut ops, word, "LDST_UNSCALED"),
            };
            emit_signed_mem(&mut ops, word, load, width, signed);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LDST_POS(inner) => {
            use vane_arch::MemWidth::{B1, B2, B4, B8};
            let (word, load, width, signed, scale) = match inner {
                LDST_POS::LDR_Rt_ADDR_UIMM12(v) => {
                    let width = if v.0 >> 30 == 0b11 { B8 } else { B4 };
                    (v.0, true, width, false, if width == B8 { 8 } else { 4 })
                }
                LDST_POS::STR_Rt_ADDR_UIMM12(v) => {
                    let width = if v.0 >> 30 == 0b11 { B8 } else { B4 };
                    (v.0, false, width, false, if width == B8 { 8 } else { 4 })
                }
                LDST_POS::LDRB_Rt_ADDR_UIMM12(v) => (v.0, true, B1, false, 1),
                LDST_POS::STRB_Rt_ADDR_UIMM12(v) => (v.0, false, B1, false, 1),
                LDST_POS::LDRH_Rt_ADDR_UIMM12(v) => (v.0, true, B2, false, 2),
                LDST_POS::STRH_Rt_ADDR_UIMM12(v) => (v.0, false, B2, false, 2),
                LDST_POS::LDRSB_Rt_ADDR_UIMM12(v) => (v.0, true, B1, true, 1),
                LDST_POS::LDRSH_Rt_ADDR_UIMM12(v) => (v.0, true, B2, true, 2),
                LDST_POS::LDRSW_Rt_ADDR_UIMM12(v) => (v.0, true, B4, true, 4),
                _ => return unsupported(&mut ops, word, "LDST_POS"),
            };
            emit_pos_mem(&mut ops, word, load, width, signed, scale);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::BITFIELD(inner) => {
            let (word, kind) = match inner {
                BITFIELD::UBFM_Rd_Rn_IMMR_IMMS(v) => (v.0, 0),
                BITFIELD::SBFM_Rd_Rn_IMMR_IMMS(v) => (v.0, 1),
                BITFIELD::BFM_Rd_Rn_IMMR_IMMS(v) => (v.0, 2),
            };
            emit_bitfield(&mut ops, word, kind);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::LOG_IMM(inner) => {
            let (word, kind, set_flags) = match inner {
                LOG_IMM::AND_Rd_SP_Rn_LIMM(v) => (v.0, 0, false),
                LOG_IMM::ANDS_Rd_Rn_LIMM(v) => (v.0, 0, true),
                LOG_IMM::ORR_Rd_SP_Rn_LIMM(v) => (v.0, 1, false),
                LOG_IMM::EOR_Rd_SP_Rn_LIMM(v) => (v.0, 2, false),
            };
            emit_logical_imm(&mut ops, word, kind, set_flags);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::EXCEPTION(EXCEPTION::BRK_EXCEPTION(v)) => {
            ops.push(StackOp::Trap(alloc::format!(
                "aarch64: brk #{:x}",
                (v.0 >> 5) & 0xffff
            )));
        }
        Operation::EXCEPTION(EXCEPTION::SVC_EXCEPTION(_)) => {
            // The embedding chooses the ABI (Linux `x8` or Darwin `x16`)
            // when it dispatches this boundary. `svc` itself always falls
            // through after a non-terminal syscall.
            ops.push(StackOp::Ecall);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::ADDSUB_EXT(inner) => {
            let (is_sub, set_flags, word) = match inner {
                ADDSUB_EXT::ADD_Rd_SP_Rn_SP_Rm_EXT(v) => (false, false, v.0),
                ADDSUB_EXT::ADDS_Rd_Rn_SP_Rm_EXT(v) => (false, true, v.0),
                ADDSUB_EXT::SUB_Rd_SP_Rn_SP_Rm_EXT(v) => (true, false, v.0),
                ADDSUB_EXT::SUBS_Rd_Rn_SP_Rm_EXT(v) => (true, true, v.0),
            };
            emit_addsub_ext(&mut ops, word, is_sub, set_flags);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::DP_2SRC(inner) => {
            let (word, kind) = match inner {
                DP_2SRC::UDIV_Rd_Rn_Rm(v) => (v.0, 0),
                DP_2SRC::SDIV_Rd_Rn_Rm(v) => (v.0, 1),
                DP_2SRC::LSLV_Rd_Rn_Rm(v) => (v.0, 2),
                DP_2SRC::LSRV_Rd_Rn_Rm(v) => (v.0, 3),
                DP_2SRC::ASRV_Rd_Rn_Rm(v) => (v.0, 4),
                DP_2SRC::RORV_Rd_Rn_Rm(v) => (v.0, 5),
                _ => return unsupported(&mut ops, word, "DP_2SRC"),
            };
            emit_dp2(&mut ops, word, kind);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::DP_3SRC(inner) => {
            let (word, kind) = match inner {
                DP_3SRC::MADD_Rd_Rn_Rm_Ra(v) => (v.0, 0),
                DP_3SRC::MSUB_Rd_Rn_Rm_Ra(v) => (v.0, 1),
                DP_3SRC::SMADDL_Rd_Rn_Rm_Ra(v) => (v.0, 2),
                DP_3SRC::SMSUBL_Rd_Rn_Rm_Ra(v) => (v.0, 3),
                DP_3SRC::UMADDL_Rd_Rn_Rm_Ra(v) => (v.0, 4),
                DP_3SRC::UMSUBL_Rd_Rn_Rm_Ra(v) => (v.0, 5),
                DP_3SRC::SMULH_Rd_Rn_Rm(v) => (v.0, 6),
                DP_3SRC::UMULH_Rd_Rn_Rm(v) => (v.0, 7),
            };
            emit_dp3(&mut ops, word, kind);
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::ADDSUB_SHIFT(inner) => {
            let (is_sub, set_flags, word) = match inner {
                ADDSUB_SHIFT::ADD_Rd_Rn_Rm_SFT(v) => (false, false, v.0),
                ADDSUB_SHIFT::ADDS_Rd_Rn_Rm_SFT(v) => (false, true, v.0),
                ADDSUB_SHIFT::SUB_Rd_Rn_Rm_SFT(v) => (true, false, v.0),
                ADDSUB_SHIFT::SUBS_Rd_Rn_Rm_SFT(v) => (true, true, v.0),
            };
            emit_addsub(
                &mut ops,
                word,
                is_sub,
                false,
                set_flags,
                (word >> 22 & 3) as u8,
                ((word >> 10) & 0x3f) as u8,
            );
            emit_to(&mut ops, this, next, labels, depth);
        }
        Operation::COMPBRANCH(inner) => {
            let (non_zero, word) = match inner {
                COMPBRANCH::CBZ_Rt_ADDR_PCREL19(v) => (false, v.0),
                COMPBRANCH::CBNZ_Rt_ADDR_PCREL19(v) => (true, v.0),
            };
            read_gpr(&mut ops, rd(word), false);
            ops.push(StackOp::PushImm(0));
            ops.push(if non_zero {
                StackOp::Ne64
            } else {
                StackOp::Eq64
            });
            let target = this
                .pc
                .wrapping_add_signed(signed_bits((word >> 5) & 0x7ffff, 19) * 4);
            branch(&mut ops, this, target, next, labels, depth);
        }
        Operation::CONDBRANCH(inner) => {
            let word = match inner {
                CONDBRANCH::B__ADDR_PCREL19(v) => v.0,
                CONDBRANCH::BC__ADDR_PCREL19(v) => v.0,
            };
            if !condition_test(&mut ops, (word & 0xf) as u8) {
                return unsupported(&mut ops, word, "B.cond condition pending NZCV lowering");
            }
            let target = this
                .pc
                .wrapping_add_signed(signed_bits((word >> 5) & 0x7ffff, 19) * 4);
            branch(&mut ops, this, target, next, labels, depth);
        }
        Operation::BRANCH_IMM(inner) => {
            let (is_link, word) = match inner {
                BRANCH_IMM::B_ADDR_PCREL26(v) => (false, v.0),
                BRANCH_IMM::BL_ADDR_PCREL26(v) => (true, v.0),
            };
            if is_link {
                ops.push(StackOp::PushImm(next));
                write_gpr(&mut ops, 30, false);
            }
            let signed = ((word & 0x03ff_ffff) as i32) << 6 >> 4;
            let target = this.pc.wrapping_add_signed(signed as i64);
            emit_to(&mut ops, this, target, labels, depth);
        }
        Operation::BRANCH_REG(inner) => {
            let (link, word) = match inner {
                BRANCH_REG::BR_Rn(v) => (false, v.0),
                BRANCH_REG::BLR_Rn(v) => (true, v.0),
                BRANCH_REG::RET_Rn(v) => (false, v.0),
                _ => return unsupported(&mut ops, word, "BRANCH_REG"),
            };
            if link {
                ops.push(StackOp::PushImm(next));
                write_gpr(&mut ops, 30, false);
            }
            read_gpr(&mut ops, rn(word), false);
            ops.push(StackOp::TailCall);
        }
        _ => return unsupported(&mut ops, word, "unimplemented disarm64 operation"),
    }
    ops
}

fn unsupported(ops: &mut Vec<StackOp>, word: u32, family: &str) -> Vec<StackOp> {
    ops.push(StackOp::Trap(alloc::format!(
        "aarch64: unsupported {family} {word:08x}"
    )));
    core::mem::take(ops)
}

vane_arch::cfg_target_wasm! {
    impl crate::Aarch64WasmJit for TemplateJit<'_> {
        fn Aarch64<'a>(
            &'a self,
            ctx: &'a (dyn vane_arch::WasmJitCtx + 'a),
        ) -> impl Iterator<Item = vane_arch::JitOpcode<'a>> + 'a {
            use vane_arch::template::render_stack_to_wasm;
            // See vane-riscv's `RiscvWasmJit::Riscv` for why this trailing,
            // unnested `unreachable` is needed: `a64_emit` wraps every trace
            // in one outer `LoopBegin`/`LoopEnd`, terminating via a nested
            // `TailCall`, and WASM doesn't propagate "unreachable" across a
            // loop boundary, so the function's implicit final return would
            // otherwise go unsatisfied despite being genuinely unreachable.
            render_stack_to_wasm(a64_emit(self).into_iter(), ctx.wasm_abi())
                .chain(core::iter::once(vane_arch::JitOpcode::Operator { op: wasmparser::Operator::Unreachable }))
        }
    }
}

vane_arch::cfg_target_js! {
    impl Aarch64Display for TemplateJit<'_> {
        fn Aarch64(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
            use vane_arch::template::render_stack_to_js;
            render_stack_to_js(a64_emit(self).into_iter(), f, self.params.flate, self.params.flags)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateSlot;
    use alloc::collections::BTreeMap;
    use vane_arch::{
        Mem,
        interp::{Exit, StackHost, interpret},
    };

    #[test]
    fn disarm64_decodes_parity_smoke_words() {
        // movz x0,#1; add x1,x0,#2; b .
        for word in [0xd280_0020, 0x9100_0801, 0x1400_0000] {
            assert!(disarm64::decoder::decode(word).is_some(), "{word:08x}");
        }
    }

    #[test]
    fn bitfield_extract_sign_extend_and_insert_execute() {
        // ubfm x2,x0,#8,#15; sbfm x4,x3,#0,#7; bfm x6,x5,#8,#15; br x30.
        let words = [0xd348_3c02u32, 0x9340_1c64, 0xb348_3ca6, 0xd61f_03c0];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(0), 0x1234);
        host.state.insert(state::x(3), 0x80);
        host.state.insert(state::x(5), 0xabcd);
        host.state.insert(state::x(6), 0xffff_0000);
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state[&state::x(2)], 0x12);
        assert_eq!(host.state[&state::x(4)], u64::MAX - 127);
        assert_eq!(host.state[&state::x(6)], 0xffff_00ab);
    }

    #[test]
    fn logical_immediate_and_brk_execute() {
        // and x2,x0,#0xff; ands w4,w3,#0xf; brk #0x1234.
        let words = [0x9240_1c02u32, 0x7200_0c64, 0xd422_4680];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(0), 0x1234);
        host.state.insert(state::x(3), 0x1f);
        assert!(
            matches!(interpret(&a64_emit(&jit), &mut host), Exit::Trap(message) if message == "aarch64: brk #1234")
        );
        assert_eq!(host.state[&state::x(2)], 0x34);
        assert_eq!(host.state[&state::x(4)], 0xf);
        assert_eq!(host.state[&state::nzcv('n')], 0);
        assert_eq!(host.state[&state::nzcv('z')], 0);
    }

    #[test]
    fn extended_add_multiply_divide_shift_and_high_multiply_execute() {
        // add x2,x0,w1,uxtw #2; sdiv x5,x3,x4; lslv x8,x6,x7;
        // rorv x11,x9,x10; madd x15,x12,x13,x14; smaddl x19,w16,w17,x18;
        // umulh x22,x20,x21; br x30.
        let words = [
            0x8b21_4802u32,
            0x9ac4_0c65,
            0x9ac7_20c8,
            0x9aca_2d2b,
            0x9b0d_398f,
            0x9b31_4a13,
            0x9bd5_7e96,
            0xd61f_03c0,
        ];
        let mut code = Mem::default();
        let mut host_code = BTreeMap::new();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                let addr = (offset * 4 + i) as u64;
                code.write_byte(addr, byte);
                host_code.insert(addr, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: host_code,
        };
        for (reg, value) in [
            (0, 10),
            (1, 3),
            (3, (-21i64) as u64),
            (4, 4),
            (6, 3),
            (7, 5),
            (9, 0x8000_0000_0000_0001),
            (10, 1),
            (12, 2),
            (13, 4),
            (14, 3),
            (16, (-2i64) as u64),
            (17, 3),
            (18, 20),
            (20, u64::MAX),
            (21, 2),
            (30, 0xfeed),
        ] {
            host.state.insert(state::x(reg), value);
        }
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state[&state::x(2)], 22);
        assert_eq!(host.state[&state::x(5)], (-5i64) as u64);
        assert_eq!(host.state[&state::x(8)], 96);
        assert_eq!(host.state[&state::x(11)], 0xc000_0000_0000_0000);
        assert_eq!(host.state[&state::x(15)], 11);
        assert_eq!(host.state[&state::x(19)], 14);
        assert_eq!(host.state[&state::x(22)], 1);
    }

    #[test]
    fn scalar_fp_binary_and_unary_execute() {
        // fadd d0,d1,d2 ; fadd s3,s4,s5 ; fneg d6,d7 ; fsqrt s8,s9 ; br x30
        let words = [
            0x1e62_2820u32,
            0x1e25_2883,
            0x1e61_40e6,
            0x1e21_c128,
            0xd61f_03c0,
        ];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::v(1), 1.5f64.to_bits());
        host.state.insert(state::v(2), 2.25f64.to_bits());
        host.state.insert(state::v(4), 1.25f32.to_bits() as u64);
        host.state.insert(state::v(5), 2.5f32.to_bits() as u64);
        host.state.insert(state::v(7), 3.0f64.to_bits());
        host.state.insert(state::v(9), 9.0f32.to_bits() as u64);
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state.get(&state::v(0)), Some(&3.75f64.to_bits()));
        assert_eq!(
            host.state.get(&state::v(3)),
            Some(&(3.75f32.to_bits() as u64))
        );
        assert_eq!(host.state.get(&state::v(6)), Some(&(-3.0f64).to_bits()));
        assert_eq!(
            host.state.get(&state::v(8)),
            Some(&(3.0f32.to_bits() as u64))
        );
    }

    #[test]
    fn scalar_fp_compare_select_fma_immediate_and_conversions_execute() {
        // fcmp d0,d1; fcsel d2,d3,d4,eq; fmadd d5,d6,d7,d8; fmov s9,#1;
        // fmov x10,d11; fmov d12,x13; scvtf d14,x15; fcvtzs x16,d17; br x30.
        let words = [
            0x1e61_2000u32,
            0x1e64_0c62,
            0x1f47_20c5,
            0x1e2e_1009,
            0x9e66_016a,
            0x9e67_01ac,
            0x9e62_01ee,
            0x9e78_0230,
            0xd61f_03c0,
        ];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::v(0), 4.0f64.to_bits());
        host.state.insert(state::v(1), 4.0f64.to_bits());
        host.state.insert(state::v(3), 7.0f64.to_bits());
        host.state.insert(state::v(4), 9.0f64.to_bits());
        host.state.insert(state::v(6), 2.0f64.to_bits());
        host.state.insert(state::v(7), 3.0f64.to_bits());
        host.state.insert(state::v(8), 5.0f64.to_bits());
        host.state.insert(state::v(11), 0x1122_3344_5566_7788);
        host.state.insert(state::x(13), 0x8877_6655_4433_2211);
        host.state.insert(state::x(15), (-12i64) as u64);
        host.state.insert(state::v(17), 42.75f64.to_bits());
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state[&state::v(2)], 7.0f64.to_bits());
        assert_eq!(host.state[&state::v(5)], 11.0f64.to_bits());
        assert_eq!(host.state[&state::v(9)], 1.0f32.to_bits() as u64);
        assert_eq!(host.state[&state::x(10)], 0x1122_3344_5566_7788);
        assert_eq!(host.state[&state::v(12)], 0x8877_6655_4433_2211);
        assert_eq!(host.state[&state::v(14)], (-12.0f64).to_bits());
        assert_eq!(host.state[&state::x(16)], 42);
    }

    #[test]
    fn conditions_cover_all_nzcv_forms() {
        let cases: &[(u8, &[(char, u64)], bool)] = &[
            (0, &[('z', 1)], true),                      // EQ
            (1, &[('z', 0)], true),                      // NE
            (2, &[('c', 1)], true),                      // CS
            (3, &[('c', 0)], true),                      // CC
            (8, &[('c', 1), ('z', 0)], true),            // HI
            (9, &[('c', 0), ('z', 0)], true),            // LS
            (10, &[('n', 1), ('v', 1)], true),           // GE
            (11, &[('n', 1), ('v', 0)], true),           // LT
            (12, &[('z', 0), ('n', 1), ('v', 1)], true), // GT
            (13, &[('z', 1), ('n', 1), ('v', 1)], true), // LE
            (14, &[], true),                             // AL
        ];
        for &(cond, flags, expected) in cases {
            let mut ops = Vec::new();
            assert!(condition_test(&mut ops, cond));
            ops.push(StackOp::IfBegin);
            ops.push(StackOp::PushImm(1));
            ops.push(StackOp::StoreState(StateSlot::new("result")));
            ops.push(StackOp::IfElse);
            ops.push(StackOp::IfEnd);
            ops.push(StackOp::PushImm(0));
            ops.push(StackOp::TailCall);
            struct Host(BTreeMap<StateSlot, u64>);
            impl StackHost for Host {
                fn load_reg(&mut self, _r: u8) -> u64 {
                    0
                }
                fn store_reg(&mut self, _r: u8, _v: u64) {}
                fn load_state(&mut self, slot: &StateSlot) -> u64 {
                    self.0.get(slot).copied().unwrap_or(0)
                }
                fn store_state(&mut self, slot: &StateSlot, value: u64) {
                    self.0.insert(slot.clone(), value);
                }
                fn load_mem(
                    &mut self,
                    _addr: u64,
                    _width: vane_arch::MemWidth,
                    _signed: bool,
                ) -> u64 {
                    0
                }
                fn store_mem(&mut self, _addr: u64, _width: vane_arch::MemWidth, _value: u64) {}
                fn ecall(&mut self) -> bool {
                    false
                }
            }
            let mut host = Host(BTreeMap::new());
            for &(flag, value) in flags {
                host.0.insert(state::nzcv(flag), value);
            }
            let _ = interpret(&ops, &mut host);
            assert_eq!(
                host.0.get(&StateSlot::new("result")).copied().unwrap_or(0) != 0,
                expected,
                "condition {cond}"
            );
        }
    }

    #[test]
    fn cbz_selects_the_taken_trace() {
        // cbz x0,+8 ; br x30 ; br x30. X0 starts as zero, so the first
        // indirect branch must be skipped and the target at PC 8 selected.
        let words = [0xb400_0040u32, 0xd61f_03c0, 0xd61f_03c0];
        let mut mem = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                mem.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let mem = Box::leak(Box::new(mem));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: mem,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _r: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _r: u8, _v: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, addr: u64, _width: vane_arch::MemWidth, _signed: bool) -> u64 {
                self.code.get(&addr).copied().unwrap_or(0) as u64
            }
            fn store_mem(&mut self, _addr: u64, _width: vane_arch::MemWidth, _value: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
    }

    #[test]
    fn subs_sets_all_nzcv_bits() {
        // movz x0,#0 ; subs x1,x0,#1 ; br x30
        let words = [0xd280_0000u32, 0xf100_0401, 0xd61f_03c0];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _r: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _r: u8, _v: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, addr: u64, _width: vane_arch::MemWidth, _signed: bool) -> u64 {
                self.code.get(&addr).copied().unwrap_or(0) as u64
            }
            fn store_mem(&mut self, _addr: u64, _width: vane_arch::MemWidth, _value: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state.get(&state::x(1)), Some(&u64::MAX));
        assert_eq!(host.state.get(&state::nzcv('n')), Some(&1));
        assert_eq!(host.state.get(&state::nzcv('z')), Some(&0));
        assert_eq!(host.state.get(&state::nzcv('c')), Some(&0));
        assert_eq!(host.state.get(&state::nzcv('v')), Some(&0));
    }

    #[test]
    fn shifted_register_add_executes() {
        // movz x0,#3 ; movz x1,#5 ; add x2,x0,x1,lsl #2 ; br x30
        let words = [0xd280_0060u32, 0xd280_00a1, 0x8b01_0802, 0xd61f_03c0];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _r: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _r: u8, _v: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, addr: u64, _width: vane_arch::MemWidth, _signed: bool) -> u64 {
                self.code.get(&addr).copied().unwrap_or(0) as u64
            }
            fn store_mem(&mut self, _addr: u64, _width: vane_arch::MemWidth, _value: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state.get(&state::x(2)), Some(&23));
    }

    #[test]
    fn integer_memory_addressing_forms_execute() {
        // LDUR, STUR, register-offset LDR, and pair pre/post-index forms.
        let words = [
            0xf85f_8020u32, // ldur x0, [x1, #-8]
            0xb800_4022,    // stur w2, [x1, #4]
            0xf862_7823,    // ldr x3, [x1, x2, lsl #3]
            0x2941_1825,    // ldp w5, w6, [x1, #8]
            0x2881_2027,    // stp w7, w8, [x1], #8
            0xd61f_03c0,    // br x30
        ];
        let mut code = Mem::default();
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                code.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let code = Box::leak(Box::new(code));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: code,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            mem: BTreeMap<u64, u8>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _r: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _r: u8, _v: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, addr: u64, width: vane_arch::MemWidth, signed: bool) -> u64 {
                let bytes = match width {
                    vane_arch::MemWidth::B1 => 1,
                    vane_arch::MemWidth::B2 => 2,
                    vane_arch::MemWidth::B4 => 4,
                    vane_arch::MemWidth::B8 => 8,
                };
                let mut value = 0u64;
                for i in 0..bytes {
                    value |= (self.mem.get(&(addr + i)).copied().unwrap_or(0) as u64) << (i * 8);
                }
                if signed {
                    match bytes {
                        1 => value as u8 as i8 as i64 as u64,
                        2 => value as u16 as i16 as i64 as u64,
                        4 => value as u32 as i32 as i64 as u64,
                        _ => value,
                    }
                } else {
                    value
                }
            }
            fn store_mem(&mut self, addr: u64, width: vane_arch::MemWidth, value: u64) {
                let bytes = match width {
                    vane_arch::MemWidth::B1 => 1,
                    vane_arch::MemWidth::B2 => 2,
                    vane_arch::MemWidth::B4 => 4,
                    vane_arch::MemWidth::B8 => 8,
                };
                for i in 0..bytes {
                    self.mem.insert(addr + i, (value >> (i * 8)) as u8);
                }
            }
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            mem: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in words.into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(1), 0x100);
        host.state.insert(state::x(2), 1);
        host.state.insert(state::x(7), 0xaabb_ccdd);
        host.state.insert(state::x(8), 0x1122_3344);
        host.state.insert(state::x(30), 0xfeed);
        for (addr, byte) in 0x1122_3344_5566_7788u64
            .to_le_bytes()
            .into_iter()
            .enumerate()
        {
            host.mem.insert(0xf8 + addr as u64, byte);
        }
        for (addr, byte) in 0x8877_6655u32.to_le_bytes().into_iter().enumerate() {
            host.mem.insert(0x108 + addr as u64, byte);
        }
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state.get(&state::x(0)), Some(&0x1122_3344_5566_7788));
        assert_eq!(host.state.get(&state::x(3)), Some(&0x8877_6655));
        assert_eq!(host.state.get(&state::x(5)), Some(&0x8877_6655));
        assert_eq!(host.state.get(&state::x(6)), Some(&0));
        assert_eq!(host.state.get(&state::x(1)), Some(&0x108));
        assert_eq!(
            host.load_mem(0x104, vane_arch::MemWidth::B4, false),
            0x1122_3344
        );
        assert_eq!(
            host.load_mem(0x100, vane_arch::MemWidth::B4, false),
            0xaabb_ccdd
        );
        assert_eq!(
            host.load_mem(0x104, vane_arch::MemWidth::B4, false),
            0x1122_3344
        );
    }

    #[test]
    fn movz_and_add_execute_with_writable_x0() {
        // movz x0,#40 ; add x1,x0,#2 ; br x30
        let mut mem = Mem::default();
        for (offset, word) in [0xd280_0500u32, 0x9100_0801, 0xd61f_03c0]
            .into_iter()
            .enumerate()
        {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                mem.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        let mem = Box::leak(Box::new(mem));
        let jit = TemplateJit {
            params: vane_arch::template::Params {
                react: mem,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: 0,
                flags: vane_arch::template::Flags::default(),
            },
            pc: 0,
            labels: Default::default(),
            depth: 0,
        };
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _r: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _r: u8, _v: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.state.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.state.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, addr: u64, _width: vane_arch::MemWidth, _signed: bool) -> u64 {
                self.code.get(&addr).copied().unwrap_or(0) as u64
            }
            fn store_mem(&mut self, _addr: u64, _width: vane_arch::MemWidth, _value: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: BTreeMap::new(),
        };
        for (offset, word) in [0xd280_0500u32, 0x9100_0801, 0xd61f_03c0]
            .into_iter()
            .enumerate()
        {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                host.code.insert((offset * 4 + i) as u64, byte);
            }
        }
        host.state.insert(state::x(30), 0xfeed);
        assert!(matches!(
            interpret(&a64_emit(&jit), &mut host),
            Exit::TailCall(0xfeed)
        ));
        assert_eq!(host.state.get(&state::x(0)), Some(&40));
        assert_eq!(host.state.get(&state::x(1)), Some(&42));
    }
}

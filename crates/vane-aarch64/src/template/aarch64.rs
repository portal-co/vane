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
    ADDSUB_IMM, ADDSUB_SHIFT, BRANCH_IMM, BRANCH_REG, COMPBRANCH, CONDBRANCH, CONDSEL, IC_SYSTEM,
    LDST_POS, LOG_SHIFT, MOVEWIDE, Operation, PCRELADDR,
};
use vane_arch::template::TemplateJit;
use vane_arch::{Heat, StackOp};

use crate::{Aarch64Display, StateSlot, state};

fn word_at(this: &TemplateJit<'_>) -> u32 {
    u32::from_le_bytes(array::from_fn(|i| {
        this.params.react.byte(this.pc + i as u64)
    }))
}

#[inline]
fn rd(word: u32) -> u8 {
    (word & 0x1f) as u8
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
        emit_result_and_flags(ops, rd(word), allow_sp, is_32bit, is_sub);
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
        emit_result_and_flags(ops, rd(word), allow_sp, is_32bit, is_sub);
    } else {
        if is_32bit {
            ops.push(StackOp::Trunc32);
        }
        write_gpr(ops, rd(word), allow_sp);
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

fn a64_emit(this: &TemplateJit<'_>) -> Vec<StackOp> {
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
                LDST_POS::LDRSW_Rt_ADDR_UIMM12(v) => (v.0, true, B4, true, 4),
                _ => return unsupported(&mut ops, word, "LDST_POS"),
            };
            emit_pos_mem(&mut ops, word, load, width, signed, scale);
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
            _ctx: &'a (dyn vane_arch::WasmJitCtx + 'a),
        ) -> impl Iterator<Item = vane_arch::JitOpcode<'a>> + 'a {
            use vane_arch::template::render_stack_to_wasm;
            render_stack_to_wasm(a64_emit(self).into_iter())
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

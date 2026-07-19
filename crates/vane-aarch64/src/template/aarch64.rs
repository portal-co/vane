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
    ADDSUB_IMM, ADDSUB_SHIFT, BRANCH_IMM, BRANCH_REG, COMPBRANCH, CONDBRANCH, LDST_POS, MOVEWIDE,
    Operation,
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

fn condition_test(ops: &mut Vec<StackOp>, cond: u8) -> bool {
    // Conditions are defined over individual NZCV bits. The StackOp integer
    // subset cannot synthesize boolean OR yet, so support the common Z and
    // NZ tests now; the remaining conditions stay explicit unsupported traps.
    match cond {
        0 => {
            ops.push(StackOp::LoadState(state::nzcv('z')));
            true
        } // EQ
        1 => {
            ops.push(StackOp::LoadState(state::nzcv('z')));
            ops.push(StackOp::PushImm(0));
            ops.push(StackOp::Eq64);
            true
        } // NE
        _ => false,
    }
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

fn emit_addsub(ops: &mut Vec<StackOp>, word: u32, is_sub: bool, allow_sp: bool) {
    read_gpr(ops, rn(word), allow_sp);
    read_gpr(ops, ((word >> 16) & 0x1f) as u8, false);
    ops.push(if is_sub {
        StackOp::Sub64
    } else {
        StackOp::Add64
    });
    if word >> 31 == 0 {
        ops.push(StackOp::Trunc32);
    }
    write_gpr(ops, rd(word), allow_sp);
}

fn emit_addsub_imm(ops: &mut Vec<StackOp>, word: u32, is_sub: bool, allow_sp: bool) {
    let imm = (((word >> 10) & 0xfff) as u64) << if word & (1 << 22) != 0 { 12 } else { 0 };
    read_gpr(ops, rn(word), allow_sp);
    ops.push(StackOp::PushImm(imm));
    ops.push(if is_sub {
        StackOp::Sub64
    } else {
        StackOp::Add64
    });
    if word >> 31 == 0 {
        ops.push(StackOp::Trunc32);
    }
    write_gpr(ops, rd(word), allow_sp);
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
            if set_flags {
                return unsupported(&mut ops, word, "ADDSUB_IMM flags pending NZCV lowering");
            }
            emit_addsub_imm(&mut ops, word, is_sub, true);
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
            let (is_sub, word) = match inner {
                ADDSUB_SHIFT::ADD_Rd_Rn_Rm_SFT(v) => (false, v.0),
                ADDSUB_SHIFT::ADDS_Rd_Rn_Rm_SFT(v) => (false, v.0),
                ADDSUB_SHIFT::SUB_Rd_Rn_Rm_SFT(v) => (true, v.0),
                ADDSUB_SHIFT::SUBS_Rd_Rn_Rm_SFT(v) => (true, v.0),
            };
            // Shifted-register variants with flag updates need NZCV lowering;
            // the initial subset accepts only LSL #0 semantics.
            if word & ((1 << 29) | (0b11 << 22) | (0b11_1111 << 10)) != 0 {
                return unsupported(&mut ops, word, "ADDSUB_SHIFT flags/shift pending");
            }
            emit_addsub(&mut ops, word, is_sub, false);
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

use core::array;
use core::fmt::Formatter;

use alloc::boxed::Box;
use rv_asm::{Inst, IsCompressed, Xlen};
use vane_arch::{FloatBinOp, FloatUnaryOp, FloatWidth, Heat, MemWidth, StackOp};

use crate::{RiscvDisplay, hint, state};
vane_arch::cfg_target_wasm! { use crate::RiscvWasmJit; }
vane_arch::cfg_target_js! { use vane_arch::template::render_stack_to_js; }
vane_arch::cfg_target_wasm! { use vane_arch::template::render_stack_to_wasm; }

use super::*;

#[cfg(test)]
mod riscv_fp_tests;

// ── StackOp producer ─────────────────────────────────────────────────────────
//
// `rv_emit` is the single entry point.  It checks the JIT cache first (emitting
// a `TailCall` for already-compiled blocks), then delegates to `jit_core` for
// the loop/label bookkeeping and calls `rv_body` to generate the per-instruction
// StackOps plus the recursive fallthrough / branch targets.

fn rv_float_ops(word: u32) -> Option<alloc::vec::Vec<StackOp>> {
    let rd = ((word >> 7) & 0x1f) as u8;
    let rs1 = ((word >> 15) & 0x1f) as u8;
    let rs2 = ((word >> 20) & 0x1f) as u8;
    let funct3 = (word >> 12) & 7;
    let funct7 = (word >> 25) & 0x7f;
    let width = |double: bool| {
        if double {
            FloatWidth::F64
        } else {
            FloatWidth::F32
        }
    };
    let mut ops = alloc::vec::Vec::new();
    match word & 0x7f {
        // FLW/FLD and FSW/FSD use the normal I/S immediate encodings but
        // maintain raw IEEE bits in explicit F-register state slots.
        0x07 if funct3 == 2 || funct3 == 3 => {
            let imm = (word as i32) >> 20;
            ops.push(StackOp::LoadReg(rs1));
            ops.push(StackOp::PushImm(imm as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: if funct3 == 2 {
                    MemWidth::B4
                } else {
                    MemWidth::B8
                },
                signed: false,
            });
            ops.push(StackOp::StoreState(state::f(rd)));
        }
        0x27 if funct3 == 2 || funct3 == 3 => {
            let imm = (((word >> 25) << 5) | ((word >> 7) & 0x1f)) as i32;
            let imm = (imm << 20) >> 20;
            ops.push(StackOp::LoadReg(rs1));
            ops.push(StackOp::PushImm(imm as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadState(state::f(rs2)));
            ops.push(StackOp::StoreMem {
                width: if funct3 == 2 {
                    MemWidth::B4
                } else {
                    MemWidth::B8
                },
            });
        }
        0x53 => {
            let (bin, double) = match funct7 {
                0x00 => (Some(FloatBinOp::Add), false),
                0x01 => (Some(FloatBinOp::Add), true),
                0x04 => (Some(FloatBinOp::Sub), false),
                0x05 => (Some(FloatBinOp::Sub), true),
                0x08 => (Some(FloatBinOp::Mul), false),
                0x09 => (Some(FloatBinOp::Mul), true),
                0x0c => (Some(FloatBinOp::Div), false),
                0x0d => (Some(FloatBinOp::Div), true),
                _ => (None, false),
            };
            if let Some(bin) = bin {
                ops.push(StackOp::LoadState(state::f(rs1)));
                ops.push(StackOp::LoadState(state::f(rs2)));
                ops.push(StackOp::FloatBinary {
                    width: width(double),
                    op: bin,
                });
                ops.push(StackOp::StoreState(state::f(rd)));
            } else if (funct7 == 0x2c || funct7 == 0x2d) && rs2 == 0 {
                ops.push(StackOp::LoadState(state::f(rs1)));
                ops.push(StackOp::FloatUnary {
                    width: width(funct7 == 0x2d),
                    op: FloatUnaryOp::Sqrt,
                });
                ops.push(StackOp::StoreState(state::f(rd)));
            } else {
                return None;
            }
        }
        _ => return None,
    }
    Some(ops)
}

fn rv_emit(this: &TemplateJit<'_>) -> alloc::vec::Vec<StackOp> {
    // If this PC is already compiled, jump straight to the cached block.
    if (this.params.trial)(this.pc) == Heat::Cached {
        return alloc::vec![StackOp::PushImm(this.pc), StackOp::TailCall];
    }

    // Open (or back-branch to) the loop for this PC via the label map.
    let mut labels = this.labels.clone();
    let mut ops = alloc::vec::Vec::new();

    match labels.0.entry(this.pc) {
        alloc::collections::btree_map::Entry::Vacant(e) => {
            let label = alloc::format!("x{}", this.pc);
            e.insert(Label {
                ident_name: label.clone(),
                index: this.depth,
            });
            let nd = this.depth + 1;
            ops.push(StackOp::LoopBegin { label });
            ops.extend(rv_body(this, labels, nd));
            ops.push(StackOp::LoopEnd);
        }
        alloc::collections::btree_map::Entry::Occupied(e) => {
            let lbl: Label = e.get().clone();
            let relative_depth = this.depth - lbl.index;
            ops.push(StackOp::Br {
                relative_depth,
                label: lbl.ident_name,
            });
        }
    }
    ops
}

/// Emit a helper to jump to `pc` using the current labels/depth.
#[inline]
fn emit_to(
    ops: &mut alloc::vec::Vec<StackOp>,
    params: Params<'_>,
    pc: u64,
    labels: Labels,
    depth: u32,
) {
    let jit = TemplateJit {
        pc,
        labels,
        depth,
        params,
    };
    ops.extend(rv_emit(&jit));
}

fn rv_body(this: &TemplateJit<'_>, labels: Labels, nd: u32) -> alloc::vec::Vec<StackOp> {
    let mut ops: alloc::vec::Vec<StackOp> = alloc::vec::Vec::new();

    // ── Instruction fetch & integrity check ──────────────────────────────────
    let inst_bytes: [u8; 4] = array::from_fn(|i| this.params.react.byte(this.pc + i as u64));
    ops.push(StackOp::CheckCode {
        addr: this.pc,
        expected: Box::from(inst_bytes),
        miss_label: alloc::format!("{}", this.params.root),
    });

    let inst_code = u32::from_le_bytes(inst_bytes);
    if let Some(mut fp_ops) = rv_float_ops(inst_code) {
        ops.append(&mut fp_ops);
        emit_to(&mut ops, this.params, this.pc + 4, labels, nd);
        return ops;
    }
    let (inst, compressed) = match Inst::decode(inst_code, Xlen::Rv64) {
        Err(e) => {
            ops.push(StackOp::Trap(alloc::format!("decoding: {e}")));
            return ops;
        }
        Ok(v) => v,
    };

    let next_pc = this.pc
        + match compressed {
            IsCompressed::Yes => 2,
            IsCompressed::No => 4,
        };

    // ── Hint / test-mode logging ──────────────────────────────────────────────
    if this.params.flags.test_mode {
        if let Some(hint_value) = hint::detect_test_marker(&inst) {
            ops.push(StackOp::Log(alloc::format!(
                "[HINT] PC=0x{:x}: Test case {}",
                this.pc,
                hint_value
            )));
        }
    }

    // ── Instruction → StackOp mapping ────────────────────────────────────────

    // Shorthand: emit fallthrough to next sequential PC.
    let ft = |ops: &mut alloc::vec::Vec<StackOp>, labels: Labels| {
        emit_to(ops, this.params, next_pc, labels, nd);
    };

    // Shorthand: emit a conditional branch (taken = branch_target, not-taken = next_pc).
    let branch =
        |ops: &mut alloc::vec::Vec<StackOp>, cmp: StackOp, branch_target: u64, labels: Labels| {
            ops.push(cmp);
            ops.push(StackOp::IfBegin);
            emit_to(ops, this.params, branch_target, labels.clone(), nd);
            ops.push(StackOp::IfElse);
            emit_to(ops, this.params, next_pc, labels, nd);
            ops.push(StackOp::IfEnd);
        };

    match inst {
        // ── Branches ─────────────────────────────────────────────────────────
        Inst::Beq { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::Eq64, target, labels);
        }
        Inst::Bne { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::Ne64, target, labels);
        }
        Inst::Blt { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::LtS64, target, labels);
        }
        Inst::Bge { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::GeS64, target, labels);
        }
        Inst::Bltu { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::LtU64, target, labels);
        }
        Inst::Bgeu { src1, src2, offset } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(&mut ops, StackOp::GeU64, target, labels);
        }

        // ── Jumps ─────────────────────────────────────────────────────────────
        Inst::Jal { offset, dest } => {
            let target = this.pc.wrapping_add_signed(offset.as_i64() * 2);
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::StoreReg(dest.0));
            emit_to(&mut ops, this.params, target, labels, nd);
        }
        Inst::Jalr { offset, base, dest } => {
            // Save return address, compute target, exit block.
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::StoreReg(dest.0));
            ops.push(StackOp::PushImm((offset.as_i64() * 2) as u64));
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::Add64);
            ops.push(StackOp::TailCall);
        }

        // ── Upper-immediate ───────────────────────────────────────────────────
        Inst::Lui { uimm, dest } => {
            ops.push(StackOp::PushImm(uimm.as_u64()));
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Auipc { uimm, dest } => {
            ops.push(StackOp::PushImm(uimm.as_u64().wrapping_add(this.pc)));
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }

        // ── Loads ─────────────────────────────────────────────────────────────
        Inst::Lb { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B1,
                signed: true,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Lbu { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B1,
                signed: false,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Lh { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B2,
                signed: true,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Lhu { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B2,
                signed: false,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Lw { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B4,
                signed: true,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Lwu { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B4,
                signed: false,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Ld { offset, dest, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadMem {
                width: MemWidth::B8,
                signed: false,
            });
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }

        // ── Stores ────────────────────────────────────────────────────────────
        // StoreMem pops: value (top), then addr.
        Inst::Sb { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B1,
            });
            ft(&mut ops, labels);
        }
        Inst::Sh { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B2,
            });
            ft(&mut ops, labels);
        }
        Inst::Sw { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B4,
            });
            ft(&mut ops, labels);
        }
        Inst::Sd { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B8,
            });
            ft(&mut ops, labels);
        }

        // ── Register-register 64-bit arithmetic ───────────────────────────────
        Inst::Add { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Add64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Sub { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Sub64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Mul { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Mul64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Mulh { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Mulhu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Mulhsu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHSU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Div { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Divu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Rem { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Remu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::And { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::And64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Or { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Or64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Xor { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Xor64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Sll { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Srl { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Sra { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Slt { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::LtS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Sltu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::LtU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }

        // ── Register-register 32-bit arithmetic (sign-extend result) ──────────
        Inst::AddW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Add32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SubW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Sub32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::MulW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Mul32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::DivW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::DivuW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::RemW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::RemuW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SllW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Shl32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SrlW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SraW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }

        // ── Immediate arithmetic ───────────────────────────────────────────────
        Inst::Addi { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::AddiW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Add32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Slti { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::LtS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Sltiu { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::LtU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Andi { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::And64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Ori { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Or64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Xori { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Xor64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Slli { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Srli { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::Srai { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SlliW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Shl32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SrliW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }
        Inst::SraiW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(&mut ops, labels);
        }

        // ── Misc ──────────────────────────────────────────────────────────────
        Inst::Fence { .. } => {
            // Fence is a no-op in this JIT; fall through.
            ft(&mut ops, labels);
        }
        Inst::Ecall => {
            ops.push(StackOp::Ecall);
            ft(&mut ops, labels);
        }
        // Ebreak and anything unrecognised → trap.
        inst => {
            ops.push(StackOp::Trap(alloc::format!("op:{inst}")));
        }
    }

    ops
}

// ── Target impls ─────────────────────────────────────────────────────────────

vane_arch::cfg_target_wasm! {
    impl<'b> RiscvWasmJit for TemplateJit<'b> {
        fn Riscv<'a>(
            &'a self,
            ctx: &'a (dyn WasmJitCtx + 'a),
        ) -> impl Iterator<Item = JitOpcode<'a>> + 'a {
            let ops = rv_emit(self);
            render_stack_to_wasm(ops.into_iter(), ctx.wasm_abi())
        }
    }
}

vane_arch::cfg_target_js! {
    impl<'a> RiscvDisplay for TemplateJit<'a> {
        fn Riscv(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
            let ops = rv_emit(self);
            render_stack_to_js(ops.into_iter(), f, self.params.flate, self.params.flags)
        }
    }
}

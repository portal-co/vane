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

// Cap on the fallthrough inline-chain length (per work item's depth).
// rv_emit inlines reachable code; without a bound, a single trace can inline
// a huge region. Capping the chain bounds each trace's compile cost; the
// dispatch loop follows the emitted TailCall to the next trace.
const MAX_INLINE: u32 = 128;
// Cap on the total ops emitted into one trace. `branch` inlines BOTH arms,
// so the emit tree grows exponentially in branch count: without a global
// bound, one trace can reach ~1M ops. The interpreter's per-trace op budget
// (4096) bounds *dynamic* execution, so static ops past it can never run in
// one dispatch anyway.
const MAX_OPS: usize = 2048;

/// A unit of pending trace emission for the iterative `rv_emit_ops`: either
/// inline the block at `pc` (an [`Emit`]), or yield a literal [`StackOp`]
/// (an [`Op`] — a control-flow marker such as `IfElse`/`IfEnd`/`LoopEnd`
/// that follows a continuation in emission order).
enum Work {
    Emit { pc: u64, labels: Labels, depth: u32 },
    Op(StackOp),
}

/// Emit one block of the trace rooted at `pc`: the cached/cap checks, the
/// `LoopBegin`/`Br`, and (for a fresh block) the instruction's ops (→
/// `out_ops`) plus its continuation work items (→ `out_work`, in emission
/// order, the closing `LoopEnd` last). Shared by the eager (`rv_emit`) and
/// lazy (`rv_emit_ops`) drivers so both emit byte-identical traces.
fn emit_block(
    params: &Params<'_>,
    pc: u64,
    labels: Labels,
    depth: u32,
    total_ops: usize,
    out_ops: &mut alloc::vec::Vec<StackOp>,
    out_work: &mut alloc::vec::Vec<Work>,
) {
    // Already compiled? Jump straight to the cached block.
    if (params.trial)(pc) == Heat::Cached {
        out_ops.push(StackOp::PushImm(pc));
        out_ops.push(StackOp::TailCall);
        return;
    }
    // Inline-chain caps: depth (per chain) and total ops.
    if depth >= MAX_INLINE || total_ops >= MAX_OPS {
        out_ops.push(StackOp::PushImm(pc));
        out_ops.push(StackOp::TailCall);
        return;
    }
    match labels.lookup(pc) {
        Some(lbl) => {
            let relative_depth = depth - lbl.index;
            out_ops.push(StackOp::Br {
                relative_depth,
                label: pc,
            });
        }
        None => {
            let body_labels = labels.insert(pc, Label { index: depth });
            out_ops.push(StackOp::LoopBegin { label: pc });
            rv_body(params, pc, body_labels, depth + 1, out_ops, out_work);
            // LoopEnd closes this block *after* its continuations.
            out_work.push(Work::Op(StackOp::LoopEnd));
        }
    }
}

// The lazy, streaming emitter `rv_emit_ops` lives in `gen_stream` (a `gen`
// block). `gen`/`yield` syntax is a parse-time nightly gate that `#[cfg]`
// cannot hide, so it must live in a file that is only *read* when the
// `gen-blocks` feature is on — hence the cfg-gated `mod`.
#[cfg(feature = "gen-blocks")]
pub mod gen_stream;
#[cfg(feature = "gen-blocks")]
pub use gen_stream::rv_emit_ops;

/// Emit the trace rooted at `this.pc` as a materialized `Vec<StackOp>` (the
/// cached form the dispatch loop interprets). Iterative worklist driver over
/// [`emit_block`] (no recursion; stable — no `gen`-block requirement).
pub fn rv_emit(this: &TemplateJit<'_>) -> alloc::vec::Vec<StackOp> {
    let mut result = alloc::vec::Vec::new();
    let mut work: alloc::vec::Vec<Work> = alloc::vec![Work::Emit {
        pc: this.pc,
        labels: this.labels.clone(),
        depth: this.depth,
    }];
    let mut total_ops = 0usize;
    let mut out_ops: alloc::vec::Vec<StackOp> = alloc::vec::Vec::new();
    let mut out_work: alloc::vec::Vec<Work> = alloc::vec::Vec::new();
    while let Some(w) = work.pop() {
        match w {
            Work::Op(op) => {
                total_ops += 1;
                result.push(op);
            }
            Work::Emit { pc, labels, depth } => {
                out_ops.clear();
                out_work.clear();
                emit_block(
                    &this.params,
                    pc,
                    labels,
                    depth,
                    total_ops,
                    &mut out_ops,
                    &mut out_work,
                );
                total_ops += out_ops.len();
                result.append(&mut out_ops);
                for wi in out_work.drain(..).rev() {
                    work.push(wi);
                }
            }
        }
    }
    result
}

fn rv_body(
    params: &Params<'_>,
    pc: u64,
    labels: Labels,
    nd: u32,
    ops: &mut alloc::vec::Vec<StackOp>,
    cont: &mut alloc::vec::Vec<Work>,
) {
    // ── Hook boundary (vane-arch::hooks) ─────────────────────────────────────
    // A hooked address must start its own trace: cut the inlining chain
    // here so the dispatch loop observes the arrival. The trace root is
    // exempt — it is already a trace start (checking it would loop
    // forever: the boundary trace would re-trigger the predicate).
    if pc != params.root
        && let Some(boundaries) = params.boundaries
        && boundaries(pc)
    {
        ops.push(StackOp::PushImm(pc));
        ops.push(StackOp::TailCall);
        return;
    }

    // ── Instruction fetch & integrity check ──────────────────────────────────
    // Instruction fetch: one lock + one translation for the whole word
    // (JitCtx::fetch4). A fetch FAULT (a not-yet-populated demand-paged
    // page) stops inlining here: boundary to this pc so the runtime
    // re-dispatch steps it and raises the in-guest page fault (which the
    // kernel uses to populate the page), rather than baking a swallowed
    // zero/garbage read into the trace (which would execute as an illegal
    // instruction the kernel cannot recover).
    let inst_bytes: [u8; 4] = match params.react.fetch4(pc) {
        Some(b) => b,
        None => {
            ops.push(StackOp::PushImm(pc));
            ops.push(StackOp::TailCall);
            return;
        }
    };
    ops.push(StackOp::CheckCode {
        addr: pc,
        expected: Box::from(inst_bytes),
        miss_label: alloc::format!("{}", params.root),
    });

    let inst_code = u32::from_le_bytes(inst_bytes);
    if let Some(mut fp_ops) = rv_float_ops(inst_code) {
        ops.append(&mut fp_ops);
        // (emit_next isn't defined yet at this point in the fn; push the
        // fallthrough continuation directly.)
        cont.push(Work::Emit {
            pc: pc + 4,
            labels,
            depth: nd,
        });
        return;
    }
    let (inst, compressed) = match Inst::decode(inst_code, Xlen::Rv64) {
        Err(e) => {
            if params.flags.privileged {
                // Decode failure under the privileged architecture: route
                // through the hart (StackOp::Privileged → execute_privileged
                // raises the in-guest illegal-instruction trap) rather than
                // fatal-exiting the interpreter. An all-zero / unmapped fetch
                // is a guest fault the kernel handles (or panics on) in-guest.
                ops.push(StackOp::Privileged { pc, raw: inst_code });
                // On the (always-taken) Divert the interpreter tail-calls the
                // trap handler directly; the PushImm fallthrough is dead.
                ops.push(StackOp::PushImm(pc));
                ops.push(StackOp::TailCall);
                return;
            }
            ops.push(StackOp::Trap(alloc::format!("decoding: {e}")));
            return;
        }
        Ok(v) => v,
    };

    let next_pc = pc
        + match compressed {
            IsCompressed::Yes => 2,
            IsCompressed::No => 4,
        };

    // ── Hint / test-mode logging ──────────────────────────────────────────────
    if params.flags.test_mode
        && let Some(hint_value) = hint::detect_test_marker(&inst)
    {
        ops.push(StackOp::Log(alloc::format!(
            "[HINT] PC=0x{:x}: Test case {}",
            pc,
            hint_value
        )));
    }

    // ── Instruction → StackOp mapping ────────────────────────────────────────

    // Push the continuation for `target` onto the worklist (inline the block
    // at `target` via an `Emit`). The depth (MAX_INLINE) and total-ops
    // (MAX_OPS) caps are applied when the Emit is popped in `rv_emit_ops`.
    let emit_next = |cont: &mut alloc::vec::Vec<Work>, target: u64, labels: Labels| {
        cont.push(Work::Emit {
            pc: target,
            labels,
            depth: nd,
        });
    };

    // Shorthand: emit fallthrough to next sequential PC.
    let ft = |cont: &mut alloc::vec::Vec<Work>, labels: Labels| {
        emit_next(cont, next_pc, labels);
    };

    // Shorthand: emit a conditional branch (taken = branch_target, not-taken = next_pc).
    let branch = |ops: &mut alloc::vec::Vec<StackOp>,
                  cont: &mut alloc::vec::Vec<Work>,
                  cmp: StackOp,
                  branch_target: u64,
                  labels: Labels| {
        ops.push(cmp);
        ops.push(StackOp::IfBegin);
        emit_next(cont, branch_target, labels.clone());
        cont.push(Work::Op(StackOp::IfElse));
        emit_next(cont, next_pc, labels);
        cont.push(Work::Op(StackOp::IfEnd));
    };

    // `rv-asm`'s B-type/J-type immediate accessors (`imm_b`/`imm_j`, and
    // their compressed counterparts feeding Beq/Bne/.../Jal below) already
    // return the fully byte-scaled PC-relative offset -- their bit-field
    // reconstruction places the low nonzero source bit at output position
    // 1, so bit 0 of the result is always 0, exactly matching the RISC-V
    // spec's "immediate encodes bit 0 implicitly as 0" convention. No
    // further `* 2` is needed (or correct): doubling here silently sent
    // every branch/jump with a nonzero offset to the wrong PC. This went
    // uncaught because no corpus test exercised a real branch/jump with a
    // nonzero offset until frame.c's `sink()` loop and its `jal` call site.
    match inst {
        // ── Branches ─────────────────────────────────────────────────────────
        Inst::Beq { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::Eq64, target, labels);
        }
        Inst::Bne { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::Ne64, target, labels);
        }
        Inst::Blt { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::LtS64, target, labels);
        }
        Inst::Bge { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::GeS64, target, labels);
        }
        Inst::Bltu { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::LtU64, target, labels);
        }
        Inst::Bgeu { src1, src2, offset } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            branch(ops, cont, StackOp::GeU64, target, labels);
        }

        // ── Jumps ─────────────────────────────────────────────────────────────
        Inst::Jal { offset, dest } => {
            let target = pc.wrapping_add_signed(offset.as_i64());
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::StoreReg(dest.0));
            emit_next(cont, target, labels);
        }
        Inst::Jalr { offset, base, dest } => {
            // Compute the jump target from `base`'s value *before* writing
            // the return address into `dest` -- per the RISC-V spec, JALR
            // must use rs1's original value even when rd == rs1 (e.g. the
            // common `jalr ra, 0(ra)` idiom for an indirect call through a
            // register already holding the callee's address). Storing
            // `next_pc` into `dest` first, as this used to, clobbers `base`
            // before it's read whenever they're the same register.
            //
            // Unlike the branches/Jal above, JALR's offset is a plain
            // I-type immediate (`imm_i`, the same field shape as e.g.
            // `addi`'s) -- never scaled by the RISC-V spec. This file used
            // to `* 2` it here too (by analogy with the branches, wrongly);
            // it needs to stay unscaled like the load/store offsets below.
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::Add64);
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::StoreReg(dest.0));
            ops.push(StackOp::TailCall);
        }

        // ── Upper-immediate ───────────────────────────────────────────────────
        Inst::Lui { uimm, dest } => {
            ops.push(StackOp::PushImm(uimm.as_u64()));
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Auipc { uimm, dest } => {
            ops.push(StackOp::PushImm(uimm.as_u64().wrapping_add(pc)));
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
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
            ft(cont, labels);
        }
        Inst::Sh { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B2,
            });
            ft(cont, labels);
        }
        Inst::Sw { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B4,
            });
            ft(cont, labels);
        }
        Inst::Sd { offset, src, base } => {
            ops.push(StackOp::LoadReg(base.0));
            ops.push(StackOp::PushImm(offset.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::LoadReg(src.0));
            ops.push(StackOp::StoreMem {
                width: MemWidth::B8,
            });
            ft(cont, labels);
        }

        // ── Register-register 64-bit arithmetic ───────────────────────────────
        Inst::Add { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Add64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Sub { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Sub64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Mul { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Mul64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Mulh { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Mulhu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Mulhsu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::MulHSU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Div { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Divu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Rem { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Remu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::And { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::And64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Or { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Or64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Xor { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Xor64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Sll { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Srl { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Sra { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Slt { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::LtS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Sltu { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::LtU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }

        // ── Register-register 32-bit arithmetic (sign-extend result) ──────────
        Inst::AddW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Add32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SubW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Sub32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::MulW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Mul32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::DivW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::DivuW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::DivU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::RemW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::RemuW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::RemU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SllW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::Shl32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SrlW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SraW { dest, src1, src2 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::LoadReg(src2.0));
            ops.push(StackOp::ShrS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }

        // ── Immediate arithmetic ───────────────────────────────────────────────
        Inst::Addi { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Add64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::AddiW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Add32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Slti { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::LtS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Sltiu { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::LtU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Andi { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::And64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Ori { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Or64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Xori { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Xor64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Slli { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Shl64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Srli { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrU64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::Srai { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrS64);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SlliW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::Shl32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SrliW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrU32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }
        Inst::SraiW { imm, dest, src1 } => {
            ops.push(StackOp::LoadReg(src1.0));
            ops.push(StackOp::PushImm(imm.as_i64() as u64));
            ops.push(StackOp::ShrS32);
            ops.push(StackOp::StoreReg(dest.0));
            ft(cont, labels);
        }

        // ── Misc ──────────────────────────────────────────────────────────────
        Inst::Fence { .. } => {
            // Fence is a no-op in this JIT; fall through.
            ft(cont, labels);
        }
        Inst::Ecall => {
            if params.flags.privileged {
                // Privileged mode (PLAN_LINUX_WASM.md §7): ecall raises an
                // environment-call exception through the trap engine, so it
                // never reaches `StackOp::Ecall` (the userspace syscall
                // hook). End the trace *before* the ecall; the dispatch
                // loop executes it via the privileged stepper.
                ops.push(StackOp::PushImm(pc));
                ops.push(StackOp::TailCall);
                return;
            }
            ops.push(StackOp::Ecall);
            if params.flags.ecall_boundary {
                // Terminate the block at the syscall (see Flags::ecall_-
                // boundary): the fallthrough becomes the next block.
                ops.push(StackOp::PushImm(next_pc));
                ops.push(StackOp::TailCall);
            } else {
                ft(cont, labels);
            }
        }
        // Privileged instructions: trace boundary in privileged mode so the
        // dispatch loop's stepper applies full CSR/trap semantics; traps in
        // legacy mode (unchanged behavior).
        Inst::Csrrw { .. }
        | Inst::Csrrs { .. }
        | Inst::Csrrc { .. }
        | Inst::Csrrwi { .. }
        | Inst::Csrrsi { .. }
        | Inst::Csrrci { .. }
        | Inst::Mret
        | Inst::Sret
        | Inst::Wfi
        | Inst::SfenceVma { .. }
        | Inst::Ebreak
            if params.flags.privileged =>
        {
            // PLAN_TRACE_CUTOVER: execute the privileged op inline via the
            // hart (StackHost::privileged) instead of boundarying out to a
            // separate step engine. The op ends the trace: on Continue the
            // PushImm/TailCall dispatch to next_pc; on Divert (trap return /
            // trap) the interpreter tail-calls the hart's pc directly,
            // skipping the fallthrough.
            ops.push(StackOp::Privileged { pc, raw: inst_code });
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::TailCall);
        }
        // F/D extension: every FP op (loads, stores, compute) is lowered as
        // StackOp::Privileged so the trace engine executes it inline via
        // execute_privileged (fp_load!/fp_store! for memory ops, exec_fp for
        // compute) instead of boundarying to the stepper — PLAN_TRACE_CUTOVER
        // P3. The op ends the trace (PushImm; TailCall after it).
        Inst::FaddD { .. }
        | Inst::FaddS { .. }
        | Inst::FclassD { .. }
        | Inst::FclassS { .. }
        | Inst::FcvtDL { .. }
        | Inst::FcvtDLu { .. }
        | Inst::FcvtDS { .. }
        | Inst::FcvtDW { .. }
        | Inst::FcvtDWu { .. }
        | Inst::FcvtLD { .. }
        | Inst::FcvtLS { .. }
        | Inst::FcvtLuD { .. }
        | Inst::FcvtLuS { .. }
        | Inst::FcvtSD { .. }
        | Inst::FcvtSL { .. }
        | Inst::FcvtSLu { .. }
        | Inst::FcvtSW { .. }
        | Inst::FcvtSWu { .. }
        | Inst::FcvtWD { .. }
        | Inst::FcvtWS { .. }
        | Inst::FcvtWuD { .. }
        | Inst::FcvtWuS { .. }
        | Inst::FdivD { .. }
        | Inst::FdivS { .. }
        | Inst::FeqD { .. }
        | Inst::FeqS { .. }
        | Inst::Fld { .. }
        | Inst::FleD { .. }
        | Inst::FleS { .. }
        | Inst::FltD { .. }
        | Inst::FltS { .. }
        | Inst::Flw { .. }
        | Inst::FmaddD { .. }
        | Inst::FmaddS { .. }
        | Inst::FmaxD { .. }
        | Inst::FmaxS { .. }
        | Inst::FminD { .. }
        | Inst::FminS { .. }
        | Inst::FmsubD { .. }
        | Inst::FmsubS { .. }
        | Inst::FmulD { .. }
        | Inst::FmulS { .. }
        | Inst::FmvDX { .. }
        | Inst::FmvWX { .. }
        | Inst::FmvXD { .. }
        | Inst::FmvXW { .. }
        | Inst::FnmaddD { .. }
        | Inst::FnmaddS { .. }
        | Inst::FnmsubD { .. }
        | Inst::FnmsubS { .. }
        | Inst::Fsd { .. }
        | Inst::FsgnjD { .. }
        | Inst::FsgnjnD { .. }
        | Inst::FsgnjnS { .. }
        | Inst::FsgnjS { .. }
        | Inst::FsgnjxD { .. }
        | Inst::FsgnjxS { .. }
        | Inst::FsqrtD { .. }
        | Inst::FsqrtS { .. }
        | Inst::FsubD { .. }
        | Inst::FsubS { .. }
        | Inst::Fsw { .. }
            if params.flags.privileged =>
        {
            // Inline FP via the hart (execute_privileged): fp_load!/fp_store!
            // for the memory ops, exec_fp for compute. Ends the trace.
            ops.push(StackOp::Privileged { pc, raw: inst_code });
            ops.push(StackOp::PushImm(next_pc));
            ops.push(StackOp::TailCall);
        }
        // Ebreak and anything unrecognised → trap. In privileged mode,
        // boundary instead so the stepper raises a proper in-guest
        // illegal-instruction trap (D5: same semantics as step_one).
        inst => {
            if params.flags.privileged {
                ops.push(StackOp::PushImm(pc));
                ops.push(StackOp::TailCall);
            } else {
                ops.push(StackOp::Trap(alloc::format!("op:{inst}")));
            }
        }
    }
}

// ── Target impls ─────────────────────────────────────────────────────────────

vane_arch::cfg_target_wasm! {
    impl<'b> RiscvWasmJit for TemplateJit<'b> {
        fn Riscv<'a>(
            &'a self,
            ctx: &'a (dyn WasmJitCtx + 'a),
        ) -> impl Iterator<Item = JitOpcode<'a>> + 'a {
            let ops = rv_emit(self);
            // `rv_emit` always wraps a full trace in one outer
            // `StackOp::LoopBegin`/`LoopEnd` (see its doc comment), and every
            // finite RV64 trace terminates via a `StackOp::TailCall` nested
            // inside that loop, never by falling off the end. WASM
            // validation does not propagate "unreachable" across a
            // block/loop boundary -- exiting a loop's `end` always resets
            // reachability to whatever it was on entry, regardless of
            // whether every path inside diverged -- so without this
            // trailing, unnested `unreachable`, the function's own implicit
            // final return goes unsatisfied even though it is genuinely
            // never reached at runtime. Confirmed with a minimal
            // `wat2wasm` repro: a bare `block` wrapping a single
            // `return_call` fails validation the same way. Provably dead
            // code in every real case, since `rv_emit`'s only exits are
            // nested tail calls.
            render_stack_to_wasm(ops.into_iter(), ctx.wasm_abi())
                .chain(core::iter::once(JitOpcode::Operator { op: wasmparser::Operator::Unreachable }))
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

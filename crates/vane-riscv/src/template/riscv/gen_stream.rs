//! The lazy, streaming form of the RV64 `StackOp` trace emitter, written as a
//! nightly `gen` block (the `gen-blocks` cargo feature). This file is only
//! *read* when that feature is on (the `mod` declaration in `super` is
//! cfg-gated), because `gen`/`yield` syntax is a parse-time gate that
//! `#[cfg]` cannot hide — see the note in `vane-arch/src/template.rs`.
//!
//! The emitter is iterative (an explicit worklist, not the old
//! `rv_emit`/`emit_to` recursion) and lazy: ops are produced on demand, so a
//! consumer can start interpreting the head of a trace while later blocks are
//! still being emitted — the streaming/pipelined form that overlaps compile
//! with interpret. [`super::rv_emit`] is the eager, stable (non-`gen`)
//! worklist over the same [`emit_block`]; both emit byte-identical traces.

use alloc::vec::Vec;
use vane_arch::StackOp;
use vane_arch::template::TemplateJit;

use super::{Work, emit_block};

/// Emit the trace rooted at `this.pc` as a lazy stream of `StackOp`s.
///
/// The worklist (`work`) drives the inline walk iteratively; each block's ops
/// are yielded as soon as `emit_block` produces them, before its continuation
/// blocks are visited. A pull-based consumer (the streaming interpreter)
/// overlaps this production with interpretation; a `collect` materializes the
/// same trace [`super::rv_emit`] builds eagerly.
pub fn rv_emit_ops<'a>(this: &TemplateJit<'a>) -> impl Iterator<Item = StackOp> + 'a {
    let params = this.params;
    let root = Work::Emit {
        pc: this.pc,
        labels: this.labels.clone(),
        depth: this.depth,
    };
    gen move {
        let mut work: Vec<Work> = alloc::vec![root];
        // Running total of ops emitted into this trace (for the MAX_OPS cap
        // in emit_block).
        let mut total_ops = 0usize;
        // Scratch buffers, reused across blocks (no per-block allocation).
        let mut out_ops: Vec<StackOp> = Vec::new();
        let mut out_work: Vec<Work> = Vec::new();
        while let Some(w) = work.pop() {
            match w {
                Work::Op(op) => {
                    total_ops += 1;
                    yield op;
                }
                Work::Emit { pc, labels, depth } => {
                    out_ops.clear();
                    out_work.clear();
                    emit_block(&params, pc, labels, depth, total_ops, &mut out_ops, &mut out_work);
                    total_ops += out_ops.len();
                    // Yield by moving each op out (mem::replace), so no
                    // borrow of the scratch buffer is held across a `yield`.
                    for i in 0..out_ops.len() {
                        let op = core::mem::replace(&mut out_ops[i], StackOp::TailCall);
                        yield op;
                    }
                    out_ops.clear();
                    // The continuations pop in emission order (LoopEnd last)
                    // because they were pushed reversed.
                    for wi in out_work.drain(..).rev() {
                        work.push(wi);
                    }
                }
            }
        }
    }
}

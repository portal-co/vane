//! aarch64 backend: lower a `StackOp` trace to executable aarch64 machine code
//! (PLAN_NATIVE_JIT Phase 2). Hand-rolled — no external codegen dependency.
//!
//! # Calling convention (the C ABI)
//! - `x0` = `ctx: *mut TraceCtx`, `x1` = `ops`, `x2` = `ops_len`.
//! - Return `TraceExit { tag: u8, pc: u64 }` — a 16-byte struct returned in
//!   `x0` (low byte = tag) + `x1` (pc).
//!
//! # Register allocation
//! - `x19` = ctx (callee-saved).
//! - `x20` = value-stack pointer: points at `ctx.stack[depth]`. Push = `str
//!   [x20], #8`; pop = `ldr [x20, #-8]!`.
//! - `x21` = `ctx.regs` (the register-cache base; callee-saved).
//! - `x9`–`x15` = scratch.
//!
//! The value stack lives in `ctx.stack` (memory) for v1 — simple + correct; a
//! register-allocated top-of-stack is a later optimization.

use vane_target_core::StackOp;

/// A small aarch64 assembler: emits 32-bit instruction words into a buffer.
struct Asm {
    buf: Vec<u8>,
    /// `LoopBegin` native labels: the byte offset of each open loop's start.
    loop_labels: Vec<usize>,
    /// Pending forward branches to patch: (instruction offset, target label id).
    /// We use a simple two-pass-free approach: record `Br`/cond-branch sites and
    /// the target label's offset, then patch.
    fixups: Vec<(usize, FixupKind, usize)>,
}

#[derive(Clone, Copy)]
enum FixupKind {
    /// Unconditional 26-bit branch (B).
    B,
    /// 19-bit conditional branch (CBZ/CBNZ).
    Cbz,
}

impl Asm {
    fn new() -> Self {
        Asm {
            buf: Vec::new(),
            loop_labels: Vec::new(),
            fixups: Vec::new(),
        }
    }
    fn off(&self) -> usize {
        self.buf.len()
    }
    fn u32(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    // ── basic encodings ────────────────────────────────────────────────────
    /// `mov x{d}, #imm` (64-bit immediate via movz/movk).
    fn mov_imm(&mut self, d: u8, imm: u64) {
        // movz x{d}, #imm0
        self.u32((0xd2800000 | ((0x2 & 0x3) << 29)) | (((imm & 0xffff) as u32) << 5) | d as u32);
        // movk for the remaining 16-bit chunks.
        for shift in [16u32, 32, 48] {
            let chunk = ((imm >> shift) & 0xffff) as u32;
            if chunk != 0 {
                // movk x{d}, #chunk, lsl #shift
                self.u32(0xf2800000 | ((shift / 16) << 21) | (chunk << 5) | d as u32);
            }
        }
    }
    /// `mov x{d}, x{s}` (alias for orr x{d}, xzr, x{s}).
    fn mov_reg(&mut self, d: u8, s: u8) {
        self.u32((0xaa000000 | (31 << 16) | ((s as u32) << 5)) | d as u32 | ((s as u32) << 16));
    }
    /// `ldr x{t}, [x{n}, #imm]` (unsigned offset).
    fn ldr(&mut self, t: u8, n: u8, imm: u16) {
        self.u32(0xf9400000 | (((imm / 8) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `str x{t}, [x{n}, #imm]` (unsigned offset).
    fn str_(&mut self, t: u8, n: u8, imm: u16) {
        self.u32(0xf9000000 | (((imm / 8) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `ldr x{t}, [x{n}, #-8]!` (pre-index, for pop).
    fn ldr_pre(&mut self, t: u8, n: u8) {
        self.u32(0xf85e0e00 | ((n as u32) << 5) | t as u32);
    }
    /// `str x{t}, [x{n}, #8]!` (post-index... actually pre-index for push).
    fn str_pre(&mut self, t: u8, n: u8) {
        // str x{t}, [x{n}, #8]! — pre-index writeback, imm=+8
        self.u32(0xf8000e00 | (0x08 << 12) | ((n as u32) << 5) | t as u32 | (1 << 11));
    }
    /// `add x{d}, x{n}, x{m}`.
    fn add(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x8b000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    /// `sub x{d}, x{n}, x{m}`.
    fn sub(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0xcb000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    /// `blr x{n}`.
    fn blr(&mut self, n: u8) {
        self.u32(0xd63f0000 | ((n as u32) << 5));
    }
    /// `ret`.
    fn ret(&mut self) {
        self.u32(0xd65f03c0);
    }
    /// `b <label>` (unconditional; patched later).
    fn b(&mut self, label: usize) {
        self.fixups.push((self.off(), FixupKind::B, label));
        self.u32(0x14000000);
    }
    /// `cbz x{t}, <label>` (patched later).
    fn cbz(&mut self, t: u8, label: usize) {
        self.fixups.push((self.off(), FixupKind::Cbz, label));
        self.u32(0xb4000000 | t as u32);
    }
}

/// Compile a `StackOp` trace to aarch64 machine code. Returns the executable
/// entry point, or `Err` for an op this backend can't lower (caller falls back
/// to the interpreter).
pub fn compile(ops: &[StackOp]) -> Result<crate::NativeTraceFn, &'static str> {
    // This is the Phase-2 core. The full per-op lowering is written in
    // `lower` below; the codegen is being brought up incrementally. For now we
    // refuse (the interpreter stays the producer) until the lowering is
    // complete and differentially validated.
    let _ = ops;
    Err("native-jit aarch64 backend: codegen not yet complete")
}

/// Compute the maximum value-stack depth of a trace (for the `STACK_CAP`
/// check in `crate::ctx`). Returns `None` if the trace is malformed (negative
/// depth).
pub fn max_stack_depth(ops: &[StackOp]) -> Option<usize> {
    let mut depth = 0i64;
    let mut max = 0i64;
    for op in ops {
        let (pops, pushes): (i64, i64) = match op {
            StackOp::PushImm(_) | StackOp::LoadReg(_) | StackOp::LoadState(_) => (0, 1),
            StackOp::StoreReg(_) | StackOp::StoreState(_) | StackOp::Drop => (1, 0),
            StackOp::LoadMem { .. } => (1, 1),
            StackOp::StoreMem { .. } => (2, 0),
            StackOp::TailCall => (1, 0),
            StackOp::Ecall | StackOp::Trap(_) | StackOp::Log(_) => (0, 0),
            StackOp::Privileged { .. } => (0, 0),
            StackOp::CheckCode { .. } => (0, 0),
            StackOp::LoopBegin { .. } | StackOp::LoopEnd => (0, 0),
            StackOp::Br { .. } => (0, 0),
            StackOp::IfBegin => (1, 0),
            StackOp::IfElse | StackOp::IfEnd => (0, 0),
            // Binary ALU / comparisons.
            _ => (2, 1),
        };
        depth -= pops;
        if depth < 0 {
            return None;
        }
        depth += pushes;
        if depth > max {
            max = depth;
        }
    }
    Some(max as usize)
}

//! aarch64 backend: lower a `StackOp` trace to executable aarch64 machine code
//! (PLAN_NATIVE_JIT Phase 2). Hand-rolled — no external codegen dependency.
//!
//! # ABI / register allocation
//! Entry: `extern "C" fn(ctx: *mut TraceCtx, ops, ops_len) -> u8` (the exit
//! tag; the exit pc is written to `ctx.exit_pc`). `x0`=ctx, `x1`=ops, `x2`=len.
//! - `x19` = ctx (callee-saved).
//! - `x20` = value-stack pointer (points at `ctx.stack[depth]`). Push =
//!   `str [x20], #8`; pop = `ldr [x20, #-8]!`.
//! - `x21` = `ctx.regs` (the host's register cache; inline access).
//! - `x22` = undo pointer (the next free `ctx.undo_buf` slot).
//! - `x23` = undo-mark pointer (the committed boundary).
//! - `x24` = `cur_inst_pc` (the `Bail` target).
//! - `x9`–`x15` = scratch (clobbered by host-shim calls).
//!
//! All instruction encodings below are verified against the host assembler
//! (see the Phase-2 dev log); the encoding helpers are named after the
//! disassembled mnemonic.

use crate::ctx::TraceCtx;
use crate::mem::ExecRegion;
use crate::{JitTrace, NativeTraceFn, TAG_BAIL, TAG_ECALL, TAG_TAILCALL, TAG_TRAP};
use core::mem::offset_of;
use vane_target_core::StackOp;

// Condition codes.
const EQ: u8 = 0;
const NE: u8 = 1;
const HS: u8 = 2; // unsigned >=
const LO: u8 = 3; // unsigned <
const GE: u8 = 10; // signed >=
const LT: u8 = 11; // signed <

// ctx field offsets.
const O_REGS: usize = offset_of!(TraceCtx, regs);
const O_DEPTH: usize = offset_of!(TraceCtx, depth);
const O_UNDO_LEN: usize = offset_of!(TraceCtx, undo_len);
const O_UNDO_MARK: usize = offset_of!(TraceCtx, undo_mark);
const O_FUEL: usize = offset_of!(TraceCtx, fuel);
const O_EXIT_PC: usize = offset_of!(TraceCtx, exit_pc);
const O_UNDO_BUF: usize = offset_of!(TraceCtx, undo_buf);
const O_MEM_LOAD: usize = offset_of!(TraceCtx, mem_load);
const O_MEM_STORE: usize = offset_of!(TraceCtx, mem_store);
const O_PRIVILEGED: usize = offset_of!(TraceCtx, privileged);
const O_TAKE_TRAP: usize = offset_of!(TraceCtx, take_trap);
const O_TAKE_IRQ: usize = offset_of!(TraceCtx, take_irq);
const O_READ_BYTE: usize = offset_of!(TraceCtx, read_byte);
const O_WANT_INTEG: usize = offset_of!(TraceCtx, want_integrity);
const O_STATE_LOAD: usize = offset_of!(TraceCtx, state_load);
const O_STATE_STORE: usize = offset_of!(TraceCtx, state_store);
const O_ECALL: usize = offset_of!(TraceCtx, ecall);
const O_LOG: usize = offset_of!(TraceCtx, log);
const O_FP_UNARY: usize = offset_of!(TraceCtx, fp_unary);
const O_FP_BINARY: usize = offset_of!(TraceCtx, fp_binary);
const O_FP_CMP: usize = offset_of!(TraceCtx, fp_cmp);
const O_INT_TO_FP: usize = offset_of!(TraceCtx, int_to_fp);
const O_FP_TO_INT: usize = offset_of!(TraceCtx, fp_to_int);
const O_FP_CONVERT: usize = offset_of!(TraceCtx, fp_convert);
const O_TAIL_LOOKUP: usize = offset_of!(TraceCtx, tail_lookup);
const O_HOST: usize = offset_of!(TraceCtx, host);

/// The aarch64 assembler + the codegen state (loop/if label stacks, exit
/// jumps to the shared epilogue).
struct Asm {
    buf: Vec<u8>,
    /// `LoopBegin` native offsets (the loop-start byte offset), as a stack
    /// mirroring the op stream's loop nesting. `Br` resolves its target by
    /// `relative_depth` into this stack. Unlike the interpreter we do NOT
    /// truncate on `Br`: the truncate is a runtime artifact of re-executing
    /// loop bodies, whereas codegen visits each op once, so the stack must
    /// keep reflecting the static nesting (which is what `relative_depth`
    /// indexes into).
    loop_stack: Vec<usize>,
    /// Per-`If` fixup offset to patch at the next `IfElse`/`IfEnd`.
    if_stack: Vec<usize>,
    /// Forward `b` offsets that target the shared epilogue.
    exit_jumps: Vec<usize>,
}

impl Asm {
    fn new() -> Self {
        Asm {
            buf: Vec::new(),
            loop_stack: Vec::new(),
            if_stack: Vec::new(),
            exit_jumps: Vec::new(),
        }
    }
    fn off(&self) -> usize {
        self.buf.len()
    }
    fn u32(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    // ── data movement ─────────────────────────────────────────────────────
    /// `mov x{d}, #imm` (movz + movk for the non-zero high chunks).
    fn mov_imm(&mut self, d: u8, imm: u64) {
        self.u32(0xd2800000 | (((imm & 0xffff) as u32) << 5) | d as u32);
        for shift in [16u32, 32, 48] {
            let chunk = ((imm >> shift) & 0xffff) as u32;
            if chunk != 0 {
                self.u32(0xf2800000 | ((shift / 16) << 21) | (chunk << 5) | d as u32);
            }
        }
    }
    /// `mov x{d}, x{s}` (orr x{d}, xzr, x{s}).
    fn mov_reg(&mut self, d: u8, s: u8) {
        self.u32(0xaa000000 | ((s as u32) << 16) | (31 << 5) | d as u32);
    }
    /// `ldr x{t}, [x{n}, #imm]` (unsigned scaled offset; imm is bytes).
    fn ldr(&mut self, t: u8, n: u8, imm: usize) {
        self.u32(0xf9400000 | (((imm / 8) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `str x{t}, [x{n}, #imm]`.
    fn str_(&mut self, t: u8, n: u8, imm: usize) {
        self.u32(0xf9000000 | (((imm / 8) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `ldr w{t}, [x{n}, #imm]` (32-bit).
    fn ldr32(&mut self, t: u8, n: u8, imm: usize) {
        self.u32(0xb9400000 | (((imm / 4) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `str w{t}, [x{n}, #imm]` (32-bit).
    fn str32(&mut self, t: u8, n: u8, imm: usize) {
        self.u32(0xb9000000 | (((imm / 4) as u32) << 10) | ((n as u32) << 5) | t as u32);
    }
    /// `ldr x{t}, [x{n}, #-8]!` (pre-index; the value-stack pop).
    fn ldr_pre(&mut self, t: u8, n: u8) {
        self.u32(0xf85f8c00 | ((n as u32) << 5) | t as u32);
    }
    /// `str x{t}, [x{n}], #8` (post-index; the value-stack push).
    fn str_post(&mut self, t: u8, n: u8) {
        self.u32(0xf8008400 | ((n as u32) << 5) | t as u32);
    }
    /// `ldur x{t}, [x{n}, #simm]` (unscaled signed offset).
    fn ldur(&mut self, t: u8, n: u8, simm: i32) {
        self.u32(0xf8400000 | (((simm as u32) & 0x1ff) << 12) | ((n as u32) << 5) | t as u32);
    }
    /// `stur x{t}, [x{n}, #simm]`.
    fn stur(&mut self, t: u8, n: u8, simm: i32) {
        self.u32(0xf8000000 | (((simm as u32) & 0x1ff) << 12) | ((n as u32) << 5) | t as u32);
    }
    /// `stp x{t1}, x{t2}, [x{n}], #imm` (post-index; the undo-buffer append).
    fn stp_post(&mut self, t1: u8, t2: u8, n: u8, imm: i32) {
        self.u32(
            0xa8800000
                | ((((imm / 8) as u32) & 0x7f) << 15)
                | ((t2 as u32) << 10)
                | ((n as u32) << 5)
                | t1 as u32,
        );
    }
    /// `stp x{t1}, x{t2}, [x{n}, #imm]!` (pre-index).
    fn stp_pre(&mut self, t1: u8, t2: u8, n: u8, imm: i32) {
        self.u32(
            0xa9800000
                | ((((imm / 8) as u32) & 0x7f) << 15)
                | ((t2 as u32) << 10)
                | ((n as u32) << 5)
                | t1 as u32,
        );
    }
    /// `stp x{t1}, x{t2}, [x{n}, #imm]` (signed offset).
    fn stp_off(&mut self, t1: u8, t2: u8, n: u8, imm: i32) {
        self.u32(
            0xa9000000
                | ((((imm / 8) as u32) & 0x7f) << 15)
                | ((t2 as u32) << 10)
                | ((n as u32) << 5)
                | t1 as u32,
        );
    }
    /// `ldp x{t1}, x{t2}, [x{n}, #imm]` (signed offset).
    fn ldp_off(&mut self, t1: u8, t2: u8, n: u8, imm: i32) {
        self.u32(
            0xa9400000
                | ((((imm / 8) as u32) & 0x7f) << 15)
                | ((t2 as u32) << 10)
                | ((n as u32) << 5)
                | t1 as u32,
        );
    }
    /// `ldp x{t1}, x{t2}, [x{n}], #imm` (post-index).
    fn ldp_post(&mut self, t1: u8, t2: u8, n: u8, imm: i32) {
        self.u32(
            0xa8c00000
                | ((((imm / 8) as u32) & 0x7f) << 15)
                | ((t2 as u32) << 10)
                | ((n as u32) << 5)
                | t1 as u32,
        );
    }

    // ── ALU (64-bit) ──────────────────────────────────────────────────────
    fn add(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x8b000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn sub(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0xcb000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn and64(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x8a000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn orr64(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0xaa000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn eor64(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0xca000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn lslv(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9ac02000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn lsrv(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9ac02400 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn asrv(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9ac02800 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn mul(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9b000000 | ((m as u32) << 16) | (31 << 10) | ((n as u32) << 5) | d as u32);
    }
    fn udiv(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9ac00800 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn sdiv(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9ac00c00 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    /// `msub x{d}, x{n}, x{m}, x{a}` = x{a} - x{n}*x{m}.
    fn msub(&mut self, d: u8, n: u8, m: u8, a: u8) {
        self.u32(
            0x9b008000 | ((m as u32) << 16) | ((a as u32) << 10) | ((n as u32) << 5) | d as u32,
        );
    }
    fn umulh(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9bc00000 | ((m as u32) << 16) | (31 << 10) | ((n as u32) << 5) | d as u32);
    }
    fn smulh(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x9b400000 | ((m as u32) << 16) | (31 << 10) | ((n as u32) << 5) | d as u32);
    }
    /// `cmp x{n}, x{m}`.
    fn cmp(&mut self, n: u8, m: u8) {
        self.u32(0xeb000000 | ((m as u32) << 16) | ((n as u32) << 5) | 31);
    }
    /// `cmp x{n}, #imm`.
    fn cmp_imm(&mut self, n: u8, imm: u16) {
        self.u32(0xf1000000 | ((imm as u32) << 10) | ((n as u32) << 5) | 31);
    }
    /// `cset x{d}, cond`.
    fn cset(&mut self, d: u8, cond: u8) {
        self.u32(0x9a9f07e0 | (((cond ^ 1) as u32) << 12) | d as u32);
    }
    /// `csinv x{d}, x{n}, x{m}, cond` = if cond { x{n} } else { !x{m} }.
    fn csinv(&mut self, d: u8, n: u8, m: u8, cond: u8) {
        self.u32(
            0xda800000 | ((m as u32) << 16) | ((cond as u32) << 12) | ((n as u32) << 5) | d as u32,
        );
    }
    /// `sxtw x{d}, w{n}` (sign-extend the low 32).
    fn sxtw(&mut self, d: u8, n: u8) {
        self.u32(0x93407c00 | ((n as u32) << 5) | d as u32);
    }
    /// `asr x{d}, x{n}, #s` (immediate).
    fn asr_imm(&mut self, d: u8, n: u8, s: u8) {
        self.u32(0x93400000 | ((s as u32) << 16) | (63 << 10) | ((n as u32) << 5) | d as u32);
    }
    /// `lsr x{d}, x{n}, #s` (immediate).
    fn lsr_imm(&mut self, d: u8, n: u8, s: u8) {
        self.u32(0xd3400000 | ((s as u32) << 16) | (63 << 10) | ((n as u32) << 5) | d as u32);
    }
    /// `add x{d}, x{n}, x{m}, lsl #s`.
    fn add_shifted(&mut self, d: u8, n: u8, m: u8, s: u8) {
        self.u32(
            0x8b000000 | ((m as u32) << 16) | ((s as u32) << 10) | ((n as u32) << 5) | d as u32,
        );
    }
    /// `add x{d}, x{n}, #imm` (handles the shifted form for large offsets).
    fn add_imm(&mut self, d: u8, n: u8, imm: usize) {
        if imm < 4096 {
            self.u32(0x91000000 | ((imm as u32) << 10) | ((n as u32) << 5) | d as u32);
        } else {
            self.u32(
                0x91000000
                    | (1 << 22)
                    | (((imm >> 12) as u32) << 10)
                    | ((n as u32) << 5)
                    | d as u32,
            );
        }
    }
    /// `sub x{d}, x{n}, #imm`.
    fn sub_imm(&mut self, d: u8, n: u8, imm: u16) {
        self.u32(0xd1000000 | ((imm as u32) << 10) | ((n as u32) << 5) | d as u32);
    }

    // ── ALU (32-bit; results in W registers, then sign-extended) ──────────
    fn add32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x0b000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn sub32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x4b000000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn mul32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1b000000 | ((m as u32) << 16) | (31 << 10) | ((n as u32) << 5) | d as u32);
    }
    fn udiv32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1ac00800 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn sdiv32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1ac00c00 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn msub32(&mut self, d: u8, n: u8, m: u8, a: u8) {
        self.u32(
            0x1b008000 | ((m as u32) << 16) | ((a as u32) << 10) | ((n as u32) << 5) | d as u32,
        );
    }
    fn lslv32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1ac02000 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn lsrv32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1ac02400 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    fn asrv32(&mut self, d: u8, n: u8, m: u8) {
        self.u32(0x1ac02800 | ((m as u32) << 16) | ((n as u32) << 5) | d as u32);
    }
    /// `mov w{d}, w{s}` (zero-extends into x{d}; the `Trunc32` lowering).
    fn mov_w(&mut self, d: u8, s: u8) {
        self.u32(0x2a000000 | ((s as u32) << 16) | (31 << 5) | d as u32);
    }
    /// `cmp w{n}, w{m}` (32-bit).
    fn cmp32(&mut self, n: u8, m: u8) {
        self.u32(0x6b000000 | ((m as u32) << 16) | ((n as u32) << 5) | 31);
    }

    // ── branches ──────────────────────────────────────────────────────────
    /// `b <target>` (the target is a known byte offset, e.g. a backward `Br`).
    fn b_to(&mut self, target: usize) {
        let off = self.off();
        let imm = (target as i64 - off as i64) >> 2;
        self.u32(0x14000000 | ((imm as u32) & 0x3ff_ffff));
    }
    /// `blr x{n}`.
    fn blr(&mut self, n: u8) {
        self.u32(0xd63f0000 | ((n as u32) << 5));
    }
    fn ret(&mut self) {
        self.u32(0xd65f03c0);
    }
    /// `br x{n}` — an UNCONDITIONAL tail jump: the frame is replaced, the
    /// callee returns directly to our caller. Used for compiled ->
    /// compiled direct tail calls.
    fn br_reg(&mut self, n: u8) {
        self.u32(0xd61f0000 | ((n as u32) << 5));
    }
    /// Emit a `b <placeholder>`; patch later with [`patch_to_here`]. Returns
    /// the instruction offset.
    fn b_fwd(&mut self) -> usize {
        let off = self.off();
        self.u32(0x14000000);
        off
    }
    fn cbz_fwd(&mut self, t: u8) -> usize {
        let off = self.off();
        self.u32(0xb4000000 | t as u32);
        off
    }
    fn cbnz_fwd(&mut self, t: u8) -> usize {
        let off = self.off();
        self.u32(0xb5000000 | t as u32);
        off
    }
    /// `cbz w{t}, <placeholder>` (32-bit; the fuel check).
    fn cbz32_fwd(&mut self, t: u8) -> usize {
        let off = self.off();
        self.u32(0x34000000 | t as u32);
        off
    }
    fn bcond_fwd(&mut self, cond: u8) -> usize {
        let off = self.off();
        self.u32(0x54000000 | cond as u32);
        off
    }
    /// Patch the branch at `off` to target the current offset.
    fn patch_to_here(&mut self, off: usize) {
        let target = self.off();
        let insn = u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap());
        let imm = (target as i64 - off as i64) >> 2;
        let new = if insn >> 24 == 0x14 {
            // B: imm26 in bits 25:0.
            (insn & !0x3ff_ffff) | ((imm as u32) & 0x3ff_ffff)
        } else if insn >> 24 == 0x54 {
            // B.cond: imm19 in bits 23:5.
            (insn & !(0x7_ffff << 5)) | (((imm as u32) & 0x7_ffff) << 5)
        } else {
            // CBZ/CBNZ (0xb4/0xb5, 64-bit; 0x34 32-bit): imm19 in bits 23:5.
            (insn & !(0x7_ffff << 5)) | (((imm as u32) & 0x7_ffff) << 5)
        };
        self.buf[off..off + 4].copy_from_slice(&new.to_le_bytes());
    }
    /// Emit an exit: `b <epilogue>` (patched at the end).
    fn exit_jump(&mut self) {
        let off = self.b_fwd();
        self.exit_jumps.push(off);
    }

    // ── value-stack helpers ───────────────────────────────────────────────
    /// Push the register `t` onto the value stack.
    fn push(&mut self, t: u8) {
        self.str_post(t, 20);
    }
    /// Pop the value-stack top into `t`.
    fn pop(&mut self, t: u8) {
        self.ldr_pre(t, 20);
    }
    /// A binary ALU op: pop b (x9), load a (x10), `f(x10, x10, x9)`, store at
    /// a's slot (the net effect is pop2 + push).
    fn binop(&mut self, f: impl FnOnce(&mut Asm)) {
        self.pop(9); // b
        self.ldur(10, 20, -8); // a
        f(self); // x10 = a op b
        self.stur(10, 20, -8);
    }
    /// A 32-bit binary ALU op (computes on W registers, then sign-extends).
    fn binop32(&mut self, f: impl FnOnce(&mut Asm)) {
        self.pop(9);
        self.ldur(10, 20, -8);
        f(self);
        self.sxtw(10, 10);
        self.stur(10, 20, -8);
    }
    /// A comparison: pop b, load a, `cmp a, b`, `cset cond`, store (0/1).
    fn cmpop(&mut self, cond: u8) {
        self.pop(9);
        self.ldur(10, 20, -8);
        self.cmp(10, 9);
        self.cset(10, cond);
        self.stur(10, 20, -8);
    }
    /// `cbnz w{t}, <placeholder>` (32-bit).
    fn cbnz32_fwd(&mut self, t: u8) -> usize {
        let off = self.off();
        self.u32(0x35000000 | t as u32);
        off
    }
    /// `sub w{d}, w{n}, #imm` (32-bit immediate).
    fn sub_imm32(&mut self, d: u8, n: u8, imm: u16) {
        self.u32(0x51000000 | ((imm as u32) << 10) | ((n as u32) << 5) | d as u32);
    }
    /// Call a host shim (`x0` = ctx; the caller sets x1–x3 first).
    fn call(&mut self, shim_offset: usize) {
        self.mov_reg(0, 19);
        self.ldr(11, 19, shim_offset);
        self.blr(11);
    }
    /// Call a pure (no-ctx) shim — the FP helpers (the caller sets x0–x3).
    fn call_pure(&mut self, shim_offset: usize) {
        self.ldr(11, 19, shim_offset);
        self.blr(11);
    }
    /// Patch the branch at `off` to target `target` (a byte offset).
    fn patch_to(&mut self, off: usize, target: usize) {
        let insn = u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap());
        let imm = (target as i64 - off as i64) >> 2;
        let new = if insn >> 24 == 0x14 {
            (insn & !0x3ff_ffff) | ((imm as u32) & 0x3ff_ffff)
        } else {
            (insn & !(0x7_ffff << 5)) | (((imm as u32) & 0x7_ffff) << 5)
        };
        self.buf[off..off + 4].copy_from_slice(&new.to_le_bytes());
    }

    // ── frame ───────────────────────────────────────────────────────────────
    fn prologue(&mut self) {
        self.stp_pre(19, 20, 31, -64);
        self.stp_off(21, 22, 31, 16);
        self.stp_off(23, 24, 31, 32);
        self.str_(30, 31, 48);
        self.mov_reg(19, 0); // x19 = ctx
        self.ldr(21, 19, O_REGS); // x21 = ctx.regs
        self.ldr32(9, 19, O_DEPTH); // w9 = ctx.depth
        self.add_shifted(20, 19, 9, 3); // x20 = &ctx.stack[depth]  (stack at off 0)
        self.add_imm(22, 19, O_UNDO_BUF); // x22 = undo ptr
        self.mov_reg(23, 22); // x23 = undo mark
        self.mov_reg(24, 31); // x24 = cur_inst_pc = 0
    }
    /// The shared epilogue: write back depth/undo_len/undo_mark, restore the
    /// callee-saved registers, return x0 (the exit tag). The exits jump here.
    fn epilogue(&mut self) {
        let epi = self.off();
        let jumps = core::mem::take(&mut self.exit_jumps);
        for off in jumps {
            self.patch_to(off, epi);
        }
        // depth = (x20 - x19) >> 3  (the value stack is at offset 0)
        self.sub(9, 20, 19);
        self.lsr_imm(9, 9, 3);
        self.str32(9, 19, O_DEPTH);
        // base = x19 + O_UNDO_BUF
        self.add_imm(9, 19, O_UNDO_BUF);
        // undo_len = (x22 - base) >> 4
        self.sub(10, 22, 9);
        self.lsr_imm(10, 10, 4);
        self.str32(10, 19, O_UNDO_LEN);
        // undo_mark = (x23 - base) >> 4
        self.sub(11, 23, 9);
        self.lsr_imm(11, 11, 4);
        self.str32(11, 19, O_UNDO_MARK);
        // restore + return x0 (the tag)
        self.ldr(30, 31, 48);
        self.ldp_off(23, 24, 31, 32);
        self.ldp_off(21, 22, 31, 16);
        self.ldp_post(19, 20, 31, 64);
        self.ret();
    }

    // ── exit helpers ────────────────────────────────────────────────────────
    /// `exit TailCall(x9)` — store x9 into ctx.exit_pc, tag, jump to the epilogue.
    fn exit_tailcall_x9(&mut self) {
        self.str_(9, 19, O_EXIT_PC);
        self.mov_imm(0, TAG_TAILCALL as u64);
        self.exit_jump();
    }
    /// `exit Bail(cur_inst_pc)` (cur_inst_pc is x24).
    fn exit_bail(&mut self) {
        self.str_(24, 19, O_EXIT_PC);
        self.mov_imm(0, TAG_BAIL as u64);
        self.exit_jump();
    }

    // ── the per-op lowering ─────────────────────────────────────────────────
    fn lower(&mut self, op: &StackOp, index: usize) {
        match op {
            StackOp::PushImm(v) => {
                self.mov_imm(9, *v);
                self.push(9);
            }
            StackOp::LoadReg(r) => {
                if *r == 0 {
                    self.mov_imm(9, 0);
                } else {
                    self.ldr(9, 21, 8 * (*r as usize));
                }
                self.push(9);
            }
            StackOp::StoreReg(r) => {
                if *r == 0 {
                    self.pop(9); // discard (x0 is hardwired zero)
                } else {
                    self.ldr(9, 21, 8 * (*r as usize)); // old value
                    self.mov_imm(10, *r as u64); // the reg index
                    self.stp_post(10, 9, 22, 16); // undo_buf <- (reg, old)
                    self.pop(11); // the value
                    self.str_(11, 21, 8 * (*r as usize));
                }
            }
            StackOp::Drop => self.sub_imm(20, 20, 8),
            // ── integer ALU (64-bit) ─────────────────────────────────────
            StackOp::Add64 => self.binop(|a| a.add(10, 10, 9)),
            StackOp::Sub64 => self.binop(|a| a.sub(10, 10, 9)),
            StackOp::Mul64 => self.binop(|a| a.mul(10, 10, 9)),
            StackOp::And64 => self.binop(|a| a.and64(10, 10, 9)),
            StackOp::Or64 => self.binop(|a| a.orr64(10, 10, 9)),
            StackOp::Xor64 => self.binop(|a| a.eor64(10, 10, 9)),
            StackOp::Shl64 => self.binop(|a| a.lslv(10, 10, 9)),
            StackOp::ShrU64 => self.binop(|a| a.lsrv(10, 10, 9)),
            StackOp::ShrS64 => self.binop(|a| a.asrv(10, 10, 9)),
            StackOp::MulHU64 => self.binop(|a| a.umulh(10, 10, 9)),
            StackOp::MulHS64 => self.binop(|a| a.smulh(10, 10, 9)),
            StackOp::MulHSU64 => {
                self.pop(9); // b
                self.ldur(10, 20, -8); // a
                self.asr_imm(11, 10, 63); // sign mask
                self.and64(11, 11, 9); // b if a < 0 else 0
                self.umulh(10, 10, 9);
                self.sub(10, 10, 11);
                self.stur(10, 20, -8);
            }
            StackOp::DivU64 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.udiv(11, 10, 9);
                self.cmp(9, 31);
                self.csinv(10, 11, 31, NE); // b==0 -> u64::MAX
                self.stur(10, 20, -8);
            }
            StackOp::DivS64 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.sdiv(11, 10, 9);
                self.cmp(9, 31);
                self.csinv(10, 11, 31, NE);
                self.stur(10, 20, -8);
            }
            StackOp::RemU64 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.udiv(11, 10, 9);
                self.msub(10, 11, 9, 10); // a - (a/b)*b  (= a when b==0)
                self.stur(10, 20, -8);
            }
            StackOp::RemS64 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.sdiv(11, 10, 9);
                self.msub(10, 11, 9, 10);
                self.stur(10, 20, -8);
            }
            // ── integer ALU (32-bit) ─────────────────────────────────────
            StackOp::Add32 => self.binop32(|a| a.add32(10, 10, 9)),
            StackOp::Sub32 => self.binop32(|a| a.sub32(10, 10, 9)),
            StackOp::Mul32 => self.binop32(|a| a.mul32(10, 10, 9)),
            StackOp::Shl32 => self.binop32(|a| a.lslv32(10, 10, 9)),
            StackOp::ShrU32 => self.binop32(|a| a.lsrv32(10, 10, 9)),
            StackOp::ShrS32 => self.binop32(|a| a.asrv32(10, 10, 9)),
            StackOp::DivU32 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.udiv32(11, 10, 9);
                self.sxtw(11, 11);
                self.cmp32(9, 31);
                self.csinv(10, 11, 31, NE);
                self.stur(10, 20, -8);
            }
            StackOp::DivS32 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.sdiv32(11, 10, 9);
                self.sxtw(11, 11);
                self.cmp32(9, 31);
                self.csinv(10, 11, 31, NE);
                self.stur(10, 20, -8);
            }
            StackOp::RemU32 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.udiv32(11, 10, 9);
                self.msub32(10, 11, 9, 10);
                self.sxtw(10, 10);
                self.stur(10, 20, -8);
            }
            StackOp::RemS32 => {
                self.pop(9);
                self.ldur(10, 20, -8);
                self.sdiv32(11, 10, 9);
                self.msub32(10, 11, 9, 10);
                self.sxtw(10, 10);
                self.stur(10, 20, -8);
            }
            // ── conversions ──────────────────────────────────────────────
            StackOp::Trunc32 => {
                self.ldur(9, 20, -8);
                self.mov_w(9, 9); // zero-extend the low 32
                self.stur(9, 20, -8);
            }
            StackOp::SignExt32 => {
                self.ldur(9, 20, -8);
                self.sxtw(9, 9);
                self.stur(9, 20, -8);
            }
            StackOp::AsSigned64 | StackOp::AsUnsigned64 => {} // bit-identical no-op
            // ── comparisons ──────────────────────────────────────────────
            StackOp::Eq64 => self.cmpop(EQ),
            StackOp::Ne64 => self.cmpop(NE),
            StackOp::LtU64 => self.cmpop(LO),
            StackOp::LtS64 => self.cmpop(LT),
            StackOp::GeU64 => self.cmpop(HS),
            StackOp::GeS64 => self.cmpop(GE),
            // ── memory ───────────────────────────────────────────────────
            StackOp::LoadMem { width, signed } => {
                self.pop(9); // addr
                self.mov_reg(1, 9);
                self.mov_imm(2, *width as u64);
                self.mov_imm(3, *signed as u64);
                self.call(O_MEM_LOAD);
                self.push(0);
            }
            StackOp::StoreMem { width } => {
                self.pop(9); // v
                self.pop(10); // addr
                self.mov_reg(1, 10);
                self.mov_imm(2, *width as u64);
                self.mov_reg(3, 9);
                self.call(O_MEM_STORE);
            }
            // ── state slots (the FP register file) ───────────────────────
            StackOp::LoadState(slot) => {
                self.mov_imm(1, slot.as_str().as_ptr() as u64);
                self.mov_imm(2, slot.as_str().len() as u64);
                self.call(O_STATE_LOAD);
                self.push(0);
            }
            StackOp::StoreState(slot) => {
                self.pop(9); // the value
                self.mov_imm(1, slot.as_str().as_ptr() as u64);
                self.mov_imm(2, slot.as_str().len() as u64);
                self.mov_reg(3, 9);
                self.call(O_STATE_STORE);
            }
            // ── side effects ─────────────────────────────────────────────
            StackOp::Ecall => {
                self.call(O_ECALL);
                let cont = self.cbnz_fwd(0); // 1 = continue
                self.mov_imm(0, TAG_ECALL as u64);
                self.exit_jump();
                self.patch_to_here(cont);
            }
            StackOp::Trap(_) => {
                self.mov_imm(9, index as u64); // the op index (the adapter reads the msg)
                self.str_(9, 19, O_EXIT_PC);
                self.mov_imm(0, TAG_TRAP as u64);
                self.exit_jump();
            }
            StackOp::TailCall => {
                self.pop(9);
                // Direct tail path: ask the host for the target's compiled
                // fn; jump straight into it when available instead of
                // exiting to the dispatch loop. The host's lookup decides
                // when NOT to (hop budget / code dirty / hooks / uncompiled
                // target) by returning ct == 0.
                self.str_(9, 19, O_EXIT_PC); // persist: the stub may clobber x9
                self.mov_reg(1, 9); // x1 = target pc (call only sets x0)
                self.call(O_TAIL_LOOKUP); // → x0 = target MACHINE CODE (0 = none)
                let fb = self.cbz_fwd(0); // 0 → normal exit to the loop
                // Tail-jump into the target's machine code with the CURRENT
                // ctx: its entry ABI is (ctx) with x19 = ctx, and its exit
                // (whichever trace ends the chain) flows back to OUR adapter
                // through the shared ctx. x19 is callee-saved, so it still
                // holds the ctx after the stub call.
                self.mov_reg(9, 0); // x9 = code
                self.mov_reg(0, 19); // x0 = ctx
                self.br_reg(9);
                self.patch_to_here(fb);
                self.ldr(9, 19, O_EXIT_PC);
                self.exit_tailcall_x9();
            }
            StackOp::Log(msg) => {
                self.mov_imm(1, msg.as_ptr() as u64);
                self.mov_imm(2, msg.len() as u64);
                self.call(O_LOG);
            }
            // ── privileged / non-lowered ──────────────────────────────────
            StackOp::Privileged { pc, raw } => {
                self.mov_imm(1, *pc);
                self.mov_imm(2, *raw as u64);
                self.call(O_PRIVILEGED);
                let cont = self.cbz_fwd(0); // 0 = Continue
                                            // Divert: exit TailCall(x0 - 1)
                self.sub_imm(0, 0, 1);
                self.str_(0, 19, O_EXIT_PC);
                self.mov_imm(0, TAG_TAILCALL as u64);
                self.exit_jump();
                self.patch_to_here(cont);
            }
            // ── integrity / instruction boundary ──────────────────────────
            StackOp::CheckCode { addr, expected, .. } => {
                let addr = *addr;
                // 1. a trapped memory access from the previous instruction bails.
                self.call(O_TAKE_TRAP);
                let no_trap = self.cbz_fwd(0);
                self.exit_bail();
                self.patch_to_here(no_trap);
                // 2. a pended interrupt exits to deliver it.
                self.call(O_TAKE_IRQ);
                let no_irq = self.cbz_fwd(0);
                self.mov_imm(9, addr);
                self.exit_tailcall_x9();
                self.patch_to_here(no_irq);
                // 3. the op fuel.
                self.ldr32(9, 19, O_FUEL);
                let has_fuel = self.cbnz32_fwd(9);
                self.mov_imm(9, addr);
                self.exit_tailcall_x9();
                self.patch_to_here(has_fuel);
                self.sub_imm32(9, 9, 1);
                self.str32(9, 19, O_FUEL);
                // 4. the code-integrity re-read (skipped when the host's
                //    fence.i/satp contract covers self-modifying code).
                self.call(O_WANT_INTEG);
                let skip = self.cbz_fwd(0);
                for (i, b) in expected.iter().enumerate() {
                    self.mov_imm(1, addr.wrapping_add(i as u64));
                    self.call(O_READ_BYTE);
                    self.cmp_imm(0, *b as u16);
                    let matches = self.bcond_fwd(EQ);
                    self.mov_imm(9, addr);
                    self.exit_tailcall_x9();
                    self.patch_to_here(matches);
                }
                self.patch_to_here(skip);
                // 5. mark the instruction boundary.
                self.mov_imm(24, addr); // cur_inst_pc = addr
                                        // Commit the boundary by RESETTING the undo buffer to its base.
                                        // Committed entries are never rolled back (only the current
                                        // instruction's are, on Bail), and a loop RE-EXECUTES its
                                        // StoreRegs, so without the reset the fixed-size undo buffer
                                        // overflows across iterations and clobbers the ctx fields that
                                        // follow it (regs, host, ...) — the cause of a prior segfault.
                self.add_imm(22, 19, O_UNDO_BUF); // x22 = undo ptr = base
                self.mov_reg(23, 22); // x23 = mark = base
            }
            // ── control flow ─────────────────────────────────────────────
            StackOp::LoopBegin { .. } => {
                self.loop_stack.push(self.off());
            }
            StackOp::LoopEnd => {
                self.loop_stack.pop();
            }
            StackOp::Br { relative_depth, .. } => {
                // Back-edge to the loop `relative_depth` levels up the static
                // nesting. No truncate (see `loop_stack`): the stack keeps the
                // full nesting so later `Br`s resolve correctly.
                let idx = self.loop_stack.len() - *relative_depth as usize;
                let target = self.loop_stack[idx];
                self.b_to(target);
            }
            StackOp::IfBegin => {
                self.pop(9); // the cond
                let fixup = self.cbz_fwd(9); // cond == 0 -> else
                self.if_stack.push(fixup);
            }
            StackOp::IfElse => {
                let b_end = self.b_fwd(); // -> end
                let cbz_fixup = self.if_stack.pop().unwrap();
                self.patch_to_here(cbz_fixup); // the cbz -> the else start
                self.if_stack.push(b_end);
            }
            StackOp::IfEnd => {
                let fixup = self.if_stack.pop().unwrap();
                self.patch_to_here(fixup);
            }
            // ── FP (via the shared helpers, bit-exact) ────────────────────
            StackOp::FloatUnary { width, op } => {
                self.pop(9);
                self.mov_imm(0, *width as u64);
                self.mov_imm(1, *op as u64);
                self.mov_reg(2, 9);
                self.call_pure(O_FP_UNARY);
                self.push(0);
            }
            StackOp::FloatBinary { width, op } => {
                self.pop(9); // b
                self.pop(10); // a
                self.mov_imm(0, *width as u64);
                self.mov_imm(1, *op as u64);
                self.mov_reg(2, 10);
                self.mov_reg(3, 9);
                self.call_pure(O_FP_BINARY);
                self.push(0);
            }
            StackOp::FloatLt { width } => self.lower_fp_cmp(0, *width),
            StackOp::FloatEq { width } => self.lower_fp_cmp(1, *width),
            StackOp::FloatUnordered { width } => self.lower_fp_cmp(2, *width),
            StackOp::IntToFloat {
                width,
                signed,
                int_width,
            } => {
                self.pop(9);
                self.mov_imm(0, *width as u64);
                self.mov_imm(1, *signed as u64);
                self.mov_imm(2, *int_width as u64);
                self.mov_reg(3, 9);
                self.call_pure(O_INT_TO_FP);
                self.push(0);
            }
            StackOp::FloatToInt {
                width,
                signed,
                int_width,
            } => {
                self.pop(9);
                self.mov_imm(0, *width as u64);
                self.mov_imm(1, *signed as u64);
                self.mov_imm(2, *int_width as u64);
                self.mov_reg(3, 9);
                self.call_pure(O_FP_TO_INT);
                self.push(0);
            }
            StackOp::FloatConvert { from, to } => {
                self.pop(9);
                self.mov_imm(0, *from as u64);
                self.mov_imm(1, *to as u64);
                self.mov_reg(2, 9);
                self.call_pure(O_FP_CONVERT);
                self.push(0);
            }
        }
    }
    fn lower_fp_cmp(&mut self, kind: u8, width: vane_target_core::FloatWidth) {
        self.pop(9); // b
        self.pop(10); // a
        self.mov_imm(0, kind as u64);
        self.mov_imm(1, width as u64);
        self.mov_reg(2, 10);
        self.mov_reg(3, 9);
        self.call_pure(O_FP_CMP);
        self.push(0);
    }
}

/// Compute the maximum value-stack depth of a trace (for the `STACK_CAP`
/// check). Returns `None` if the trace is malformed (negative depth).
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
            StackOp::Privileged { .. } | StackOp::CheckCode { .. } => (0, 0),
            StackOp::LoopBegin { .. } | StackOp::LoopEnd => (0, 0),
            StackOp::Br { .. } => (0, 0),
            StackOp::IfBegin => (1, 0),
            StackOp::IfElse | StackOp::IfEnd => (0, 0),
            StackOp::Trunc32 | StackOp::SignExt32 | StackOp::AsSigned64 | StackOp::AsUnsigned64 => {
                (1, 1)
            }
            // Unary FP ops.
            StackOp::FloatUnary { .. }
            | StackOp::IntToFloat { .. }
            | StackOp::FloatToInt { .. }
            | StackOp::FloatConvert { .. } => (1, 1),
            // Binary ALU / comparisons / binary FP.
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

/// Compile a `StackOp` trace to aarch64 machine code (the W^X region +
/// entry point). Err for a trace the backend can't lower (the caller falls
/// back to the interpreter).
pub fn compile(ops: &[StackOp]) -> Result<JitTrace, &'static str> {
    let max_depth = max_stack_depth(ops).ok_or("native-jit: malformed trace (stack underflow)")?;
    if max_depth > crate::ctx::STACK_CAP {
        return Err("native-jit: value-stack depth exceeds the ctx capacity");
    }
    // The undo buffer is reset at each CheckCode (instruction boundary), so it
    // only holds ONE instruction's register writes. Guard against a malformed
    // trace whose per-instruction StoreReg count would overflow it (real
    // rv_emit traces emit a CheckCode per instruction, so this never fires; a
    // hand-written trace with a CheckCode-less loop would otherwise overflow
    // the buffer into the adjacent ctx fields).
    let mut run = 0usize;
    let mut max_run = 0usize;
    for op in ops {
        match op {
            StackOp::StoreReg(r) if *r != 0 => {
                run += 1;
                max_run = max_run.max(run);
            }
            StackOp::CheckCode { .. } => run = 0,
            _ => {}
        }
    }
    if max_run > crate::ctx::UNDO_CAP {
        return Err("native-jit: undo-buffer overflow between instruction boundaries");
    }
    let mut a = Asm::new();
    a.prologue();
    // Codegen must never panic (a panic would abort the whole emulator). Any
    // bug in the lowering is caught and reported so the caller falls back to
    // the interpreter for this trace.
    let lowered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (i, op) in ops.iter().enumerate() {
            a.lower(op, i);
        }
    }));
    if lowered.is_err() {
        return Err("native-jit: codegen panicked (falling back to the interpreter)");
    }
    a.epilogue();
    let code = a.buf;
    let mut region =
        ExecRegion::new(code.len().max(4096)).map_err(|_| "native-jit: mmap failed")?;
    let base = region.write(&code).ok_or("native-jit: exec region full")?;
    // Safety: the region holds complete, valid machine code; no further writes.
    unsafe {
        region
            .finalize()
            .map_err(|_| "native-jit: mprotect failed")?
    };
    // Safety: `base` is the executable entry point of the finalized region.
    let f: NativeTraceFn = unsafe { core::mem::transmute(base) };
    Ok(JitTrace { code: f, region })
}

// Fix a typo: the FloatLt arm used a stale helper name.
#[allow(dead_code)]
fn _unused() {}

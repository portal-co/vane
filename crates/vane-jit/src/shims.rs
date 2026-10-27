//! The host shims a native trace calls for the cold machine operations
//! (PLAN_NATIVE_JIT Phase 2). Each is a C-ABI fn stored in the `TraceCtx`; the
//! generated code calls it indirectly. The host-dependent shims are generic
//! over the reactor `R` (monomorphized, like the rest of the trace engine);
//! the pure FP helpers are non-generic.
//!
//! # Safety
//! The shims deref the `ctx` and `host` raw pointers. This is sound because
//! the adapter keeps the `TraceCtx` and the host alive for the duration of the
//! native call, and the generated code is paused (in the `blr`) while a shim
//! runs — no concurrent access.

use crate::ctx::{TraceCtx, UndoEntry, STACK_CAP, UNDO_CAP};
use vane_arch::interp::{
    float_binary, float_convert, float_eq, float_lt, float_to_int, float_unary, float_unordered,
    int_to_float, LocalReactor, LocalStackHost, PrivOutcome,
};
use vane_arch::{FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StateSlot};

/// `&mut LocalStackHost<R>` from the ctx's opaque host pointer.
///
/// # Safety: see the module docs — the host outlives the native call.
unsafe fn host<'a, R: LocalReactor>(ctx: *mut TraceCtx) -> &'a mut LocalStackHost<R> {
    &mut *((*ctx).host as *mut LocalStackHost<R>)
}

// ── discriminant ↔ enum conversions (must match the codegen's `as u8`) ──────
fn mw(d: u8) -> MemWidth {
    match d {
        0 => MemWidth::B1,
        1 => MemWidth::B2,
        2 => MemWidth::B4,
        _ => MemWidth::B8,
    }
}
fn fw(d: u8) -> FloatWidth {
    if d == 0 {
        FloatWidth::F32
    } else {
        FloatWidth::F64
    }
}
fn fu(d: u8) -> FloatUnaryOp {
    match d {
        0 => FloatUnaryOp::Abs,
        1 => FloatUnaryOp::Neg,
        _ => FloatUnaryOp::Sqrt,
    }
}
fn fb(d: u8) -> FloatBinOp {
    match d {
        0 => FloatBinOp::Add,
        1 => FloatBinOp::Sub,
        2 => FloatBinOp::Mul,
        3 => FloatBinOp::Div,
        4 => FloatBinOp::Min,
        _ => FloatBinOp::Max,
    }
}

// ── host-dependent shims (generic over the reactor) ─────────────────────────
pub unsafe extern "C" fn shim_mem_load<R: LocalReactor>(
    ctx: *mut TraceCtx,
    addr: u64,
    width: u8,
    signed: u8,
) -> u64 {
    host::<R>(ctx)
        .reactor
        .lr_load_mem(addr, mw(width), signed != 0)
}
pub unsafe extern "C" fn shim_mem_store<R: LocalReactor>(
    ctx: *mut TraceCtx,
    addr: u64,
    width: u8,
    val: u64,
) {
    host::<R>(ctx).reactor.lr_store_mem(addr, mw(width), val);
}
pub unsafe extern "C" fn shim_privileged<R: LocalReactor>(
    ctx: *mut TraceCtx,
    pc: u64,
    raw: u32,
) -> u64 {
    let h = host::<R>(ctx);
    h.regs_dirty = true;
    // 0 = Continue; else the Divert target + 1 (a guest pc of u64::MAX is
    // impossible, so 0 is unambiguous — and `cbz` is a cheap test).
    match h.reactor.lr_privileged(pc, raw, &mut h.regs) {
        PrivOutcome::Continue => 0,
        PrivOutcome::Divert(a) => a.wrapping_add(1),
    }
}
pub unsafe extern "C" fn shim_take_trap<R: LocalReactor>(ctx: *mut TraceCtx) -> u32 {
    host::<R>(ctx).reactor.lr_take_pending_trap() as u32
}
pub unsafe extern "C" fn shim_take_irq<R: LocalReactor>(ctx: *mut TraceCtx) -> u32 {
    host::<R>(ctx).reactor.lr_take_irq_check() as u32
}
pub unsafe extern "C" fn shim_read_byte<R: LocalReactor>(ctx: *mut TraceCtx, addr: u64) -> u64 {
    host::<R>(ctx).reactor.lr_read_byte(addr) as u64
}
pub unsafe extern "C" fn shim_want_integrity<R: LocalReactor>(ctx: *mut TraceCtx) -> u32 {
    host::<R>(ctx).check_code_integrity as u32
}
pub unsafe extern "C" fn shim_ecall<R: LocalReactor>(ctx: *mut TraceCtx) -> u32 {
    let h = host::<R>(ctx);
    h.pending_ecall = true;
    1 // LocalStackHost::ecall always continues the trace
}
pub unsafe extern "C" fn shim_state_load<R: LocalReactor>(
    ctx: *mut TraceCtx,
    name_ptr: u64,
    name_len: u64,
) -> u64 {
    let name = core::str::from_utf8_unchecked(core::slice::from_raw_parts(
        name_ptr as *const u8,
        name_len as usize,
    ));
    host::<R>(ctx).sync_load_state(&StateSlot::new(name))
}
pub unsafe extern "C" fn shim_state_store<R: LocalReactor>(
    ctx: *mut TraceCtx,
    name_ptr: u64,
    name_len: u64,
    val: u64,
) {
    let name = core::str::from_utf8_unchecked(core::slice::from_raw_parts(
        name_ptr as *const u8,
        name_len as usize,
    ));
    host::<R>(ctx).sync_store_state(&StateSlot::new(name), val);
}
pub unsafe extern "C" fn shim_log<R: LocalReactor>(ctx: *mut TraceCtx, msg_ptr: u64, msg_len: u64) {
    let msg = core::str::from_utf8_unchecked(core::slice::from_raw_parts(
        msg_ptr as *const u8,
        msg_len as usize,
    ));
    host::<R>(ctx).sync_log(msg);
}

// ── pure FP helpers (non-generic; call the shared interpreter helpers) ──────
pub unsafe extern "C" fn shim_fp_unary(width: u8, op: u8, a: u64) -> u64 {
    float_unary(fw(width), fu(op), a)
}
pub unsafe extern "C" fn shim_fp_binary(width: u8, op: u8, a: u64, b: u64) -> u64 {
    float_binary(fw(width), fb(op), a, b)
}
pub unsafe extern "C" fn shim_fp_cmp(kind: u8, width: u8, a: u64, b: u64) -> u64 {
    let r = match kind {
        0 => float_lt(fw(width), a, b),
        1 => float_eq(fw(width), a, b),
        _ => float_unordered(fw(width), a, b),
    };
    r as u64
}
pub unsafe extern "C" fn shim_int_to_fp(width: u8, signed: u8, int_width: u8, val: u64) -> u64 {
    int_to_float(fw(width), signed != 0, int_width, val)
}
pub unsafe extern "C" fn shim_fp_to_int(width: u8, signed: u8, int_width: u8, val: u64) -> u64 {
    float_to_int(fw(width), signed != 0, int_width, val)
}
pub unsafe extern "C" fn shim_fp_convert(from: u8, to: u8, val: u64) -> u64 {
    float_convert(fw(from), fw(to), val)
}

/// Build the `TraceCtx` for a native trace run against `host`: the shim table,
/// the register-cache pointer (`host.regs`), and a zeroed value stack / undo
/// buffer / fuel.
pub fn build_ctx<R: LocalReactor>(host: &mut LocalStackHost<R>) -> TraceCtx {
    TraceCtx {
        stack: [0; STACK_CAP],
        undo_buf: [UndoEntry { reg: 0, old: 0 }; UNDO_CAP],
        regs: host.regs.as_mut_ptr(),
        host: host as *mut _ as *mut core::ffi::c_void,
        depth: 0,
        undo_len: 0,
        undo_mark: 0,
        fuel: 4096,
        cur_inst_pc: 0,
        exit_pc: 0,
        mem_load: shim_mem_load::<R>,
        mem_store: shim_mem_store::<R>,
        privileged: shim_privileged::<R>,
        take_trap: shim_take_trap::<R>,
        take_irq: shim_take_irq::<R>,
        read_byte: shim_read_byte::<R>,
        want_integrity: shim_want_integrity::<R>,
        state_load: shim_state_load::<R>,
        state_store: shim_state_store::<R>,
        ecall: shim_ecall::<R>,
        log: shim_log::<R>,
        fp_unary: shim_fp_unary,
        fp_binary: shim_fp_binary,
        fp_cmp: shim_fp_cmp,
        int_to_fp: shim_int_to_fp,
        fp_to_int: shim_fp_to_int,
        fp_convert: shim_fp_convert,
    }
}

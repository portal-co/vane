//! Native JIT for vane `StackOp` traces (PLAN_NATIVE_JIT Phase 2).
//!
//! Hand-rolled per-host-target codegen (aarch64 first) that lowers a `StackOp`
//! trace to executable machine code. The compiled artifact is registered into
//! the trace engine's `compiled` registry as a `vane_arch::interp::CompiledTrace`
//! whose `f` is [`native_adapter`] — a thin shim that drives the native entry —
//! sharing the registry with the interpreter/AOT/Weval producers.
//!
//! This crate is host-only (`std`) and behind vane's `native-jit` feature (off
//! by default), so the wasm/js-host and stable-toolchain consumers are
//! unaffected.

use vane_arch::interp::{CompiledTrace, Exit, LocalReactor, LocalStackHost};
use vane_target_core::StackOp;

pub mod ctx;
pub mod mem;
pub mod shims;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub mod arun;
#[cfg(target_arch = "aarch64")]
pub mod fiber;

// ── the native-trace exit tags ──────────────────────────────────────────────
// The native entry returns one of these (in `x0`); the exit pc is written to
// `ctx.exit_pc` (the `TailCall`/`Bail` target, or the `Trap` op index).
pub const TAG_TAILCALL: u8 = 0;
pub const TAG_TRAP: u8 = 1;
pub const TAG_ECALL: u8 = 2;
pub const TAG_BAIL: u8 = 3;

/// The native entry-point signature every backend produces. `ctx` is the
/// machine-state context block; `ops`/`ops_len` are the trace's `StackOp`s
/// (alive for the call; the codegen bakes the `Log`/`Trap`/state-slot payloads
/// as immediates pointing into them). Returns the exit tag.
pub type NativeTraceFn = unsafe extern "C" fn(*mut ctx::TraceCtx, *const StackOp, usize) -> u8;

/// A compiled native trace: the executable entry point plus the `ExecRegion`
/// (kept alive — dropping it would unmap the code).
pub struct JitTrace {
    pub code: NativeTraceFn,
    pub region: mem::ExecRegion,
}

/// Compile a `StackOp` trace to native code for the host target. Returns the
/// executable artifact, or `Err` for a trace the backend can't lower (the
/// caller then falls back to the interpreter producer).
pub fn compile_native(ops: &[StackOp]) -> Result<JitTrace, &'static str> {
    #[cfg(target_arch = "aarch64")]
    {
        aarch64::compile(ops)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = ops;
        Err("native-jit: no backend for this host target yet")
    }
}

/// The `CompiledTrace.f` for a native trace: build the `TraceCtx`, run the
/// native code, and translate the flat exit back into the interpreter's
/// `Exit` (including the `Bail` register-undo handoff to the dispatch loop).
///
/// This is the reference integration the differential tests drive.
pub fn native_adapter<R: LocalReactor>(
    host: &mut LocalStackHost<R>,
    ct: &CompiledTrace<R>,
) -> Exit {
    // Debug (VANE_JIT_TRACE): log the trace pc on change (and every 1M runs),
    // so a hang reveals the looping/diverging trace. The env check is cached so
    // the disabled path is ~free.
    fn trace_enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VANE_JIT_TRACE").is_some())
    }
    if trace_enabled() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNT: AtomicU64 = AtomicU64::new(0);
        static LASTPC: AtomicU64 = AtomicU64::new(u64::MAX);
        let c = COUNT.fetch_add(1, Ordering::Relaxed);
        let pc = host.pc;
        let prev = LASTPC.swap(pc, Ordering::Relaxed);
        if prev != pc || c.is_multiple_of(1_000_000) {
            eprintln!("[jit] #{c} pc={pc:#x}");
        }
    }
    let native = ct
        .native
        .as_ref()
        .expect("native_adapter: a Jit/Aot trace carries a native artifact");
    // Safety: `native.code` is the executable entry the backend produced (the
    // `owner` keeps the region alive), with the `NativeTraceFn` signature.
    let code: NativeTraceFn = unsafe { core::mem::transmute(native.code) };
    let mut ctx = shims::build_ctx::<R>(host);
    // Safety: the ctx is valid for the call; `ct.ops` outlives it (the
    // dispatch loop holds the `CompiledTrace`).
    let tag = unsafe { code(&mut ctx, ct.ops.as_ptr(), ct.ops.len()) };
    map_exit(host, &ctx, ct, tag)
}

/// Translate a native exit tag + the resulting `TraceCtx` into the
/// interpreter's `Exit` (shared by the sync adapter and the async driver).
/// Marks the register cache dirty and, on `Bail`, hands the current
/// instruction's register writes to the host's undo log.
pub(crate) fn map_exit<R: LocalReactor>(
    host: &mut LocalStackHost<R>,
    ctx: &ctx::TraceCtx,
    ct: &CompiledTrace<R>,
    tag: u8,
) -> Exit {
    // The native code wrote the register cache (`ctx.regs` aliases
    // `host.regs`); mark it dirty so the host flushes it.
    host.regs_dirty = true;
    match tag {
        TAG_TAILCALL => Exit::TailCall(ctx.exit_pc),
        TAG_ECALL => Exit::Ecall,
        TAG_TRAP => match ct.ops.get(ctx.exit_pc as usize) {
            Some(StackOp::Trap(msg)) => Exit::Trap(msg.clone()),
            _ => Exit::Trap("native-jit: bad Trap op index".into()),
        },
        TAG_BAIL => {
            // Hand the current instruction's register writes to the host's
            // undo log so the dispatch loop's `undo_pending_inst` rolls them
            // back before re-stepping the faulting instruction.
            for i in ctx.undo_mark..ctx.undo_len {
                let e = ctx.undo_buf[i as usize];
                host.undo_log.push((e.reg as u8, e.old));
            }
            host.undo_mark = 0;
            Exit::Bail(ctx.exit_pc)
        }
        _ => Exit::Trap("native-jit: bad exit tag".into()),
    }
}

/// Compile `ops` to a native trace and wrap it as a [`CompiledTrace`] for the
/// registry. Returns `None` if the backend can't lower the trace (the caller
/// falls back to [`vane_arch::interp::compile_trace_ptr`]).
pub fn compiled_trace<R: LocalReactor>(
    ops: vane_arch::Shared<Vec<StackOp>>,
) -> Option<CompiledTrace<R>> {
    // Debug (VANE_JIT_CC): count native compile ATTEMPTS vs SUCCESSES (a
    // trace that fails codegen is retried on every dispatch — counting both
    // reveals a failed-retry storm).
    let jt = compile_native(&ops);
    if std::env::var_os("VANE_JIT_CC").is_some() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
        static SUCC: AtomicU64 = AtomicU64::new(0);
        let a = ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
        let s = if jt.is_ok() {
            SUCC.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            SUCC.load(Ordering::Relaxed)
        };
        if a.is_multiple_of(256) {
            eprintln!("[jit] compiles attempts={a} success={s}");
        }
    }
    let jt = jt.ok()?;
    let owner: vane_arch::Shared<dyn core::any::Any + Send + Sync> = std::sync::Arc::new(jt.region);
    Some(CompiledTrace {
        f: native_adapter::<R>,
        ops,
        kind: vane_arch::interp::TraceBackend::Jit,
        native: Some(vane_arch::interp::NativeTrace {
            code: jt.code as *const core::ffi::c_void,
            owner,
        }),
    })
}

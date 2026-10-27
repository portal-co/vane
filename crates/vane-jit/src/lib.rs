//! Native JIT for vane `StackOp` traces (PLAN_NATIVE_JIT).
//!
//! Hand-rolled per-host-target codegen (aarch64 / x86_64 / riscv64) that lowers
//! a `StackOp` trace to executable machine code. The compiled artifact is
//! registered into the trace engine's `compiled` registry as a
//! `vane_arch::interp::CompiledTrace` whose `f` is a thin `extern "C"` shim
//! that drives the native entry — sharing the registry with the
//! interpreter/AOT/Weval producers.
//!
//! This crate is host-only (`std`) and behind vane's `native-jit` feature (off
//! by default), so the wasm/js-host and stable-toolchain consumers are
//! unaffected.

use vane_target_core::StackOp;

pub mod ctx;
pub mod mem;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

/// The result of a compiled trace, mirroring `vane_arch::interp::Exit` but in a
/// flat C-ABI form the native code can return in registers.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TraceExit {
    /// 0 = TailCall, 1 = Trap, 2 = Ecall, 3 = Bail.
    pub tag: u8,
    /// TailCall/Bail target guest pc.
    pub pc: u64,
}

impl TraceExit {
    pub const TAG_TAILCALL: u8 = 0;
    pub const TAG_TRAP: u8 = 1;
    pub const TAG_ECALL: u8 = 2;
    pub const TAG_BAIL: u8 = 3;
}

/// The native entry-point signature every backend produces. `ctx` is the
/// machine-state context block; `ops`/`ops_len` are the trace's `StackOp`s
/// (used by `CheckCode`; native code otherwise ignores them).
pub type NativeTraceFn =
    unsafe extern "C" fn(*mut ctx::TraceCtx, *const StackOp, usize) -> TraceExit;

/// A compiled native trace: the executable code pointer plus the ops it was
/// built from (kept alive for `CheckCode` + the interpreter fallback).
pub struct NativeTrace {
    pub code: NativeTraceFn,
    pub ops: std::vec::Vec<StackOp>,
}

/// Compile a `StackOp` trace to native code for the host target. Returns the
/// executable entry point, or `Err` for an op the backend can't lower (the
/// caller then falls back to the interpreter).
pub fn compile_native(ops: &[StackOp]) -> Result<NativeTraceFn, &'static str> {
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

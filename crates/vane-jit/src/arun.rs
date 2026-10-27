//! The async trace driver (PLAN_ASYNC_JIT Phase A): runs a native trace on a
//! fiber so a suspending host shim (e.g. a remote memory fetch from a
//! non-local host) can yield and resume.
//!
//! The generated trace code is identical to the sync path — it calls the
//! `TraceCtx` shim fn-pointers; only the shim *implementations* differ (the
//! async shims stash the pending op and `fiber_suspend`). The driver here
//! fulfills each pending op and resumes the fiber. A real non-local host
//! fulfills the op asynchronously (the caller awaits between `step` and
//! `resume_with`); the differential tests fulfill it synchronously.
//!
//! # Safety contract
//! The host and the `CompiledTrace` passed to [`AsyncTraceRun::start`] must
//! outlive the run, and the host must not be touched while the fiber runs
//! (i.e. only between `step` returning `Suspended` and the next `step`). The
//! run boxes the fiber and the ctx so their addresses stay stable across
//! suspensions even if the `AsyncTraceRun` itself is moved.

use crate::ctx::{pending_op, TraceCtx};
use crate::fiber::Fiber;
use crate::{map_exit, NativeTraceFn};
use vane_arch::interp::{CompiledTrace, Exit, LocalReactor, LocalStackHost};

/// A host op the trace suspended on; the driver fulfills it and resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOp {
    LoadMem { addr: u64, width: u8, signed: bool },
    StoreMem { addr: u64, width: u8, value: u64 },
}

/// The result of one [`AsyncTraceRun::step`]: the trace completed (with a raw
/// exit tag to pass to [`AsyncTraceRun::finish`]) or suspended on an op.
pub enum Step {
    Done(u8),
    Suspended(PendingOp),
}

/// A running (suspendable) native trace against an async host.
pub struct AsyncTraceRun {
    // Boxed: the fiber must not move while suspended (the context switch
    // saves/restores its context in place, and `CURRENT_FIBER` points at it).
    fiber: Box<Fiber>,
    // Boxed: the fiber holds a raw pointer into the ctx, so it must not move.
    ctx: Box<TraceCtx>,
}

impl AsyncTraceRun {
    /// Start running `ct` against `host` on a fresh fiber.
    ///
    /// Safety: see the module docs — `host` and `ct` must outlive the run.
    pub fn start<R: LocalReactor>(host: &mut LocalStackHost<R>, ct: &CompiledTrace<R>) -> Self {
        let native = ct
            .native
            .as_ref()
            .expect("AsyncTraceRun: a Jit/Aot trace carries a native artifact");
        // Safety: as in `native_adapter`.
        let code: NativeTraceFn = unsafe { core::mem::transmute(native.code) };
        let mut ctx = Box::new(crate::shims::build_async_ctx(host));
        let ctx_ptr: *mut TraceCtx = ctx.as_mut();
        let fiber = Box::new(Fiber::new(code, ctx_ptr, ct.ops.as_ptr(), ct.ops.len()));
        AsyncTraceRun { fiber, ctx }
    }

    /// Drive the trace until it completes or suspends. On `Suspended`, fulfill
    /// the [`PendingOp`] against the host (awaiting it if async), then call
    /// [`AsyncTraceRun::resume_with`] and `step` again.
    ///
    /// Safety: the host must be alive and exclusively available for this call.
    pub fn step(&mut self) -> Step {
        match self.fiber.run() {
            Some(tag) => Step::Done(tag),
            None => Step::Suspended(self.pending()),
        }
    }

    /// Provide the pending op's result and prepare to resume.
    pub fn resume_with(&mut self, result: u64) {
        self.ctx.pending_result = result;
        self.ctx.pending_op = pending_op::NONE;
    }

    fn pending(&self) -> PendingOp {
        match self.ctx.pending_op {
            pending_op::LOAD_MEM => PendingOp::LoadMem {
                addr: self.ctx.pending_a,
                width: self.ctx.pending_b as u8,
                signed: self.ctx.pending_c != 0,
            },
            pending_op::STORE_MEM => PendingOp::StoreMem {
                addr: self.ctx.pending_a,
                width: self.ctx.pending_b as u8,
                value: self.ctx.pending_c,
            },
            t => panic!("AsyncTraceRun: bad pending op tag {t}"),
        }
    }

    /// Map the completed exit tag to the interpreter's `Exit`.
    pub fn finish<R: LocalReactor>(
        self,
        tag: u8,
        host: &mut LocalStackHost<R>,
        ct: &CompiledTrace<R>,
    ) -> Exit {
        map_exit(host, &self.ctx, ct, tag)
    }
}

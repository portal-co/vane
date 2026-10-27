//! Native coroutine (fiber) support for async host shims.
//!
//! A compiled trace normally runs on the host stack and returns a final exit.
//! For an async host (a shim that must await — e.g. a remote memory fetch from
//! a non-local host), the trace instead runs on a separate fiber stack; a
//! suspending shim switches back to the adapter, which reports a suspension; on
//! resume the adapter switches back into the fiber and the shim continues. The
//! trace's machine state (value stack, undo buffer, pc) lives in `TraceCtx`, so
//! it survives the suspend; only the trace's call frames live on the fiber
//! stack, which is preserved across the switch.

#![cfg(target_arch = "aarch64")]

use crate::ctx::TraceCtx;
use crate::NativeTraceFn;
use core::cell::Cell;
use vane_target_core::StackOp;

/// Saved callee-saved context for a fiber switch: x19..=x30 plus SP.
#[repr(C)]
pub struct FiberContext {
    regs: [u64; 13],
}

impl FiberContext {
    const fn new() -> Self {
        FiberContext { regs: [0; 13] }
    }
    /// Set the stack pointer slot (index 12).
    fn set_sp(&mut self, sp: u64) {
        self.regs[12] = sp;
    }
    /// Set the entry/return-address slot (x30, index 11).
    fn set_entry(&mut self, entry: u64) {
        self.regs[11] = entry;
    }
}

/// Switch from `from` (saving the current context into it) to `to` (restoring
/// it). When control later switches back to `from`, this call returns.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn fiber_switch(_from: *mut FiberContext, _to: *const FiberContext) {
    core::arch::naked_asm!(
        // save x19..=x30 + sp into *from (x0)
        "stp x19, x20, [x0, #0]",
        "stp x21, x22, [x0, #16]",
        "stp x23, x24, [x0, #32]",
        "stp x25, x26, [x0, #48]",
        "stp x27, x28, [x0, #64]",
        "stp x29, x30, [x0, #80]",
        "mov x9, sp",
        "str x9, [x0, #96]",
        // restore x19..=x30 + sp from *to (x1)
        "ldp x19, x20, [x1, #0]",
        "ldp x21, x22, [x1, #16]",
        "ldp x23, x24, [x1, #32]",
        "ldp x25, x26, [x1, #48]",
        "ldp x27, x28, [x1, #64]",
        "ldp x29, x30, [x1, #80]",
        "ldr x9, [x1, #96]",
        "mov sp, x9",
        "ret",
    );
}

thread_local! {
    /// The fiber currently being driven (set by the adapter before switching).
    static CURRENT_FIBER: Cell<*mut Fiber> = const { Cell::new(core::ptr::null_mut()) };
}

/// A fiber that runs one compiled trace, able to suspend + resume.
pub struct Fiber {
    stack: Vec<u8>,
    fiber_ctx: FiberContext,
    main_ctx: FiberContext,
    code: NativeTraceFn,
    ctx: *mut TraceCtx,
    ops: *const StackOp,
    ops_len: usize,
    /// Exit tag produced when the trace completed (valid when `!suspended`).
    exit: u8,
    /// True when the trace suspended (a shim is awaiting) rather than exited.
    suspended: bool,
    /// True once the fiber has been entered at least once.
    started: bool,
}

/// The entry point a fresh fiber switches into. Runs the trace to completion
/// or a suspension, then switches back to the adapter.
extern "C" fn fiber_entry() -> ! {
    let fiber = CURRENT_FIBER.with(|c| c.get());
    let fiber = unsafe { &mut *fiber };
    let exit = unsafe { (fiber.code)(fiber.ctx, fiber.ops, fiber.ops_len) };
    fiber.exit = exit;
    fiber.suspended = false;
    // Switch back to the adapter for the last time. If we're ever resumed after
    // completion that's a bug.
    unsafe { fiber_switch(&mut fiber.fiber_ctx, &fiber.main_ctx) };
    unreachable!("fiber resumed after completion");
}

/// Yield from a suspending host shim. Switches back to the adapter, marking the
/// trace suspended. Returns (in the shim) when the adapter resumes the fiber.
///
/// # Safety
/// Only valid on a fiber stack while `CURRENT_FIBER` points at the live fiber.
pub unsafe fn fiber_suspend() {
    let fiber = CURRENT_FIBER.with(|c| c.get());
    let fiber = unsafe { &mut *fiber };
    fiber.suspended = true;
    unsafe { fiber_switch(&mut fiber.fiber_ctx, &fiber.main_ctx) };
    // Resumed: the awaited value is now available to the shim.
    fiber.suspended = false;
}

impl Fiber {
    /// Create a fiber that will run `code` over `ctx`/`ops`.
    pub fn new(
        code: NativeTraceFn,
        ctx: *mut TraceCtx,
        ops: *const StackOp,
        ops_len: usize,
    ) -> Self {
        // A generous stack for the trace's host-side call frames (prologue +
        // shim calls). The trace's own value stack lives in TraceCtx.
        let stack = vec![0u8; 64 * 1024];
        Fiber {
            stack,
            fiber_ctx: FiberContext::new(),
            main_ctx: FiberContext::new(),
            code,
            ctx,
            ops,
            ops_len,
            exit: 0,
            suspended: false,
            started: false,
        }
    }

    /// Run (or resume) the trace. Returns `Some(exit_tag)` on completion, or
    /// `None` if the trace suspended.
    pub fn run(&mut self) -> Option<u8> {
        CURRENT_FIBER.with(|c| c.set(self as *mut Fiber));
        if !self.started {
            self.started = true;
            // Set up the fiber's initial context: SP at the (16-aligned) top of
            // the stack, entry = fiber_entry.
            let top = self.stack.as_mut_ptr() as u64 + self.stack.len() as u64;
            self.fiber_ctx.set_sp(top & !15);
            self.fiber_ctx.set_entry(fiber_entry as *const () as u64);
        }
        unsafe { fiber_switch(&mut self.main_ctx, &self.fiber_ctx) };
        CURRENT_FIBER.with(|c| c.set(core::ptr::null_mut()));
        if self.suspended {
            None
        } else {
            Some(self.exit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};

    unsafe extern "C" fn completes(_c: *mut TraceCtx, _o: *const StackOp, _n: usize) -> u8 {
        42
    }

    #[test]
    fn fiber_runs_to_completion() {
        let mut f = Fiber::new(completes, core::ptr::null_mut(), core::ptr::null(), 0);
        assert_eq!(f.run(), Some(42));
    }

    static SUSPENDED_ONCE: AtomicBool = AtomicBool::new(false);

    unsafe extern "C" fn suspends_then_completes(
        _c: *mut TraceCtx,
        _o: *const StackOp,
        _n: usize,
    ) -> u8 {
        if !SUSPENDED_ONCE.swap(true, Ordering::SeqCst) {
            unsafe { fiber_suspend() };
        }
        7
    }

    #[test]
    fn fiber_suspends_and_resumes() {
        SUSPENDED_ONCE.store(false, Ordering::SeqCst);
        let mut f = Fiber::new(
            suspends_then_completes,
            core::ptr::null_mut(),
            core::ptr::null(),
            0,
        );
        assert_eq!(f.run(), None); // first run: suspends inside the shim
        assert_eq!(f.run(), Some(7)); // resume: shim continues, trace completes
    }

    // Suspend twice, then complete: the fiber stack frames must persist across
    // multiple suspensions.
    static SUSPEND_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    unsafe extern "C" fn suspends_twice(_c: *mut TraceCtx, _o: *const StackOp, _n: usize) -> u8 {
        while SUSPEND_COUNT.fetch_add(1, Ordering::SeqCst) < 2 {
            unsafe { fiber_suspend() };
        }
        99
    }

    #[test]
    fn fiber_suspends_twice() {
        SUSPEND_COUNT.store(0, Ordering::SeqCst);
        let mut f = Fiber::new(suspends_twice, core::ptr::null_mut(), core::ptr::null(), 0);
        assert_eq!(f.run(), None);
        assert_eq!(f.run(), None);
        assert_eq!(f.run(), Some(99));
    }
}

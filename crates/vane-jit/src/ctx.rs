//! The `TraceCtx` — the C-ABI context block a native-compiled trace runs
//! against. It carries (a) the hot data native code reads/writes directly (the
//! value stack, the register-cache pointer, the exit pc) and (b) function
//! pointers for the cold machine operations (memory translation + access,
//! privileged instructions, trap/IRQ polls). The function pointers are
//! implemented once as `extern "C"` shims over the reactor's existing
//! `LocalReactor` logic, so the JIT and the interpreter share one machine
//! semantics implementation.

use vane_target_core::StackOp;

/// Max value-stack depth a compiled trace supports. `StackOp` traces are
/// stack-machine code; the depth is statically bounded by the trace's ops and
/// is small in practice (register arithmetic). The codegen computes the exact
/// max depth per trace and refuses to compile one that would overflow.
pub const STACK_CAP: usize = 512;

/// The C-ABI context block. Layout is fixed (`repr(C)`) so the native code can
/// address fields by constant offset.
#[repr(C)]
pub struct TraceCtx {
    /// The value (operand) stack.
    pub stack: [u64; STACK_CAP],
    /// Current value-stack depth.
    pub depth: u64,
    /// Pointer to the register cache (`[u64; 32]`).
    pub regs: *mut u64,
    /// Out: the guest pc at exit (TailCall/Bail target).
    pub pc: u64,

    // ── cold machine ops (host function pointers) ─────────────────────────
    /// `mem_load(ctx, addr, width_log2, signed) -> value`.
    pub mem_load: unsafe extern "C" fn(*mut TraceCtx, u64, u8, u8) -> u64,
    /// `mem_store(ctx, addr, width_log2, value)`.
    pub mem_store: unsafe extern "C" fn(*mut TraceCtx, u64, u8, u64),
    /// `privileged(ctx, pc, raw) -> PrivOutcome as u64` (0 = continue, else the
    /// divert target pc + 1).
    pub privileged: unsafe extern "C" fn(*mut TraceCtx, u64, u32) -> u64,
    /// A memory access trapped since the last poll.
    pub take_trap: unsafe extern "C" fn(*mut TraceCtx) -> u8,
    /// A device (MMIO) access ran since the last poll (an IRQ may be pending).
    pub take_irq: unsafe extern "C" fn(*mut TraceCtx) -> u8,

    /// Opaque pointer back to the host (`LocalStackHost`/`Reactor`).
    pub host: *mut core::ffi::c_void,
}

impl TraceCtx {
    /// The guest pc a `CheckCode` integrity check re-reads.
    /// (`CheckCode` compares baked bytes; native code calls `mem_load`.)
    pub unsafe fn push(&mut self, v: u64) {
        self.stack[self.depth as usize] = v;
        self.depth += 1;
    }
    pub unsafe fn pop(&mut self) -> u64 {
        self.depth -= 1;
        self.stack[self.depth as usize]
    }
}

/// The `ops` slice passed to a native trace is never mutated by it.
pub fn ops_slice<'a>(ops: *const StackOp, len: usize) -> &'a [StackOp] {
    unsafe { core::slice::from_raw_parts(ops, len) }
}

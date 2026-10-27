//! The `TraceCtx` C-ABI context block — the shared state a native trace reads
//! and writes (PLAN_NATIVE_JIT Phase 2). Laid out `repr(C)` so the generated
//! code can address fields by fixed byte offsets (via `core::mem::offset_of!`).
//!
//! # Model
//! - The **value stack** (`stack`/`depth`) is the StackOp operand stack, held
//!   inline so pushes/pops are single native stores/loads.
//! - The **register cache** (`regs`) points at the host's `[u64; 32]` cache, so
//!   register reads/writes are inline (no host call). `x0` is handled at
//!   compile time (`LoadReg(0)` → `PushImm(0)`; `StoreReg(0)` → `Drop`).
//! - The **undo buffer** (`undo_buf`/`undo_len`/`undo_mark`) records `(reg,
//!   old)` per register write for trap-atomic `Bail`; on a trap exit the
//!   adapter copies the post-`undo_mark` entries into the host's `undo_log` so
//!   the dispatch loop's `undo_pending_inst` rolls the instruction back.
//! - The **host shims** are the cold machine operations (memory, privileged,
//!   the CheckCode trap/irq/integrity reads, the FP helpers, ...). Each is a
//!   C-ABI fn pointer the generated code calls indirectly.

use core::ffi::c_void;

/// Value-stack capacity. Codegen computes the trace's exact maximum depth and
/// refuses (falls back to the interpreter) if it exceeds this.
pub const STACK_CAP: usize = 512;
/// Undo-buffer capacity (one entry per register write in the trace). Codegen
/// counts the `StoreReg`s and refuses if it exceeds this.
pub const UNDO_CAP: usize = 512;

/// One undo-buffer entry: a register write's `(reg, old_value)`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UndoEntry {
    pub reg: u64,
    pub old: u64,
}

// ── Host-shim signatures ────────────────────────────────────────────────────
// All take the ctx first (except the pure FP helpers). `unsafe extern "C"`.
pub type ShimMemLoad = unsafe extern "C" fn(*mut TraceCtx, u64, u8, u8) -> u64;
pub type ShimMemStore = unsafe extern "C" fn(*mut TraceCtx, u64, u8, u64);
/// Returns the `Divert` target, or `u64::MAX` for `PrivOutcome::Continue`.
pub type ShimPrivileged = unsafe extern "C" fn(*mut TraceCtx, u64, u32) -> u64;
/// A boolean host query (take_trap / take_irq / want_integrity / ecall).
pub type ShimFlag = unsafe extern "C" fn(*mut TraceCtx) -> u32;
pub type ShimReadByte = unsafe extern "C" fn(*mut TraceCtx, u64) -> u64;
/// State-slot ops: `(ctx, name_ptr, name_len)`.
pub type ShimStateLoad = unsafe extern "C" fn(*mut TraceCtx, u64, u64) -> u64;
pub type ShimStateStore = unsafe extern "C" fn(*mut TraceCtx, u64, u64, u64);
/// `Log`: `(ctx, msg_ptr, msg_len)`.
pub type ShimLog = unsafe extern "C" fn(*mut TraceCtx, u64, u64);
// Pure FP helpers (no ctx): discriminants are baked in by the codegen.
pub type ShimFpUnary = unsafe extern "C" fn(u8, u8, u64) -> u64;
pub type ShimFpBinary = unsafe extern "C" fn(u8, u8, u64, u64) -> u64;
/// FP comparison: `(kind, width, a, b)`; kind 0=lt 1=eq 2=unordered.
pub type ShimFpCmp = unsafe extern "C" fn(u8, u8, u64, u64) -> u64;
pub type ShimIntToFp = unsafe extern "C" fn(u8, u8, u8, u64) -> u64;
pub type ShimFpToInt = unsafe extern "C" fn(u8, u8, u8, u64) -> u64;
pub type ShimFpConvert = unsafe extern "C" fn(u8, u8, u64) -> u64;

/// The context block a native trace operates on.
#[repr(C)]
pub struct TraceCtx {
    /// The value stack.
    pub stack: [u64; STACK_CAP],
    /// The undo buffer.
    pub undo_buf: [UndoEntry; UNDO_CAP],
    /// The register cache (points at the host's `[u64; 32]`).
    pub regs: *mut u64,
    /// The opaque host pointer (cast to the host type by the shims).
    pub host: *mut c_void,
    /// Current value-stack depth.
    pub depth: u32,
    /// Current undo-buffer length.
    pub undo_len: u32,
    /// The committed undo mark (set at each instruction boundary).
    pub undo_mark: u32,
    /// The op fuel (decremented at each `CheckCode`; yields on 0).
    pub fuel: u32,
    /// The current instruction's pc (the `Bail` target).
    pub cur_inst_pc: u64,
    /// The exit pc — the `TailCall`/`Bail` target, or (for `Trap`) the op index.
    pub exit_pc: u64,
    // ── host shims ──────────────────────────────────────────────────────────
    pub mem_load: ShimMemLoad,
    pub mem_store: ShimMemStore,
    pub privileged: ShimPrivileged,
    pub take_trap: ShimFlag,
    pub take_irq: ShimFlag,
    pub read_byte: ShimReadByte,
    pub want_integrity: ShimFlag,
    pub state_load: ShimStateLoad,
    pub state_store: ShimStateStore,
    pub ecall: ShimFlag,
    pub log: ShimLog,
    pub fp_unary: ShimFpUnary,
    pub fp_binary: ShimFpBinary,
    pub fp_cmp: ShimFpCmp,
    pub int_to_fp: ShimIntToFp,
    pub fp_to_int: ShimFpToInt,
    pub fp_convert: ShimFpConvert,
}

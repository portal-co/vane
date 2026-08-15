#![no_std]
extern crate alloc;

/// Context threaded through [`WasmJit::jit`].
///
/// An embedder (e.g. speet's runtime, resolving indices from a live
/// `OobConfig`/dynamic-JIT-table setup at JIT-request time) implements this
/// to hand the renderer its [`WasmAbiConfig`].
pub trait WasmJitCtx {
    fn wasm_abi(&self) -> WasmAbiConfig;
}

#[derive(Clone)]
pub enum JitOpcode<'a> {
    Operator { op: wasmparser::Operator<'a> },
}

pub trait WasmJit {
    fn jit<'a>(
        &'a self,
        ctx: &'a (dyn WasmJitCtx + 'a),
    ) -> impl Iterator<Item = JitOpcode<'a>> + 'a;
}

/// ABI/layout configuration for [`vane_arch::template::render_stack_to_wasm`]'s
/// lowering of [`StackOp`](vane_target_core::StackOp) into WASM.
///
/// The emitted function's own signature must be exactly `(reg_0..reg_{num_regs-1}:
/// i64, target_pc: i64) -> (reg_0..reg_{num_regs-1}: i64)` — i.e. `reg_base` is
/// always `0` and `target_pc_local` is always `num_regs` in practice, but both
/// are spelled out explicitly rather than assumed, since a future embedder may
/// want extra leading locals. This matches speet's own `OobConfig` shared
/// function-type convention (arch regs + `target_pc: i64` -> arch regs) so a
/// JIT-compiled function can be spliced directly into the same
/// `return_call`/`return_call_indirect` dispatch chain as AOT-compiled code —
/// see `speet-link-core::oob::OobConfig` and `speet-interp`'s lookup stub.
#[derive(Clone, Copy, Debug)]
pub struct WasmAbiConfig {
    /// Local index of architectural register 0. Registers occupy the
    /// contiguous range `reg_base .. reg_base + num_regs`.
    pub reg_base: u32,
    /// Number of architectural registers exposed as locals/params.
    pub num_regs: u32,
    /// Local index holding `target_pc: i64` on entry.
    pub target_pc_local: u32,
    /// Scratch i64 local, used internally to reorder stack values (e.g. when
    /// wrapping a 64-bit operand to i32 while a sibling operand sits above
    /// it on the stack). Must not alias any register or `target_pc_local`.
    pub scratch_a: u32,
    /// Second scratch i64 local, needed alongside `scratch_a` for binary
    /// 32-bit ops and for the divide/remainder-by-zero check (which must
    /// hold both operands live across a value-returning `if`/`else`).
    pub scratch_b: u32,
    /// WASM memory index guest `LoadMem`/`StoreMem`/`CheckCode` address.
    ///
    /// Addresses are treated as flat 32-bit offsets into this memory (the
    /// low 32 bits of the 64-bit guest address, via `i32.wrap_i64` /
    /// compile-time truncation for `CheckCode`'s constant address) — this
    /// assumes a flat/direct-mapped guest memory model (e.g.
    /// `os-page::backends::LinearHost`, "the thin runtime / JIT fast path"
    /// memory backend), not a `SharedPageTable`-translated one. Full
    /// page-table-aware address translation is out of scope for this
    /// renderer; a future embedder targeting `SharedPageTable` guests needs
    /// its own lowering.
    pub mem_idx: u32,
    /// Host import function index for `Ecall`.
    ///
    /// Type: `(num_regs x i64) -> (num_regs x i64)` — the JIT'd function
    /// pushes its current register locals as arguments and overwrites them
    /// with the call's results. There is currently no channel for an ecall
    /// to signal guest process exit from inside JIT'd code (the JS backend's
    /// `if(_ecall?.kind==='exit')return _ecall;` has no WASM equivalent yet)
    /// — deferred; see `StackOp::Ecall`'s doc comment.
    pub ecall_func_idx: u32,
    /// Host import function index for `CheckCode`'s cache-invalidation path.
    ///
    /// Type: `(pc: i64) -> ()`, called before the tail-call-equivalent
    /// re-dispatch to `lookup_stub_func_idx`.
    pub jit_invalidate_func_idx: u32,
    /// WASM function index of the OOB lookup stub that `TailCall` and a
    /// failed `CheckCode` both `return_call` into — i.e.
    /// `OobConfig::lookup_stub_func_idx` on the speet side.
    pub lookup_stub_func_idx: u32,
    /// Named-state-slot → local-index table for
    /// `StackOp::LoadState`/`StoreState` (architectural state an ISA
    /// frontend addresses by name rather than by an indexed
    /// `LoadReg`/`StoreReg` register — e.g. AArch64, which has no
    /// `LoadReg`/`StoreReg` at all and represents its GPRs, NZCV flags, and
    /// V-registers exclusively as named slots; see `vane-aarch64::state`).
    ///
    /// Entries' indices fall **within** `reg_base .. reg_base + num_regs` —
    /// the same params/results range `LoadReg`/`StoreReg` index into, not
    /// extra locals beyond it. This matters for correctness, not just
    /// convention: params/results are what `TailCall`/`CheckCode`/the OOB
    /// dispatch chain thread across `return_call`/`return_call_indirect`
    /// boundaries (see `OobConfig`'s shared calling convention) — a plain
    /// local beyond `scratch_b` would silently reset to zero on every call,
    /// losing e.g. an AArch64 X0 across the very first dispatch. An
    /// architecture may address the pool entirely by name (AArch64,
    /// `num_regs` = its total named-slot count, no `LoadReg`/`StoreReg`
    /// ever emitted) or entirely by index (RISC-V, `state_slots: &[]`); one
    /// index must not be claimed by both a `state_slots` entry and a
    /// `LoadReg`/`StoreReg` numeric register in the same embedding.
    /// Looked up by linear scan of `StateSlot::as_str()` against the first
    /// tuple element (the table is small and built once per ISA crate as a
    /// `const`, so this needs no runtime hashing). An architecture with no
    /// named state (RISC-V, whose GPRs are all `LoadReg`/`StoreReg`) passes
    /// `&[]`; a name that isn't in the table lowers to `Unreachable`, same
    /// as today's blanket behavior — an unmapped slot name is a genuine bug,
    /// not something to silently zero.
    pub state_slots: &'static [(&'static str, u32)],
}

# PLAN_NATIVE_JIT.md — Native JIT + uniform compiled-trace function pointers

Status: **draft for review** (no code yet). Supersedes the "compiled registry holds
`TraceFn` closures" design sketched in PLAN_TRACE_CUTOVER.md §4.2.

## 0. Goal

Two coupled changes to the trace engine:

1. **Native JIT for the trace engine.** Lower `StackOp` traces to *native machine
   code* for the three host architectures we care about — **aarch64, x86_64,
   riscv64** — instead of interpreting them (the current dense interpreter) or
   round-tripping through WASM (the vane-patch Weval/patcher path).

2. **A uniform compiled-trace ABI based on raw function pointers.** Today the
   `compiled` registry is `BTreeMap<u64, TraceFn>` where `TraceFn =
   Arc<dyn Fn(&mut LocalStackHost<R>) -> Exit>` — a *closure* that captures the
   `Shared<Vec<StackOp>>` and calls back into the interpreter. Replace that
   function-object cache with **raw function pointers** that all share one
   signature and one context block, so that every producer — the **native JIT**,
   an **AOT** artifact, **Weval** (WASM→native), and the **default interpreter**
   (a fallback that just interprets the `StackOp`s) — registers the same kind of
   callable.

The payoff: a hot trace runs as native code with no per-op dispatch, no
value-stack `Vec` traffic, and no `Arc<dyn Fn>` indirect-call overhead; and the
registry becomes a flat `pc -> code pointer` map that any backend can fill.

## 1. Current architecture (what we're replacing)

### 1.1 The trace pipeline

```
guest pc ──► trace_cache (pc -> Shared<Vec<StackOp>>)     [compile on miss]
                 │
                 ▼
        dispatch loop (vane-meta-gen macro):
                 │  ┌─ compiled registry hit? ──► cf(&mut host)   [TraceFn closure]
                 │  └─ else ──► interpret_async_fueled(&ops, &mut host, fuel)
                 ▼
              Exit { TailCall(pc) | Trap | Ecall | Bail(pc) }
```

- `trace_cache: RefCell<BTreeMap<u64, Shared<Vec<StackOp>>>>` — the decoded
  trace (a stack-machine op list). Filled by `rv_emit`/`a64_emit` (the
  `TemplateJit` emitters).
- `compiled: RefCell<BTreeMap<u64, TraceFn>>` — the "function object cache" this
  plan replaces. Consulted *before* the interpreter each dispatch
  (vane-meta-gen `trace_run_fueled`, ~line 621).
- `compile_trace(ops) -> TraceFn` (vane-arch/src/interp.rs:1124) is the *only*
  producer today: it wraps `interpret_async_fueled`, capturing `ops` in an
  `Arc`/`Rc`. So a "compiled" trace today is just "the interpreter, pre-bound to
  its ops" — a closure object.

### 1.2 The interpreter's shape (what the JIT must reproduce)

`interpret_async_fueled` (vane-arch/src/interp.rs) is a loop over `ops: &[StackOp]`
with:

- a **value stack** `Vec<u64>` (the operand stack),
- a **loop stack** `Vec<usize>` (open `LoopBegin` instruction indices, for `Br`),
- the **`LocalStackHost`** as the machine-state context (see below),
- a per-trace **op fuel** budget (4096) so a trace yields for IRQ/ecall/fuel,
- **`CheckCode`** integrity re-reads (skipped when `check_code_integrity` is
  false — the running hart, per the fence.i/satp contract; see the fine-grained
  sfence.vma work),
- **trap atomicity** via an undo log (`note_inst_boundary` / `undo_pending_inst`)
  so a mid-instruction memory fault discards only that instruction's writes.

### 1.3 `LocalStackHost` — the context

`LocalStackHost<R: LocalReactor>` carries what a trace touches:

- `reactor: R` — the machine (behind `lr_*` calls),
- `pc: u64` — current guest pc,
- `regs: [u64; 32]` + `regs_dirty` — the **register cache** (hot path),
- `reg_cache: bool` — whether the cache is authoritative (false under js-host),
- `undo_log` / `undo_mark` — trap-atomicity,
- `pending_ecall`, `test_mode`, `check_code_integrity`.

The `LocalReactor` interface the ops call into (vane-arch/src/interp.rs):

```
lr_load_reg(r) -> u64        lr_store_reg(r, v)
lr_load_mem(addr, w, s)      lr_store_mem(addr, w, v)   // translate + access
lr_read_byte / lr_write_byte
lr_privileged(pc, raw, &mut regs) -> PrivOutcome        // StackOp::Privileged
lr_take_pending_trap()       lr_take_irq_check()
lr_load_all_regs / lr_store_all_regs                     // register cache sync
```

### 1.4 The existing "compiled code" producers (WASM, not native)

- **JS renderer** (`render_stack_to_js`, vane-arch/src/template.rs) — emits JS
  source; uses the `String` labels (`x{pc}`) and `CheckCode.miss_label` as the
  `jit_cache` key.
- **WASM renderer** (`render_stack_to_wasm`, vane-arch/src/template/render_wasm.rs)
  — emits a WASM function via `wasm-encoder`; ignores labels.
- **vane-patch** — the custom WASM binary patcher (`compile_trace_pack`) and the
  external **Weval** wrapper (`weval_specialize`). These compile *the whole
  interpreter module* against a snapshot and fill a WASM dispatch table — a
  WASM-level "JIT", not native codegen for individual traces.

None of these produce *native* code for a single `StackOp` trace. That is the
gap this plan fills.

## 2. The uniform compiled-trace ABI

### 2.1 The signature

One C-ABI signature for every producer:

```rust
/// The machine-state context a compiled trace runs against (§2.2).
/// `ops`/`ops_len` are the trace's `StackOp`s — used by the interpreter
/// producer and by `CheckCode`; native/JIT code ignores them (it *is* the code).
pub type CompiledTraceFn =
    unsafe extern "C" fn(ctx: *mut TraceCtx, ops: *const StackOp, ops_len: usize) -> TraceExit;
```

### 2.2 `TraceCtx` — a C-ABI context block

`LocalStackHost` is a Rust struct (not layout-stable, generic over `R`). Native
code can't call its trait methods directly, so we give it a `#[repr(C)]` shadow
that carries (a) the hot data the code touches directly and (b) function
pointers for the cold machine operations:

```rust
#[repr(C)]
pub struct TraceCtx {
    // ── hot data, accessed directly by compiled code ────────────────────
    pub regs: *mut u64,              // the [u64;32] register cache
    pub pc: *mut u64,                // current guest pc (read on exit)
    pub undo_len: *mut usize,        // undo-log length (trap atomicity)
    // ── cold machine ops, called as C function pointers ─────────────────
    pub mem_load:  extern "C" fn(*mut TraceCtx, addr: u64, width: u8, signed: u8) -> u64,
    pub mem_store: extern "C" fn(*mut TraceCtx, addr: u64, width: u8, v: u64),
    pub privileged: extern "C" fn(*mut TraceCtx, pc: u64, raw: u32) -> u64, // -> PrivOutcome
    pub check_code: extern "C" fn(*mut TraceCtx, addr: u64, expected: *const u8, len: usize) -> u8,
    pub take_trap: extern "C" fn(*mut TraceCtx) -> u8,
    pub take_irq:  extern "C" fn(*mut TraceCtx) -> u8,
    pub log:       extern "C" fn(*mut TraceCtx, ptr: *const u8, len: usize),
    pub host: *mut c_void,           // opaque: the real LocalStackHost/Reactor
}
```

Design notes / decisions to confirm:

- **Registers and the value stack stay in registers/native stack in JIT code.**
  The `regs` pointer is read/written directly by the compiled code (the register
  cache is the hot path today). The JIT keeps the value stack in host registers
  and a small static spill area, not a `Vec`.
- **`CheckCode`** is emitted by the JIT as an inline byte compare against the
  baked-in `expected` word, calling `check_code` (or just reading via
  `mem_load`) and jumping to a `Bail`/re-dispatch stub on mismatch. When
  `check_code_integrity` is off (running hart), the JIT omits it entirely.
- **`Privileged`** (CSR/FP/ecall-adjacent ops) stays a host call — the JIT does
  not try to lower the hart semantics; it calls `privileged` and branches on the
  returned `PrivOutcome` (continue vs divert-pc).
- **Trap atomicity** — on a faulting `mem_load`/`mem_store` (signalled via
  `take_trap`), the JIT jumps to a bail stub that returns `TraceExit::Bail(pc)`;
  the register-cache *undo* stays in the host (the compiled code calls a
  `note_inst_boundary` host fn per instruction so the undo log is correct).

### 2.3 `TraceExit` — a C-ABI exit

`Exit` has a `String` (in `Trap`), which is not C-ABI. The JIT path needs a flat
result:

```rust
#[repr(C)]
pub struct TraceExit {
    pub tag: u8,        // 0=TailCall, 1=Trap, 2=Ecall, 3=Bail
    pub pc: u64,        // TailCall/Bail target pc
    pub trap_code: u32, // Trap: a small code instead of a String (mapped back in the host)
}
```

The host converts `TraceExit` → `Exit` at the dispatch boundary (the only place
a `String` trap message is materialized, and only on the rare `Trap` path).

### 2.4 The new registry

```rust
pub struct CompiledTrace {
    pub f: CompiledTraceFn,
    pub ctx: *mut TraceCtx,          // owned by the producer (see lifetimes, §6)
    pub ops: Shared<Vec<StackOp>>,   // kept alive for CheckCode/interpreter/Debug
    pub kind: TraceBackend,          // Interp | Jit | Aot | Weval (for stats/debug)
}

// in the macro Core:
compiled: RefCell<BTreeMap<u64, CompiledTrace>>
```

Dispatch becomes:

```rust
let exit = match lock.compiled.borrow().get(&pc) {
    Some(ct) => unsafe { (ct.f)(ct.ctx, ct.ops.as_ptr(), ct.ops.len()) }.into_exit(),
    None => interpret_async_fueled(&ops, &mut host, fuel).await...,
};
```

## 3. The producers

All four register a `CompiledTrace { f, ctx, ops, kind }`. The dispatch loop is
producer-agnostic.

### 3.1 Interpreter (default, always available)

A generic `unsafe extern "C" fn interp_trace(ctx, ops, ops_len) -> TraceExit`
that drives the *existing* op semantics over the `ops` slice using the `TraceCtx`
host calls. This is the fallback and the reference: **the JIT's output is
differentially tested against it** (§7). `compile_trace` is reimplemented to
return this instead of a closure.

### 3.2 Native JIT (the new work)

A `vane-jit` crate (host-only, `std`, behind a `native-jit` cargo feature, off by
default so the wasm/js-host and stable-toolchain consumers are unaffected — see
the gen-blocks precedent for how we gate nightly/host-only code). Per host
target:

- `vane-jit/src/emit_aarch64.rs`
- `vane-jit/src/emit_x86_64.rs`
- `vane-jit/src/emit_riscv64.rs`

Each lowers `&[StackOp]` → executable machine code (§4), then registers it.

### 3.3 AOT

A build-time/offline step that compiles a fixed set of *known-hot* traces (e.g.
the OpenSBI/boot hot loop) to native code and links them in, registering them at
startup. Uses the same JIT codegen, just invoked ahead of time with the code
serialized into the binary (or a sidecar `.so`/section). This is how we get
"instant" hot paths for a known kernel without warmup.

### 3.4 Weval (existing)

The vane-patch Weval path stays as-is for the *whole-module* WASM case; we add a
thin adapter that, for a single trace, exposes its result as a `CompiledTrace`
(mostly a no-op rename so all four producers share the registry). This is
low-priority — the native JIT is the point.

## 4. Native JIT codegen (StackOp → machine code)

This is the core. We lower the stack machine to native code **without a value-stack
`Vec`**: the operand stack lives in host registers with a bounded static spill.

### 4.1 Stack-machine → register/stack mapping

The `StackOp` operand stack has a *statically known depth at every op* (the trace
is straight-line within a block; branches rejoin at known depths). So the JIT
computes, per op, the operand stack depth and maps:

- the top few slots → host registers (e.g. on aarch64: `x9..x15` scratch),
- deeper slots → a fixed spill area in the `TraceCtx` (or the native stack).

This eliminates essentially all `Vec` push/pop traffic — the dominant
interpreter cost after the host calls.

### 4.2 Per-op lowering (target-independent sketch)

| StackOp | native lowering |
|---|---|
| `PushImm(v)` | `mov reg, #v` / push to spill if deep |
| `LoadReg(r)` | `ldr reg, [regs + 8*r]` |
| `StoreReg(r)` | `str val, [regs + 8*r]` |
| `Drop` | (just adjust the depth bookkeeping) |
| `Add64/Sub64/And64/…` | one native ALU op on the top two slots |
| `LoadMem/StoreMem` | call `mem_load`/`mem_store` host fn (args in ABI regs) |
| `CheckCode` | inline compare of baked bytes vs `mem_load`; on mismatch → bail stub |
| `Privileged` | call `privileged` host fn; branch on `PrivOutcome` |
| `IfBegin/IfElse/IfEnd` | compare top-of-stack, native conditional branch |
| `LoopBegin/LoopEnd` | block label; `Br` → native jump back |
| `Br` | native branch to the loop's native label |
| `TailCall` | pop value → return `TraceExit::TailCall(v)` |
| `Ecall` | return `TraceExit::Ecall` |
| `Trap` | return `TraceExit::Trap(code)` |
| `Log` | call `log` host fn (or omit) |

### 4.3 Control flow

`LoopBegin`/`Br` use `relative_depth` (WASM-style). The JIT maps each `LoopBegin`
to a native label at the current code offset and each `Br` to a native jump to
`labels[depth - relative_depth]`. This is exactly the `Labels` chain the emitter
already maintains — reused directly. (`IfBegin/Else/End` become compare+branch.)

### 4.4 Codegen strategy decision (to confirm)

Two options:

- **(a) Hand-roll each backend** (emit bytes directly, like `render_wasm.rs`
  emits WASM). Max control, no new deps, consistent with the codebase, but ~3
  encoders to write and maintain.
- **Use Cranelift** (`cranelift-jit`): one IR, all three targets for free,
  register allocation included. Heavy dep, std-only, and we give up some control
  (and Cranelift's riscv64 support is younger than its aarch64/x86_64).

**Recommendation: hand-roll, starting with aarch64** (the dev host + a target we
already emit StackOps for), because the StackOp set is small and mostly
register/move/ALU/call — well within hand-rolling. Keep the codegen behind a
`Backend` trait so a Cranelift backend could be dropped in later without changing
the registry. **This is the main open decision for your review.**

## 5. The host interface (`extern "C"` shims)

The `TraceCtx` function pointers are implemented once as `extern "C"` shims over
the existing `LocalReactor`/`LocalStackHost` logic — so the JIT and the
interpreter share *one* machine-semantics implementation (no duplicated memory/
privileged code). E.g.:

```rust
extern "C" fn shim_mem_load(ctx: *mut TraceCtx, addr: u64, w: u8, s: u8) -> u64 {
    let host = unsafe { &mut *((*ctx).host as *mut LocalStackHost<Reactor>) };
    host.load_mem(addr, width(w), s != 0)   // the same code the interpreter uses
}
```

## 6. Lifetimes, memory, and invalidation

- **Executable memory**: the JIT writes into an `mmap`/`VirtualAlloc` region made
  W^X (write, then `mprotect`/`flush_icache` to exec). One growing region per
  Reactor, bump-allocated; freed traces are *not* individually reclaimed (they're
  small; the whole region is dropped with the Reactor).
- **Invalidation** ties into the trace-cache invalidation we just built: a
  trace's `CompiledTrace` is dropped (and its code logically dead) exactly when
  its `trace_cache` entry is invalidated (full clear / fine-grained sfence.vma /
  fence.i). The `ops` field keeps the `StackOp`s alive for `CheckCode` and for
  the interpreter fallback.
- **`TraceCtx` ownership**: each `CompiledTrace` owns a boxed `TraceCtx` pointing
  at the Reactor's register cache. The registry owns the box; dropping the
  registry entry frees it.
- **Thread safety**: today the registry is single-threaded under the core lock;
  the raw pointers are `Send`-able only if the JIT code is position-independent
  and the `TraceCtx` points at lock-protected state — we keep the registry under
  the existing core lock, matching the current `TraceFn` semantics.

## 7. Implementation phases

Each phase lands green (machine gate M1:TTTK + cross-fuzz + boots the R8 kernel),
benchmarked with the fixed-quantum / fixed-marker harness.

1. **Phase 1 — the ABI + registry conversion.** Add `TraceCtx`, `TraceExit`,
   `CompiledTraceFn`, `CompiledTrace`; reimplement `compile_trace` as the
   `extern "C"` interpreter (`interp_trace`); convert the registry and the
   dispatch loop. No native code yet — behavior identical, just a flat fn
   pointer instead of a closure. *Validates the ABI with zero codegen risk.*

2. **Phase 2 — aarch64 JIT.** `vane-jit` crate + `emit_aarch64.rs` covering the
   full `StackOp` set; W^X region; register it for hot traces (behind
   `native-jit`). Differential-test each compiled trace against `interp_trace`
   (extend the cross-fuzz harness: run both, compare register state + `Exit`).

3. **Phase 3 — x86_64 + riscv64 backends.** Same `Backend` trait, two more
   encoders. (riscv64 host is the "run vane on riscv64 hardware" case; x86_64 is
   the common dev/CI host.)

4. **Phase 4 — AOT.** Compile known-hot boot traces ahead of time; register at
   startup.

5. **Phase 5 — Weval adapter + cleanup.** Share the registry with the WASM path;
   remove the now-dead `TraceFn`-closure machinery.

## 8. Risks + validation

- **Correctness of codegen** — the big one. Mitigation: the interpreter producer
  is the always-present reference; every JIT trace is differentially tested
  (cross-fuzz style) before being trusted, and the `native-jit` feature is
  opt-in so the default build is unaffected.
- **W^X / icache** — per-OS exec-memory handling (macOS `mmap`+`mprotect`+
  `sys_icache_invalidate`, Linux, etc.). Contained in one module.
- **Trap atomicity** — the undo-log contract must hold in JIT code (a faulting
  memory op discards only the current instruction's writes). Covered by the
  differential tests + the machine gate.
- **js-host / wasm / stable-toolchain consumers** — untouched: `vane-jit` is a
  separate host-only crate behind an off-by-default feature, and the ABI change
  keeps the interpreter as the default producer so nothing else needs a JIT.
- **Performance validation** — same fixed-work harness as this round (fixed
  quantum count / fixed guest marker), plus a JIT-vs-interpreter per-trace
  microbenchmark.

## 9. Open questions for review

1. **Codegen strategy** (§4.4): hand-roll vs Cranelift. I recommend hand-roll +
   aarch64-first.
2. **`TraceCtx` shape** (§2.2): is the function-pointer-table approach right, or
   do you want a flatter `*mut LocalStackHost` + `extern "C"` free functions?
3. **Value-stack mapping** (§4.1): static register allocation vs a simple
   native-stack spill for v1 (simpler, slightly slower).
4. **AOT scope** (§3.3/Phase 4): which traces are "known-hot" enough to bake in?
5. **Does this replace the vane-patch WASM path** for the native use case, or
   coexist (WASM for the browser/js-host, native for the CLI)?

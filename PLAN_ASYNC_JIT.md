# PLAN_ASYNC_JIT — async hosts + WASM AOT + snapshot/resume

Status: **in progress**. Phase A (native coroutines) is done (`542b32b`). The
**vane-native WASM ABI** — the renderer `render_stack_to_wasm_vane` +
vane-patch's `compile_trace_vane` — is done (`ea1c5f2`, `4872149`); the speet
path (`WasmAbiConfig`/`render_stack_to_wasm`) is untouched. The **sync trampoline
seam is complete end-to-end** (`bed68e2`, `a9a5b21`, `eaa1a50`, `1a4897d`,
`d03f8e2`, `4eeefac`, and `bake_aot` — see Phase C): a baked WASM module
executes AOT'd traces through the funcref table, proven by the wasmi execution
test in `crates/vane-patch/tests/patch.rs`. Binaryen asyncify (the async layer)
and Phase D remain.

**Design decision (user, 2026-10-27): the WASM integration routes through the
fiber mechanism** — the WASM build uses the same unified async model as the
native JIT, with Binaryen **asyncify** as the WASM-level counterpart of the
native fiber (asyncify unwinds/rewinds the WASM stack on suspend/resume, exactly
as the native fiber switches stacks). A WASM trace suspends for an async host
the same way a native trace does.

## Goal

1. **The JIT interface fully supports async hosts.** A compiled trace must be
   able to *suspend* mid-execution when a host op needs to await (e.g. a remote
   memory fetch from a non-local host) and *resume* later. This is the
   prerequisite for non-local hosts. Two backends: native coroutines (the
   native JIT) and Binaryen asyncify (the WASM build).
2. **The WASM patcher AOT-compiles all traces for the next run.** A "Wizened"
   binary (a WASM binary with a snapshotted machine state, à la Wizer) exposes
   an interface for the patcher to bake every hot trace into the binary — so
   the next run starts with traces pre-compiled, no warmup.
3. **Validate with a larger userspace**: a guest that prints a message, then
   the emulator suspends + snapshots, then resumes with the traces recompiled.

## Phase A — async JIT interface (native coroutines)

The native JIT'd trace currently runs on the host stack and returns a final
`Exit`. For an async host, a host shim (memory load/store, privileged, state)
may need to suspend. Design:

- Run the compiled trace on a **separate stack** (a fiber). The trace's
  machine state (value stack, undo buffer, pc) already lives in `TraceCtx` in
  memory, so it survives a suspend; only the trace's *call frames* (the
  prologue's saved regs + the current shim frame) live on the fiber stack,
  which is preserved across the suspend.
- A hand-rolled aarch64 **context switch** (save/restore x19–x30 + SP) — the
  JIT's own style, no library.
- A **suspend/resume protocol** in `TraceCtx`: a suspending shim sets a
  `suspended` flag + the pending op, and `switch_back` returns control to the
  adapter, which returns a new `Exit::Suspended`. On resume, the adapter
  `switch_to` the fiber stack and the shim continues.
- Gate: the interpreter still drives; a mock async host (a memory op that
  suspends once, then returns) proves the JIT can suspend + resume and stay
  bit-exact with the interpreter.

## Phase B — WASM integration (through the fiber/asyncify mechanism)

For the WASM build, traces are AOT-compiled to WASM (vane-patch, the vane-native
ABI). The integration keeps the unified async model:

- The AOT'd traces live in a WASM **funcref table**. The vane interpreter's
  compiled registry holds a trace's table index; the dispatch loop calls it via
  a **thunk** (a Rust fn, compiled to WASM, with the `CompiledTraceFn` ABI) that
  `call_indirect`s the pack's function (the vane-native ABI) and converts the
  returned `(exit_tag, exit_pc)` into an `Exit`.
- The interpreter + the thunk + the traces are **asyncified** (Binaryen). A host
  shim that needs to await (non-local memory etc.) calls
  `asyncify_start_unwind`, unwinding the WASM stack through the thunk back to
  the dispatch loop; the loop drives the async op, then `asyncify_start_rewind`
  + re-calls to resume the trace — the WASM-level fiber switch, matching the
  native `Fiber`/`AsyncTraceRun` protocol (`step` / `resume_with`).
- Gate: a WASM trace suspends + resumes bit-exact with the interpreter, exactly
  as the native Phase A test does.

**Status (2026-10-28)**: the suspend/resume protocol is implemented and the
orchestration is in place; the full gate needs the external tool + an
asyncify-capable host, so it is NOT yet proven end-to-end. Done:
`vane_mem_load_async`/`vane_mem_store_async` shims (feature `asyncify`,
wasm32-gated) stash the op in the exported `VANE_ASYNC_OP` area and call
`env.asyncify_start_unwind` over the baked `VANE_ASYNCIFY_DATA` buffer —
the driver fulfills the op, writes `result`, rewinds, and re-invokes the
same export (the `AsyncTraceRun::step`/`resume_with` protocol). The build
needs `RUSTFLAGS="-C link-arg=--allow-undefined"` (undefined externs do not
link on this toolchain otherwise — verified); the async module then carries
the `env.asyncify_start_unwind` import + the `*_async` exports.
`vane_patch::bake_aot_async` binds pack-fn memory ops to the async shims and
returns the pending-op address; `vane_patch::asyncify_module` wraps the
external transform with a structure audit + custom-section re-attach (the
weval pattern). Remaining: run the real `wasm-opt --asyncify`, and a
suspend/resume differential test against an asyncify-capable host (Phase D).

## Phase C — WASM patcher AOT

Extend vane-patch: a Wizened binary (machine state snapshotted into the WASM
data section / a custom section) exposes a hook — the patcher enumerates the
hot traces recorded in the snapshot and AOT-compiles each into the binary's
dispatch table, so the next run starts with all traces pre-compiled.

**Done (`bake_aot`)**: the patcher builds each pack fn (vane-native ABI) and
trampoline (`CompiledTraceFn` ABI) DIRECTLY in the target module via walrus
`FunctionBuilder` (walrus's ID-remapping machinery is `pub(crate)`, so an
encode/parse round-trip cannot work), appends trampolines to the funcref table
(a WASM fn pointer IS its table index), bakes the `[count][pad][entries...]`
trace table, and patches the interpreter's exported `VANE_AOT_TRACE` static to
it (`Reactor::register_aot_traces` reads it on the next run). Design notes:
the pack fns call the interpreter's own exports directly (no imports), the
`check_code` shim is a generated multi-value wrapper over the sret
`vane_check_code_impl`, and the trampoline reads `vane_regs_offset()` at
runtime rather than baking it. Verified by `bake_aot_runs_traces_in_wasmi`
(an executed baked trace, bit-exact side effects + Exit) plus the
additivity audit. (Why a patched exported static rather than `extern` statics:
wasm-ld rejects undefined data symbols on wasm32-wasip1 — there are no data
imports to satisfy — so the patcher satisfies the contract by patching bytes.)

## Phase D — larger userspace test

A guest userspace program (larger than the initramfs init) that prints a
message; the emulator suspends at a marker + snapshots the machine state;
resume from the snapshot with the traces AOT-compiled; assert the resumed run
continues correctly (prints the rest / reaches the same state).

## Sequencing

A (foundation) → B → C → D. Each phase commits independently and keeps the
differential tests + machine gate + Linux boot green.

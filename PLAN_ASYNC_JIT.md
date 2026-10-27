# PLAN_ASYNC_JIT — async hosts + WASM AOT + snapshot/resume

Status: **planned**. Builds on `PLAN_NATIVE_JIT.md` (the native JIT, now 1.6×
the interpreter on the Linux boot after retain+revalidate).

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

## Phase B — WASM asyncify

For the WASM build, traces are compiled to WASM (vane-patch). Apply Binaryen
`asyncify` to the compiled trace functions so a WASM trace can unwind on a
suspend and rewind on resume. The `TraceCtx` equivalent lives in linear
memory; asyncify's unwind/rewind handles the WASM operand stack + call frames.

## Phase C — WASM patcher AOT

Extend vane-patch: a Wizened binary (machine state snapshotted into the WASM
data section / a custom section) exposes a hook — the patcher enumerates the
hot traces recorded in the snapshot and AOT-compiles each into the binary's
dispatch table, so the next run starts with all traces pre-compiled.

## Phase D — larger userspace test

A guest userspace program (larger than the initramfs init) that prints a
message; the emulator suspends at a marker + snapshots the machine state;
resume from the snapshot with the traces AOT-compiled; assert the resumed run
continues correctly (prints the rest / reaches the same state).

## Sequencing

A (foundation) → B → C → D. Each phase commits independently and keeps the
differential tests + machine gate + Linux boot green.

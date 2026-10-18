# Coroutines & host suspension (non-local hosts)

A **non-local host** is one whose register/memory access isn't locally
resident — e.g. guest RAM living in a remote browser (wsdom) or any host that
must round-trip. Such a host can't answer a memory read synchronously; the
guest must *suspend* until the round-trip resolves. This is the foundation
for non-local hosts (PLAN_LINUX_WASM.md §4.2).

## The interpreter is already a coroutine

`vane_arch::interp::interpret_async` is `async` and `.await`s every host
operation via `AsyncStackHost`. For a **local** host (`LocalStackHost`) these
futures are always immediately-ready, so the guest runs without yielding
(`block_on_ready`). For a **non-local** host, a host op returns `Pending`
until its round-trip resolves — the guest future parks.

### `GuestCoroutine` (the host-agnostic seam)

`vane_arch::interp::GuestCoroutine` wraps a guest-execution future and drives
it step-by-step:

- `resume()` runs the guest until it completes (`CoroutineState::Done`) or
  parks on a host op (`CoroutineState::Parked`).
- `is_woken()` reports whether the host has signalled round-trip completion
  since the last resume.

A local host never parks; a non-local host parks at each round-trip and the
driver resumes it when the round-trip resolves. Tested in
`crates/vane-arch/tests/coroutine.rs`.

## Wasm-level suspension: JSPI vs Binaryen asyncify

When the guest is compiled to `wasm32` and the host is JS, the suspension has
to cross the wasm/JS boundary. Two mechanisms:

| Mechanism | Overhead | Availability | Notes |
|---|---|---|---|
| **JSPI** (`WebAssembly.Suspending` / `promising`) | none | JSPI-capable engines | The js-host build already uses this (`suspend` helper in the generated JS). Zero-overhead; the wasm stack is genuinely suspended. |
| **Binaryen asyncify** (`wasm-opt --asyncify`) | shadow-stack copy on each suspend/resume | portable | Instruments the wasm so an async import unwinds it and a later rewind resumes. |

### Asyncify constraint (important)

`wasm-opt --asyncify` **does not support reference types** (`externref`) at
the suspend boundary (Binaryen issue #3739). The wasm-bindgen-generated
surface uses `externref` heavily, so asyncify **cannot** be applied to a
wasm-bindgen module directly. Asyncify is therefore only usable for a
**numeric-only** host boundary — a hand-written import set where the async
host operations take and return plain numbers (addresses, lengths, handles).

Practical guidance:

- The default browser path is **JSPI** (already wired in the js-host build).
- Use **asyncify** only with a numeric-only host boundary:
  `harness/build-asyncify.sh <crate> 'env.<glob>'` builds the crate and
  applies the transform; `demo/browser/asyncify.js` is the JS driver
  (`Asyncify::hostCall` / `Asyncify::run`).
- The **guest code is identical** under JSPI, asyncify, or a native executor
  — only the driver differs. That sameness is the point: write the host once
  against `AsyncStackHost`, pick the suspension mechanism per target.

## What this enables

With `GuestCoroutine` + a suspension mechanism, a non-local host can be added
by implementing `AsyncStackHost` (or the `os-async` surfaces via
`AsyncOsHost`) with operations that return `Pending` until their round-trip
resolves. The interpreter, trace engine, and compiled-trace registry need no
changes — the suspension is entirely at the host boundary.

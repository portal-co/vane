# Testing Guide

## Host configurations (M0, PLAN_LINUX_WASM.md)

vane now builds in two host configurations:

- **`js-host` (default)** — the historical wasm-bindgen browser/Node surface.
- **plain-Rust twin (`--no-default-features`)** — no wasm-bindgen, js-sys, or
  JsValue anywhere; this is the build for `wasm32-wasip1`, WASIX, and native
  hosts. The `vane_meta!` macro emits both surfaces from one definition
  (decision D1).

`vane-cli` (`crates/vane-cli`) is the self-driving host binary: it loads a
statically linked RV64 ELF and runs it under `Reactor::interp_core` (the
host-independent interpreter loop), bridging a minimal Linux syscall set
(`write`, `read` EOF, `exit`/`exit_group`) to stdio. It builds natively and
for `wasm32-wasip1` (`--no-default-features`).

### M4 gates: machine model and Linux boot

```sh
harness/run-machine-gate.sh       # devices: UART/ACLINT/PLIC/syscon (fast)
harness/run-linux-boot-gate.sh    # full OpenSBI+Linux+initramfs boot (slow)
```

### M6 gate: browser (offline core)

```sh
harness/run-web-smoke-gate.sh     # vane-web wasm + OPFS-shape swap bridge in Node (fast)
```

Builds the `vane-web` wasm package (`wasm-pack --target nodejs`) and runs the
swap-stress guest through it with a JS-provided swap store (the same shape
the OPFS bridge uses in a browser worker). The full browser demo lives in
`demo/browser/` (see its README); the Playwright E2E needs npm + a browser
download (network) and is a nightly job.

### M7: WASIX threaded host IO

The `wasix` cargo feature on `vane-cli` runs the UART stdin pump on a
background thread (a `std::sync::mpsc` channel feeds the interpreter loop)
instead of cooperative polling between quanta. Time is virtual
(instruction-counted), so the thread only moves console input off the
interpreter's critical path. Validated natively by `harness/payload-echo`
(a guest polls the UART and echoes a byte delivered via the stdin-pump
thread): `printf V | vane-cli --features wasix --elf payload-echo --machine`
→ `ECHO:V:OK`.

**WASIX build/run (network-gated)**: needs `cargo install cargo-wasix`,
`cargo wasix download-toolchain`, and the `wasmer` runtime. Then:
`cargo wasix build -p vane-cli --no-default-features --features wasix` and
boot under wasmer. The plain `wasm32-wasip1` build already runs under wasmer
(WASIX is a wasip1 superset); the `wasix` feature additionally uses a real
thread for console IO (S6).

### M5 gate: encrypted swap

```sh
harness/run-swap-gate.sh          # encrypted RAM paging (fast)
```

`run-swap-gate.sh` runs `harness/payload-swap-stress` (a bare-metal guest that
writes/verifies a per-page pattern across a 6 MiB region) under a 512 KiB
resident cap with an encrypted backing store (`--swap`/`--resident`), on
native and WASI, asserting the `S4:<checksum>:OK` marker. S5 (tamper
detection) is covered by `cargo test -p vane-swap --features std`. See
[PAGING_SWAP.md](PAGING_SWAP.md).

`run-machine-gate.sh` runs `harness/payload-machine` (M-mode device
exerciser) under `vane-cli --machine` natively and under WASI; expect
`M1:TTTK`.

`run-linux-boot-gate.sh` boots OpenSBI + the Debian riscv64 kernel + a
busybox initramfs (see `harness/linux/README.md` for fetching the
artifacts) and asserts the OpenSBI banner, userspace init, correct
`uname -a` and `sha256sum` output, and a clean SBI SRST poweroff. The
interpreter runs ~1M instr/s, so the boot takes tens of minutes — this is a
manual/nightly gate, not per-commit CI.

The device model is QEMU `virt`-shaped: ns16550a UART @ 0x1000_0000 (PLIC
source 10), CLINT-compatible timer @ 0x200_0000, PLIC @ 0xC00_0000, SiFive
test finisher @ 0x10_0000. The DTB is generated at runtime by
`vane_riscv::machine::virt_dtb`.

### M3 gate: privileged architecture

```sh
harness/run-priv-gate.sh
```

Builds and runs `harness/payload-priv` — a bare-metal payload that boots in
M-mode, builds Sv39 page tables, drops to S-mode, exercises ecall delegation
(M then S), translated loads/stores, and a delegated store page fault —
under `vane-cli --hart` both natively and inside WASI (node:wasi). Exit code
0 = pass (riscv-tests `tohost` convention).

Unit coverage lives in `crates/vane/tests/privileged.rs` (trap delegation,
CSR privilege rules, xRET, interrupts, Sv39 walk/faults) and
`crates/vane/tests/regressions.rs`. Both run in either host configuration:

```sh
cargo test -p vane --test privileged --test regressions
cargo test -p vane --no-default-features --test privileged --test regressions
```

### M1 gate: no-JIT execution + engine parity

```sh
harness/run-nojit-gate.sh
```

This builds the wasm-pack nodejs package and runs the payload in Node twice:
once with `new Function`, `eval`, and `WebAssembly.compile*` disabled
(mirroring Cloudflare Workers / strict CSP), and once with codegen allowed.
In both environments the `interp` engine (the `StackOp` trace interpreter —
`Reactor::trace_run`, exposed to JS as `reactor.run()` with
`set_engine("interp")`) must produce the expected output, the `jit` engine
must either run (codegen allowed) or degrade via the `get$` fallback (codegen
banned), and the observable behavior of both engines must be identical (D5
engine parity).

The Rust-side engine-parity test (same program through `interp_core` and
`trace_run`, full register + memory comparison) runs with:

```sh
cargo test -p vane --no-default-features --test engine_parity
```

### M0 gate: payload-in-WASI

```sh
harness/run-wasip1-hello.sh
```

This builds the `harness/payload-hello` RV64 bare-metal payload, builds
`vane-cli` for `wasm32-wasip1` (asserting the dependency tree has no JS
bindings), and runs the payload inside the WASI module under Node's
`node:wasi` (`harness/run-wasi.mjs`). Expected output:
`hello from vane wasip1` and `M0 wasip1 gate: PASS`.

Native equivalent (payload mode now requires the privileged architecture —
the user-mode syscall-shim micro-OS was removed; `--hart` runs the payload
with the trap engine live, completing via `--tohost` or trap):

```sh
cargo run -p vane-cli -- \
  --elf harness/payload-hello/target/riscv64gc-unknown-none-elf/release/payload-hello \
  --hart
```

## Current approach: WASM-in-JS

vane's emulator core (`crates/vane`) is compiled to `wasm32-unknown-unknown` and its
public API is generated by `wasm-bindgen`.  The entry points (`interp`, `jit_run`)
return `JsValue`-based futures that only exist inside a WASM host.  **A native Rust
binary cannot call them directly** — there is no `JsValue` outside a browser or Node.js
WASM runtime, and `futures::executor::block_on` cannot drive them.

The correct way to run the emulator from the command line today is via the Node.js
tester in `tools/`, which loads the `wasm-pack`-built package and drives it inside
Node.js exactly as a browser would.  See [`tools/README.md`](tools/README.md) for
build instructions and usage.

## Writing tests

- **Rust unit tests**: `cargo test` — for logic that lives in pure-Rust crates
  (`vane-arch`, `vane-riscv`, `vane-target-js`, `vane-target-wasm`).
- **WASM integration tests**: `wasm-pack test --node -- crates/vane` — for tests that
  exercise `wasm-bindgen`-annotated code.
- **End-to-end ELF tests**: build a RISC-V ELF with the harness (see `harness/`), then
  run it through `node tools/dist/vane-tester-wasm.js --input <elf>`.

## Harness

The `harness/` directory is the place to add tooling used by tests: scripts to build
RISC-V test binaries, Docker images for cross-compilation, etc.

Suggested layout:
- `harness/build.sh` — cross-compile test binaries
- `harness/run.sh` — run all ELF tests through the WASM tester
- `harness/docker/` — container images for reproducible cross-compilation

Current contents:
- `harness/payload-hello/` — detached-cargo-workspace RV64GC bare-metal
  payload (build from inside its directory so its `.cargo/config.toml`
  applies; cargo config discovery is cwd-based)
- `harness/run-wasi.mjs` — minimal WASI preview1 runner for `vane-cli.wasm`
  (Node `node:wasi`; preopens the cwd so `--elf` paths resolve)
- `harness/run-wasip1-hello.sh` — the M0 end-to-end gate

## Future plans

### 1 · Automated RISC-V ELF testing via WASM-in-JS

Wire the `harness/` build pipeline into `npm test` so that ELF test binaries are built
and executed through `tools/dist/vane-tester-wasm.js` automatically in CI.  Pass/fail
would be determined by the `__success` sentinel already emitted by the `ecall` handler,
or by exit-code conventions from the ELF under test.

### 2 · Native testing via a WASM-interpreter host

A future Rust binary (likely a separate crate, e.g. `vane-test-runner`) would embed a
lightweight WASM interpreter such as `wasmtime` or `wasmi`, load the
`wasm32-unknown-unknown` output of `crates/vane` at test time, and invoke `interp` /
`jit_run` through the interpreter's host API.  Results would be reported as ordinary
`cargo test` outcomes.

This removes the JS engine dependency from automated testing, keeps the emulator
running as genuine WASM (no environment mismatch), and integrates cleanly with `cargo
test` and Rust-native CI tooling.

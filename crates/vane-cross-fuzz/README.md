# Cross-Fuzz — Differential Testing Against Unicorn

This directory hosts **`vane-cross-fuzz`**, the differential test harness
executing Vane's architecture frontends (RV64, AArch64) against
[Unicorn 2.x](https://www.unicorn-engine.org/) on random instruction streams.

**Status:** Implemented milestones M0–M3 of [`CROSS_FUZZ_PLAN.md`](../CROSS_FUZZ_PLAN.md).

## Quick start

```sh
cd crates/vane-cross-fuzz          # standalone crate — NOT a workspace member

# RV64 differential tests (smoke + proptest + encoder-driven)
cargo test --offline --features rv64

# AArch64 differential tests
cargo test --offline --features aarch64

# Everything
cargo test --offline --features rv64,aarch64
```

First run compiles Unicorn 2.1.5 from vendored C sources via cmake
(requires `cmake`; ~2–5 min, cached afterwards).

## What it does

For each random instruction word, both engines execute **exactly one
instruction** from identical initial state (registers, code bytes, scratch
data page), then the harness diffs the full architectural state:

- **Vane side:** the frontend's `StackOp` trace (`rv_emit`/`a64_emit`) runs
  through the native interpreter (`vane_arch::interp`) against a `Mem`-backed
  host. An `ebreak`/`brk` **stop-pad** placed after the tested word bounds the
  trace to a single instruction.
- **Unicorn side:** `emu_start(pc, end, count=1)` — QEMU's own single step.
- **Compare:** GPRs, PC reachability, NZCV (AArch64), and scratch-memory bytes.

Traps are classified per [`src/skip.rs`](src/skip.rs): invalid encodings,
unsupported instructions, guest breaks, and syscalls **skip**; any *unknown*
trap message **fails closed**; genuine state mismatches fail with a
self-contained report.

## Test targets

| File | What |
|---|---|
| `tests/rv64_smoke.rs` | Hand-picked words: match, skip, memory, and a planted-divergence gate proving the harness detects real bugs |
| `tests/rv64_prop.rs` | Uniform random RV64 words (proptest) + skip-rate sweep |
| `tests/rv64_encoded.rs` | Encoder-driven RV64 words (all implemented families, boundary immediates) |
| `tests/aarch64_smoke.rs` | AArch64 hand-picked words incl. NZCV and STP memory |
| `tests/aarch64_prop.rs` | Uniform random AArch64 words + sweep |

Proptest regression seeds are committed (`*.proptest-regressions`), so any
divergence ever found replays on every run.

## Bugs found so far

1. **`c.andi` immediate sign extension** (in `rv-asm`, fixed upstream):
   `c.andi s0, s0, -22` decoded as `+42`. Found within seconds of the first
   random-word run.
2. **`rem`-by-zero returned all-ones instead of the dividend** (in Vane,
   fixed in M2): the JS renderer's quirk had been deliberately mirrored into
   the interpreter and WASM renderer for "cross-backend parity", propagating
   an ISA violation across every backend. The RISC-V M extension table says
   remainder-by-zero is the *dividend*.
3. **`a64_emit` panics** on some malformed encodings (`unreachable!` in
   ADDSUB_SHIFT with `shift_type=3`) — counted by the AArch64 sweep
   (`frontend_panics`), should trap rather than panic; fix pending.

# Cross-Testing Vane Against Unicorn: Fuzzing Plan

**Status:** Draft
**Goal:** Gain high confidence that Vane's architecture frontends (RV64 and
AArch64) are behaviorally correct by differentially testing them against
[Unicorn](https://www.unicorn-engine.org/) with random instruction streams,
using `proptest` (CI-friendly, deterministic) and `libfuzzer` (coverage-guided,
nightly).

---

## 1. Why

Vane's frontends (`vane-riscv` via `rv_emit`, `vane-aarch64` via `a64_emit`)
translate architecture instructions into architecture-agnostic `StackOp`
traces, which are then executed by three different backends (the JS renderer,
the WASM renderer, and the native `interpret` in `vane-arch::interp`).
Correctness of every backend ultimately rests on the **frontend's semantics**.

Today's tests are a hand-picked corpus (`rv-corpus` ELF tests, per-family unit
tests like `riscv_fp_tests`). Random binaries fuzzing Vane against a trusted
reference implementation (Unicorn 2.x) closes the gap: any divergence in
register/memory/PC state after executing the same random instructions is a
frontend bug — or an intentional divergence that must be explicitly gated (see
§6).

## 2. Scope

| Frontend | Decoder | Emit entry | Native executor | Unicorn mode |
|---|---|---|---|---|
| RV64 | `rv-asm` (`Inst::decode(code, Xlen::Rv64)`) | `rv_emit` (`vane-riscv/src/template/riscv.rs`) | `interpret` + `StackHost` over `vane_arch::Mem` | `UC_MODE_RISCV` (RV64GC, machine `RISCV`) |
| AArch64 | `disarm64` (`decoder_full`) | `a64_emit` (`vane-aarch64/src/template/aarch64.rs`) | `interpret` + `StackHost` over `vane_arch::Mem` | `UC_MODE_ARM` (AArch64, machine `ARM64`) |

Out of scope for phase 1:

- The JS codegen path (`Reactor::j`) and WASM JIT — the native `StackOp`
  interpreter exercises the same trace the renderers consume, so frontend
  correctness transfers. A follow-up phase can mirror the same fuzz inputs
  through `wasm-pack test` (Node) to cover the renderers.
- Privileged/system modes, atomics (unimplemented in both Vane and this
  comparison), vector/SIMD.
- Floating-point with non-default rounding modes (Vane traps these today —
  they become skips per §5).

## 3. New crates / layout

```
crates/vane-cross-fuzz/            # shared harness (dev-only helper crate)
  src/
    lib.rs                         # VaneCrossHost: StackHost impl over Mem
    state.rs                       # arch register image <-> Unicorn sync
    compare.rs                     # state diffing + minimality reporting
    skip.rs                        # skip/trap classification (§5)
    input.rs                       # random binary generation strategies (§7)
  Cargo.toml                       # dev/deps: unicorn-engine, proptest

fuzz/                              # cargo-fuzz workspace (libfuzzer, nightly)
  fuzz_targets/
    fuzz_rv64.rs
    fuzz_aarch64.rs
  Cargo.toml
```

Key dependency decisions:

- `unicorn-engine = "2"` (Unicorn 2.x Rust bindings). It links the native C
  library, so **it lives only in dev-dependencies / the `fuzz/` workspace** —
  never in the main workspace's dependency graph, keeping
  `wasm32-unknown-unknown` builds and `wasm-pack` untouched.
- `proptest` as a dev-dependency of `vane-cross-fuzz` and of the two frontend
  crates that host the `#[cfg(test)]`-adjacent integration tests.
- Everything else (`Mem`, `StackOp`, `interpret`, `StackHost`, `Exit`) already
  exists and is usable natively — no WASM/JS host required (confirmed by the
  existing `riscv_fp_tests` pattern).

### Visibility prerequisites

`rv_emit` and `a64_emit` are currently private to their crates. The fuzz tests
need a public (or `#[doc(hidden)]`, test-oriented) entry point per frontend:

- `vane-riscv`: expose a thin wrapper, e.g.
  `pub fn rv_trace(pc: u64, params: &Params) -> Vec<StackOp>` (or re-export
  `rv_emit` behind a `fuzz` feature). It only needs `TemplateJit{pc, labels,
  depth, params}` with empty labels/depth 0, as `rv_emit` does today.
- `vane-aarch64`: same for `a64_emit`.

These wrappers must be gated so they never leak into the `wasm32` cdylib
(e.g. behind a `fuzz-traces` feature that is default-off, and only declared in
the fuzz/dev context).

## 4. Test architecture: single-instruction differential stepping

The most robust shape is **step-differential**, not whole-program: execute one
random instruction at a time, compare state after each step, then advance.

Per step:

1. **Setup (shared):** identical initial state on both engines —
   - Fresh random GPR values (full 64-bit patterns, including nasty cases:
     all-ones, `1<<63`, values that alias PC-relative math).
   - Identical memory image in the mapped code page and one scratch data page
     (`Mem` pages on the Vane side; `mem_write` on the Unicorn side).
   - Same PC, same zero registers (`x0` hardwired to 0 in both).
2. **Vane:** build the `StackOp` trace for the single instruction, run
   `interpret(&trace, &mut host)`, stop at the first `Exit`.
3. **Unicorn:** `emu.emu_start(pc, pc + len, count = 1, timeout)` — exactly one
   instruction.
4. **Compare:** GPRs (including Vane's `StateSlot` flags/NZCV for AArch64),
   PC (`Exit::TailCall(next_pc)` on Vane side vs Unicorn PC), and bytes of the
   data page (for stores). Ignore registers Unicorn does not expose and
   slots both agree are unpredictable.

Short traces (2–8 random instructions) are added in phase 2 to catch
cross-instruction state corruption (flag reuse, scratch-slot aliasing), still
compared step by step so the *first* diverging instruction is pinpointed.

> **Why the native `interpret` path instead of the JS JIT:** the JS renderer is
> a translation of the same `StackOp` trace; if the trace's semantics are
> proven equal to Unicorn's, remaining risk is confined to the renderers,
> which are far more cheaply exercised by the existing browser corpus. A later
> phase can replay the exact same `StackOp` traces through
> `render_stack_to_wasm` + `wasmi` (the pattern already used in
> `vane-arch/tests/render_stack_to_wasm.rs`) to get renderer coverage *in the
> same fuzz loop*, still without Unicorn involvement.

## 5. Skip semantics (the heart of this plan)

Random binaries hit instructions Vane does not support, and instructions that
trap *by design*. Every such case must **skip silently** (not fail, not
noise), while a true semantic mismatch must fail loudly with a reproducible
seed. The skip rules, in evaluation order:

| # | Condition (Vane side) | Condition (Unicorn side) | Action |
|---|---|---|---|
| 1 | `Exit::Trap(msg)` where `msg` starts with `decoding:` (invalid/undecodable RV encoding) | *any* Unicorn error (`UC_ERR_FETCH_UNMAPPED`, `UC_ERR_INSN_INVALID`, …) | **Skip.** Unimplemented or invalid encodings are outside Vane's contract. |
| 2 | `Exit::Trap(msg)` where `msg` starts with `op:` (RV instruction recognized by decoder but unimplemented in `rv_body`, incl. `ebreak`) or `aarch64: unsupported {family} {word}` | `UC_ERR_INSN_UNSUPPORTED` or invalid-instruction error | **Skip.** Explicitly unsupported instruction. |
| 3 | `Exit::Trap(msg)` where `msg` starts with `aarch64: brk #` | Unicorn hits `BRK` (guest exception) | **Skip** — guest trap, both engines agree it's not a normal exit. |
| 4 | Vane traps on rounding-mode FP variants (current documented limitation) | Unicorn executes it | **Skip**, tracked on a denylist of SFN/rounding-mode encodings until implemented. |
| 5 | `Exit::Ecall` (RV `ecall`, AArch64 `svc`) | Unicorn stops on the syscall hook | **Skip** (syscall semantics are a host concern, not architectural; a later phase can fuzz the syscall layer against a Linux harness). |
| 6 | Any trap message **not** matching the prefixes above (new trap categories added later must be registered here) | — | **Fail** by default. Fail-closed: unknown trap kinds surface instead of being silently skipped. |
| 7 | Vane completes the step normally | Unicorn returns an error | **Fail** (Vane executed something the reference refused — assume Vane is wrong, investigate). |
| 8 | Vane completes normally | Unicorn completes normally | **Compare state.** Mismatch ⇒ **Fail** with a minimized repro. |

Practical notes:

- Classification lives in one place (`vane-cross-fuzz/src/skip.rs`) keyed on
  the trap-message prefixes listed above, with a unit test per prefix so
  message-format changes in `rv_body`/`a64_emit` break the fuzz tests visibly
  rather than silently over-skipping.
- Unicorn-side mapping faults from *data* addresses are a genuine comparison
  opportunity: pre-map both sides' data pages identically so a load/store that
  faults in only one engine is a real mismatch (not a skip).
- Track a skip-rate counter per fuzz target. A sudden jump in skip rate after
  a frontend change is a regression signal (coverage being pushed into "skip"
  territory), surfaced by proptest statistics and libfuzzer's feature counters.

## 6. Known intentional divergences (documented, not skipped silently)

These are cases where Vane and real hardware are *known* to differ; each needs
an explicit, commented gate in the comparator rather than a blanket skip:

- **Shift amounts:** the native interpreter masks shifts to the low 6 bits
  (matching real RV64), while the JS renderer's BigInt semantics differ for
  unmasked counts (see the comment block in `vane-arch/src/interp.rs`). Since
  the fuzz target runs the *native* path, no gate is needed; the divergence is
  noted so nobody "fixes" the fuzz harness to match the JS quirk.
- **JS-renderer `signed(a & max32)` sign-extension quirks** (`DivS32`/
  `RemS32`/`ShrS32`): the native interpreter intentionally does *not* mirror
  these (they are flagged there as probable JS-path bugs). If this plan finds
  they are indeed bugs, fixing the renderer is the follow-up; the fuzz harness
  compares against hardware semantics (i.e. the interpreter's), not the
  renderer's.
- **Divide-by-zero sentinels** and `i64::MIN / -1` wraparound: both Vane and
  RISC-V specify all-ones/all-operand values; Unicorn should agree. If it
  doesn't for a specific encoding, gate and verify against the ISA manual.
- **HINT instructions:** with `test_mode` off they must be no-ops; assert
  state equality (a cheap invariant check), no special skip.

## 7. Random binary generation strategies

Layered, from dumb to smart, all feeding the same differential engine:

1. **Uniform words** (`u32::any()` / proptest's `any::<[u8;4]>()`): mostly
   skips, but establishes baseline coverage and exercises the skip classifier
   itself. Cheap.
2. **Encoder-driven generation:** build words from the actual field layouts
   (opcode/funct3/funct7 for RV; the 11-bit top pattern for AArch64), with
   random register fields and random immediates. This is where almost all
   *comparable* instructions come from. Reuse the field tables the frontends
   already match on; keep the generator in `vane-cross-fuzz` so RV and AArch64
   targets share the scaffolding.
3. **Bias toward implemented sets:** weight the generator toward the families
   the README documents as implemented (RV64IM + M + scalar F/D subset;
   AArch64 integer subset + scalar FP subset). Weighting is tunable per fuzz
   run so the corpus follows implementation progress.
4. **Adversarial constants:** bias immediates/registers toward the usual
   boundary set (`0`, `!0`, `1<<31`, `1<<63`, min/max signed immediates,
   NaN payloads, ±0.0, subnormals).
5. **Short directed sequences** (phase 2):`move`-then-`op`-then-`compare`
   prefixes so flags carry into the compared instruction (AArch64 S-suffix
   coverage), and a store-then-load pairing to cross-check memory widths.

## 8. The two fuzzing backends

### 8.1 `proptest` — deterministic, CI (stable toolchain)

- Files: `crates/vane-riscv/tests/cross_unicorn.rs`,
  `crates/vane-aarch64/tests/cross_unicorn.rs`, each driving the shared
  harness.
- Strategy: `proptest!` with a bounded number of cases per invocation
  (`PROPTEST_CASES`, e.g. 256 in CI), generation strategy per §7, and
  `max_shrink_iters` tuned so shrinking produces a *minimal* failing
  instruction sequence.
- Persist failures: `proptest` regression files live in
  `crates/<crate>/proptest-regressions/` — committed, so any found divergence
  becomes a permanent regression test even after the bug is fixed.
- Skip accounting: use proptest's statistics (or a simple atomic counter
  printed at the end) so CI logs show cases run / skipped / compared.
- Runtime guard: cap each case's total instructions; no async; everything
  native; Unicorn setup/teardown per case (open a fresh `Unicorn` per case —
  cheap relative to the C engine's per-instruction cost).

### 8.2 `libfuzzer` — coverage-guided, nightly (`cargo-fuzz`)

- Files: `fuzz/fuzz_targets/fuzz_rv64.rs`, `fuzz/fuzz_targets/fuzz_aarch64.rs`
  with a `lib.rs` that calls into `vane-cross-fuzz`'s runner.
- Input = raw bytes → interpreted as instruction words (RV: 4-byte words,
  honoring 16-bit compressed encodings via the `IsCompressed` path; AArch64:
  4-byte words) plus a header byte selecting the register-seed strategy.
- Coverage feedback comes from the frontends' own match arms (`rv_body`'s
  per-instruction arms, `a64_emit`'s `Operation` families) — exactly the code
  whose correctness we want to saturate. `-use_value_profile=1` helps squeeze
  immediate-range coverage.
- Artifacts: `crash-*` files from a mismatch are automatically minimized by
  libFuzzer (`-minimize_crash=1`); commit them under
  `fuzz/corpus/` after triage so they become proptest regression cases too.
- CI: nightly job only (`cargo +nightly fuzz run fuzz_rv64 -- -max_total_time=300`)
  with `UC_MODE_RISCV`/`ARM64` unicorn builds cached; proptest remains the
  PR-gate.

## 9. Failure reporting requirements

When comparison fails, the report must contain — without re-running anything:

- the exact instruction word(s), PC, and initial register/memory image (as
  hex, plus the proptest seed / libfuzzer artifact path);
- Vane's `Exit` result and (if applicable) full `StackOp` trace for the
  instruction (`format!("{:#?}", ops)` — traces are small, and this is the
  single most useful artifact for a fix);
- Unicorn's resulting state and error code;
- the comparator's field-by-field diff (only differing registers/slots).

This is deliberately engineered so a divergence report is pastable into an
issue and directly replayable with a unit test (each failure class gets a
`#[test]` template in `vane-cross-fuzz/src/compare.rs`).

## 10. Milestones

1. **M0 — Visibility + harness skeleton.** `fuzz-traces` features on
   `vane-riscv`/`vane-aarch64`; `vane-cross-fuzz` crate with the `StackHost`
   over `Mem`, Unicorn state sync, and the skip classifier (with prefix unit
   tests). No fuzzing yet; `cargo test -p vane-cross-fuzz` green on host.
2. **M1 — RV64 single-instruction proptest.** Uniform words + skip
   classification (§5 rows 1–8). Expect mostly skips at first; the classifier
   counters prove the pipeline works end to end. Gate: a hand-planted
   semantics mutation in `rv_body` (e.g. flip `ADD`/`SUB` funct7 handling) is
   caught, and reverting makes it pass — proving the harness actually
   compares, not just skips.
3. **M2 — RV64 encoder-driven generation.** Coverage of the implemented RV64
   integer/M/F/D families; proptest regression file gains its first real
   entries (or, better, zero entries — either is a meaningful result to
   record).
4. **M3 — AArch64 parity.** Same harness, `a64_emit` traces, ARM64 Unicorn,
   `StateSlot` comparison including NZCV. The known Vane gaps (rounding-mode
   FP, scalar FP memory) land as row-4/row-2 skips.
5. **M4 — libfuzzer targets + CI.** `cargo-fuzz` workspace, nightly job with
   `-max_total_time` bound, corpus artifacts committed post-triage.
6. **M5 — Multi-instruction traces.** 2–8 instruction sequences, step-by-step
   comparison, sequence-level shrinking. Includes the store/load width pairs
   and flag-carry prefixes (§7.5).
7. **M6 — (optional) renderer replay.** Feed identical `StackOp` traces
   through `render_stack_to_wasm` + `wasmi` inside the same harness to extend
   differential coverage to the WASM rendering path.

## 11. Risks and mitigations

| Risk | Mitigation |
|---|---|
| `unicorn-engine` crate drags a C toolchain into normal builds | Dev-deps only; `fuzz/` is a separate workspace; `vane-cross-fuzz` is excluded from the root workspace members and built only for tests. |
| Unicorn's RV64 model differs subtly from the ISA spec (e.g. CSR side effects) | Restrict phase 1 to a CSR-free subset (`csrr*` generation off) and compare only GPR/PC/memory; add CSRs only with explicit whitelist comparison. |
| Over-skipping hides real bugs (§5 classifiers too broad) | Fail-closed row 6; unit tests per prefix; skip-rate telemetry; the M1 mutation gate proves false negatives aren't structural. |
| FP results diverge on NaN propagation / `minNum`-style ops where the spec leaves latitude | Compare raw bit patterns first; on mismatch, consult the ISA manual and gate per-instruction with a comment citing it, rather than silently skipping whole families. |
| Fuzz slowness (Unicorn per-case setup) | Reuse one `Unicorn` instance with `mem_reset`/register re-init where safe; keep default CI case counts modest; rely on libfuzzer for volume. |
| AArch64 `disarm64` decode and Unicorn disagree on what an encoding *is* | When Vane can't decode (row 1/2), skip regardless of Unicorn's verdict — decode disagreements are upstream `disarm64` issues, logged separately, not fuzz failures. |

## 12. What "done" looks like

- `cargo test -p vane-cross-fuzz -p vane-riscv -p vane-aarch64` runs the
  proptest differential suite on every PR, with committed regression files
  and no known-silent skips beyond the documented §5 table.
- A nightly libfuzzer run accumulates corpus entries covering every
  implemented frontend family at least once (verified by the skip/compare
  counters), and its failures reproduce as one-line unit tests.
- Every intentional divergence from §6 has either been fixed, or has a code
  comment + denylist entry that says exactly why it differs — no undocumented
  mismatches remain.

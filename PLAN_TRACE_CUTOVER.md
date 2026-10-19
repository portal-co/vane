# Plan: full cutover to the StackOp trace interpreter

**Status (2026-10-18):** drafted. Goal: make the StackOp **trace engine the only
execution engine** — remove the step engine (`step_one`-in-a-loop) and the
`privileged_step` boundary fallback — by teaching the tracer to compile
*privileged* instructions (and the other currently-boundary'd instructions:
AMOs, extended FP) into the trace. One engine, one semantics, and the
M-mode-dominated Linux boot runs as traces instead of per-instruction dispatch.

## Why

Today there are two execution engines:

- **Trace engine** (`trace_run`): runs `rv_emit`-produced `StackOp` traces. Fast
  (traces amortize decode + dispatch), but it *boundaries out* on any
  privileged/AMO/extended-FP instruction: `rv_emit` emits `PushImm(pc);
  TailCall`, the dispatch loop re-runs the instruction through `step_one`.
- **Step engine** (`step_one` in a loop, `--engine step`): the single-instruction
  interpreter that handles everything. The semantic reference and the fallback.

The Linux boot is dominated by privileged code (OpenSBI M-mode, kernel trap
handlers, CSR churn). Every privileged instruction ends its trace, so the boot
effectively runs at stepper speed with extra dispatch overhead. Folding the
privileged instructions *into* the trace is where the speed is.

## Design

The privileged semantics already exist as callable hart methods
(`Hart::csr_op`, `mret`, `sret`, `raise`, the FP exec, the AMO exec). The
cutover invokes them *from inside the trace* instead of boundarying out:

- **`StackOp::Privileged { pc: u64, raw: u32 }`** (new variant) carries the raw
  instruction and its pc. Emitted by `rv_emit` in place of the
  `PushImm(pc); TailCall` boundary.
- **`StackHost::privileged(&mut self, pc, raw) -> Result<PrivOutcome, _>`** (new
  trait method). The `LocalStackHost`/`LocalReactor` implementation decodes
  `raw` and executes it via the hart's existing methods — **one implementation
  of the semantics** (no second copy).
- **`PrivOutcome`**: `Continue` (the trace proceeds to the next op — e.g. a CSR
  write that doesn't trap) or `Divert(pc)` (the trace ends and the dispatcher
  resumes at `pc` — Mret/Sret, or a trap the hart raised and resolved to the
  handler pc). Non-diverting privileged ops no longer split the trace, which is
  the actual speedup.
- Trap atomicity is preserved: a faulting privileged op raises through the hart
  and diverts, never partially committing register state it shouldn't.

## Phases (commit each)

- **P1 — StackHost/LocalReactor surface.** Add `StackOp::Privileged` (+
  serializer support in `vane-riscv::stackop_ser`), `StackHost::privileged`,
  and the `LocalReactor` plumbing that executes the raw instruction through the
  hart. Land behind the existing trace engine without changing `rv_emit` yet
  (no behavioural change).
- **P2 — trace the privileged instructions.** `rv_emit` emits `Privileged` for
  `Csrrw/s/c{,i}`, `Mret`, `Sret`, `Wfi`, `SfenceVma`, `Ebreak`, `FenceI`
  (dropping the boundary). The trace continues through non-diverting ops.
- **P3 — AMOs and extended FP.** Lower `Lr/Sc/Amo{W,D}` and the FP ops the
  tracer currently boundaries (`Fmadd*`, `Fsgnj*`, `Fmin/Fmax`, `Fclass`,
  `Fmv*`, `Fle*`) — either to primitive StackOps or via the same `Privileged`
  path. Remove them from the boundary list.
- **P4 — remove the step engine.** Delete the `--engine step` path and the
  `privileged_step` boundary; `step_one`'s privileged core is subsumed by the
  P1 handler. Keep a minimal single-instruction stepper only where a test or
  the fuzzer genuinely needs a semantic reference (decide per-call-site).

## Acceptance

- Machine gate (`M1:TTTK`) and the workspace green with the trace engine only.
- The Linux boot (M4 / R8 gate) runs the privileged hot paths as traces —
  measurably fewer dispatches / faster than the pre-cutover trace engine.
- No behavioural divergence: the cross-fuzz reference still agrees.

## Notes / risks

- **Trap atomicity** (`Exit::Bail`): a privileged op that faults mid-trace must
  not leave partial register writes. The interpreter already restores the entry
  snapshot on `Bail`; the `Privileged` handler must route faults the same way.
- **The trace cache key** is the pc; a trace containing a `Privileged` op is
  still invalidated on `fence.i`/`satp` (the existing `code_dirty` path).
- **js-host**: the JS-side host must implement `privileged` too, or the trace
  engine stays the default only where the host supports it.
- **`StackOp` isn't `Clone`** — the serializer takes refs; the new variant must
  round-trip.

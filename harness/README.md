# vane harness

Integration gates and guest-side artifacts. See `PLAN_LINUX_WASM.md`,
`PLAN_VIRTIO_WEVAL.md`, and `PLAN_PI_IN_VM.md` for the milestone structure each
gate belongs to.

## Gate conventions

- Gate scripts are named `run-<stage>-gate.sh`, are executable, and exit 0 on
  pass / non-zero on fail with a clear first failing line on stderr.
- Every gate accepts `--from-snapshot <S<n>>` (default: the latest snapshot for
  its stage) and `--cold` (full boot path). Snapshots live in
  `harness/snapshots/` (not committed).
- Every gate records timing to `harness/results/<YYYY-MM-DD>-<stage>.json`
  with at least: `{stage, started_at, wall_seconds, from_snapshot, pass,
  host_os, runtime, notes}`. These results feed `PLAN_PI_IN_VM.md` §9.
- Guest-side helper scripts live in the image at `/opt/gates/` (built by
  `build-agent-image.sh`); the host-side drivers here talk to them over the
  console.

## Directories

- `linux/` — boot artifacts for the M4 Linux gate (fetched, not committed).
- `net/` — networking gate helpers: host echo server, TLS test endpoint, and
  the per-runtime socket-extension support matrix (`net/README.md`).
- `node/` — pinned Node.js riscv64 artifact versions + hashes (`VERSIONS`).
- `guest/` — files baked into the agent disk image (see
  `build-agent-image.sh`), including `guest/README.md` describing the in-VM
  environment contract for agents.
- `share/` — host-side root of the default virtio-9p share (fixtures; the
  repo itself is shared separately at gate time).
- `mock-provider/` — the deterministic LLM-provider mock used by the Pi gate.
  `recordings/` holds recorded request/response sessions (see PLAN_PI_IN_VM
  Q3: replayable recorded session; `mock-provider` replays a recording in
  order, and `--record` captures a new one from a real provider).
- `results/` — gate timing/output records (committed; small JSON only).
- `snapshots/` — machine snapshots S0–S3 (not committed; large).
- `mitm/` — generated CA + proxy state for `--mitm` runs (never committed;
  see `.gitignore`).

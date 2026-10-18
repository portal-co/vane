# Plan: End-to-end Pi agent in the vane VM — WASIX TCP tunnel, WASI threads + socket extensions, Node → HTTPS → npm → Pi

Status: **DRAFT, living document** (created 2026-10-19, branch `mile/agent-in-vane`).
Scope: this repo (`vane` / `vane-agent`), the sibling `os-emulation` host interface, and
guest-side artifacts under `harness/`.
Builds on: `PLAN_LINUX_WASM.md` (boot, no-JIT, WASIX host) and `PLAN_VIRTIO_WEVAL.md`
(virtio-blk / virtio-9p / virtio-net, machine snapshots, acceleration).

Ultimate goal: **run the Pi coding agent (`@earendil-works/pi-coding-agent`) end-to-end
inside the vane-emulated RISC-V Linux guest, with guest TCP tunneled to the host through
WASIX sockets, and prove the whole stack with a staged test chain:
Node → Node HTTPS → npm → Pi with `github:gkgoat1/pi-surplus-intelligence`.**

This document is a **reusable reference for any agent (human or LLM) working in this
area**: it records the architecture, the gates, the runbooks, the performance budget, and
a running blocker log. It is updated as we merge, coordinate, build, and test — see §12
for the update protocol.

---

## 1. Goals, non-goals, success criteria

### 1.1 Goals

- **G1. Networking to the host.** Guest TCP connections (initiated by Node/npm/Pi) are
  tunneled to host TCP connections through WASIX socket calls (`sock_*`) made by the vane
  WASM module running under a WASIX-capable runtime (primary: wasmer). DNS resolution
  happens host-side.
- **G2. WASI threads.** The I/O pump (device `tick()`, network poll, UART) runs on a
  WASI thread — either WASIX threads under wasmer (already prototyped via the `wasix`
  feature in `vane-cli`) or the `wasi-threads` proposal (`thread-spawn`) under
  wasmtime/other runtimes. The emulator core stays single-hart.
- **G3. Unofficial WASI socket extension.** In addition to WASIX, support the
  de-facto/unofficial preview1 socket extensions (`sock_open`/`sock_bind`/`sock_connect`/
  `sock_listen`/`sock_accept`/`sock_send`/`sock_recv`/`sock_shutdown` family as exposed by
  wasmer's WASIX and by runtimes that carry the pre-preview2 socket patches), so the same
  tunnel backend works on hosts that have sockets but not full WASIX. This is a
  portability layer, not a second network stack.
- **G4. The test chain.** Staged, independently-runnable gates (each with a
  `harness/run-*-gate.sh` script):
  1. **Node** — a riscv64 Linux Node.js binary runs in the guest (`node -e` hello).
  2. **Node HTTPS** — Node in the guest fetches an `https://` URL through the tunnel.
  3. **npm** — `npm install` of a small package completes in the guest.
  4. **Pi** — the Pi CLI runs non-interactively in the guest with the
     `github:gkgoat1/pi-surplus-intelligence` extension installed and loaded.
- **G5. Reusable guest environment.** The guest image has pre-installed extensions and
  configuration so any agent dropping into the VM gets a working Pi without a network
  install. Host files (repo checkouts, extension source under development, CA bundle) are
  shared via virtio-9p; guest-owned files (rootfs, npm cache, Pi data) live on the
  virtio-blk disk.
- **G6. Snapshotted booted state.** A machine snapshot taken after boot + environment
  setup is the canonical starting point, so each test run skips the slow interpreter
  boot. Derived snapshots (per PLAN_VIRTIO_WEVAL Workstream E) bracket each chain stage.
- **G7. Optional TLS interception.** A host-side MITM proxy mode (guest trusts a
  generated CA) for debugging, caching, and offline replay of npm/registry traffic.
  Off by default; the chain must pass with it on and off.
- **G8. Reasonable performance.** Boot-to-shell and each gate complete inside documented
  time budgets (§9), using snapshots and trace acceleration where needed. Numbers are
  recorded in this doc as we measure them.

### 1.2 Non-goals

- Multi-hart SMP in the guest (WASI threads are for the host-side I/O pump only).
- A second network path (browser WebSocket relay) — that remains a PLAN_VIRTIO_WEVAL
  browser concern; this plan is the WASIX/native-host path.
- Running the Pi TUI interactively (the gates drive Pi non-interactively; the TUI over
  the UART console is a stretch goal).
- Guest-initiated inbound connections (port forwarding into the guest) — outbound only.
- Shipping any of this as a product; this is a test/dev environment.

### 1.3 Success criteria (acceptance tests)

| # | Criterion | How verified |
| --- | --- | --- |
| P1 | Guest opens a TCP connection to a host echo server; bytes round-trip through the WASIX tunnel | `harness/run-net-echo-gate.sh` |
| P2 | The same echo gate passes with the network backend running on a WASI thread (WASIX thread under wasmer; `wasi-threads` build under wasmtime where supported) | `run-net-echo-gate.sh --threaded` |
| P3 | The same echo gate passes against the unofficial socket extension backend (non-WASIX host path) | `run-net-echo-gate.sh --backend sock-ext` |
| P4 | `node -v` and a `node -e` script run in the guest | `run-node-gate.sh` |
| P5 | Node `https.get` fetches a URL through the tunnel and validates the certificate (default mode) | `run-node-https-gate.sh` |
| P6 | Gate P5 also passes with TLS interception enabled (guest trusts the generated CA; proxy logs the flow) | `run-node-https-gate.sh --mitm` |
| P7 | `npm install <small package>` completes in the guest with a cold cache | `run-npm-gate.sh` |
| P8 | Pi runs a non-interactive task in the guest with `github:gkgoat1/pi-surplus-intelligence` installed; extension load is verified (Pi reports it / its tools are callable) | `run-pi-gate.sh` |
| P9 | The full chain P4→P8 starts from the post-setup snapshot, not a cold boot | gate scripts default to `--from-snapshot` |
| P10 | Every gate's wall time is within the budget in §9 (or the budget is explicitly revised in this doc with a reason) | timing recorded in §9 table |

---

## 2. Current state (ground truth, as of 2026-10-19)

### 2.1 What exists

- **Boot**: OpenSBI → Linux boots under `vane-cli --machine` on the StackOp interpreter
  (no JIT required). `harness/linux/README.md` documents the artifacts; boot takes tens
  of minutes on the interpreter (~1M instr/s) — the single biggest performance fact in
  this plan.
- **WASIX host feature**: `crates/vane-cli` has a `wasix` feature (stdin pump on a
  background thread, channel into the machine loop) — the pattern G2 generalizes.
- **Virtio plan**: PLAN_VIRTIO_WEVAL defines the virtio-mmio transport, virtio-blk
  (`BlockBackend` incl. COW), virtio-9p (backend = `os_unix_emulation::AsyncUnixHost`),
  virtio-net (`NetBackend`, smoltcp fallback), machine snapshots, and Weval/patcher
  acceleration. Landed so far (git log): the custom wasm patcher + Weval wrapper (W2/W3),
  multi-pass acceleration over derived snapshots (W4). Device milestones V1–V3 are the
  dependency for this plan's networking and storage.
- **os-emulation** (`../os-emulation`): `AsyncUnixHost` fs surface (the 9p backend),
  `TunneledHostApi` (native bridge). **No socket trait yet** — G1/G3 add one.
- **Snapshot pieces**: encrypted swap format, `vane-swap`, `restore_regs`; full machine
  snapshots are PLAN_VIRTIO_WEVAL Workstream E (with Wizer-style pre-init noted in the
  W3 redesign).

### 2.2 Established external facts

- **Node on riscv64 Linux**: no official builds; `unofficial-builds.nodejs.org` ships
  `linux-riscv64` tarballs (glibc) for recent LTS lines. Fallback: cross-build or build
  under Homebrew QEMU per the global AGENTS.md Linux-coverage rules (fail-closed: missing
  QEMU/image = reported prerequisite, never silently substituted).
- **Pi**: distributed as the npm package `@earendil-works/pi-coding-agent`; extensions
  install via `pi install <source>` where `<source>` may be a `github:owner/repo` spec.
  Pi is pure Node + prebuilt native bits where needed — audit for native modules early
  (risk R3).
- **WASIX sockets**: wasmer's WASIX provides the full `sock_*` preview1-style syscall
  family incl. `sock_accept`, plus DNS. wasmtime's preview1 does **not**; the
  "unofficial socket extension" (the pre-preview2 `sock_*` patches carried by some
  runtimes) is the portability target for G3. Verify per-runtime coverage before gating.
- **Interpreter speed** means cold boots and npm installs are the long poles; snapshots
  and acceleration are load-bearing, not optional polish.

### 2.3 Coordination context

Multiple Pi terminals on the local link are working adjacent areas (`vane`, `spday`,
`spday-net-snapshot-filtering`, `aero2`, `volar-ir`, `mpc`, …). This doc is the shared
reference for the "agent in the VM" direction; §12 defines how we keep it in sync.

---

## 3. Architecture overview (target state)

```text
┌────────────────────────────── host (macOS / Linux) ───────────────────────────────┐
│                                                                                    │
│  wasmer (WASIX: threads + sock_*)  |  wasmtime (wasi-threads + sock ext)          │
│      ▲ WASIX syscalls                    ▲ unofficial sock_* extension             │
│  ┌───┴────────────────────────────────────────────────────────────────────────┐   │
│  │ vane (wasm32-wasmer-wasi / wasm32-wasip1-threads)                          │   │
│  │  ┌──────────────┐   ┌─────────────────┐   ┌─────────────────────────────┐  │   │
│  │  │ machine core │──▶│ virtio devices  │──▶│ NetBackend: smoltcp ──▶     │  │   │
│  │  │ (StackOp     │   │ blk / 9p / net  │   │  TcpTunnel ──▶ WASIX sock_* │  │   │
│  │  │  interp +    │   │                 │   │  (guest IP ↔ host TCP map)  │  │   │
│  │  │  trace accel)│   │ 9p backend:     │   │  DNS: host-resolved         │  │   │
│  │  └──────────────┘   │  AsyncUnixHost  │   └─────────────────────────────┘  │   │
│  │        ▲ I/O pump on WASI thread (wasix thread / wasi-threads spawn)       │   │
│  └────────┼───────────────────────────────────────────────────────────────────┘   │
│           │ host TCP (optionally via MITM proxy: TLS intercept + cache)            │
└───────────┼────────────────────────────────────────────────────────────────────────┘
            ▼
     registry.npmjs.org / github.com / LLM API endpoints / host echo server

Guest (riscv64 Linux, single hart):
  virtio-blk disk: rootfs, Node install, npm cache, Pi + extensions, guest-owned state
  virtio-9p share: host repo checkouts, extension source under dev, generated CA bundle
  snapshot chain:  S0 booted → S1 +node → S2 +npm-cache-warm → S3 +pi-installed
```

Key design decisions:

- **D1. Tunnel at the TCP-stream level, not the packet level, where the host allows.**
  smoltcp terminates the guest's TCP; each guest flow maps to one host `sock_connect`ed
  WASIX socket. Guest IP addresses are synthetic (static map or host DNS resolution).
  This sidesteps TUN/TAP privileges and works identically under wasmer and the socket
  extension path. Raw-packet bridging remains available via PLAN_VIRTIO_WEVAL's native
  backend but is out of scope here.
- **D2. One `SocketBackend` trait, two host bindings.** `sock_connect/send/recv/shutdown`
  (+ `sock_accept` later) behind a trait; bindings for WASIX (wasmer) and the unofficial
  socket extension. Selection is a CLI flag + runtime probe, never silently assumed.
- **D3. Host-side DNS.** smoltcp answers guest DNS queries by forwarding to host
  resolution (WASIX `sock_resolve` where present, else a host helper process over the
  existing tunneled-stdio seam). Keeps guest config static.
- **D4. 9p for host↔guest, blk for guest-only.** Anything the host must read/write
  (source trees, CA cert, test fixtures, gate logs) goes over 9p. Anything that must
  survive without the host share (rootfs, npm cache, Pi install) goes on the disk image.
- **D5. Snapshots are the interface between chain stages.** Each stage's gate script
  restores the previous stage's snapshot by default; `--cold` forces the full path.
  Snapshot format and COW disks come from PLAN_VIRTIO_WEVAL (E + B).

---

## 4. Networking workstream (G1–G3, G7)

### 4.1 Guest side

- virtio-net device (PLAN_VIRTIO_WEVAL Workstream D) with smoltcp as the in-vane TCP/IP
  stack: guest sees a static IP (`10.0.2.15/24`, QEMU-user-style), gateway `10.0.2.2`,
  DNS `10.0.2.3`.
- smoltcp sockets are driven from the device `tick()` / coroutine seam; each accepted
  guest TCP connection spawns a tunnel entry: `{smoltcp socket} ↔ {WASIX fd}`.

### 4.2 Host side: `SocketBackend`

```rust
// crates/vane-host (or os-emulation) — shape, not final API
trait SocketBackend {
    fn connect(&mut self, host: &str, port: u16) -> io::Result<SockFd>;
    fn send(&mut self, fd: SockFd, buf: &[u8]) -> io::Result<usize>;
    fn recv(&mut self, fd: SockFd, buf: &mut [u8]) -> io::Result<usize>; // non-blocking + WouldBlock
    fn shutdown(&mut self, fd: SockFd, how: Shutdown) -> io::Result<()>;
    fn resolve(&mut self, name: &str) -> io::Result<Vec<IpAddr>>;
}
```

- **Binding A: WASIX (wasmer).** Direct `sock_connect`/`sock_send`/`sock_recv`/
  `sock_shutdown`/`sock_resolve` calls. Non-blocking sockets + `poll_oneoff` in the pump
  thread.
- **Binding B: unofficial socket extension.** Same surface against runtimes carrying the
  pre-preview2 `sock_*` patches. Feature-detect at startup (probe import availability);
  degrade to Binding A-only with a clear error if neither is present.
- **Threading**: the pump loop (poll host sockets + smoltcp + device ticks) runs on a
  WASI thread. Under wasmer: WASIX thread (extends the existing `wasix` feature
  pattern). Under wasi-threads runtimes: `thread-spawn` import; shared memory required —
  audit `Mem`/device state for `Send`/sync safety and guard shared pages.

### 4.3 TLS interception (optional, off by default)

- Host-side MITM proxy (e.g. a small Rust/Go proxy, or `mitmproxy` for debugging) on
  `127.0.0.1:<port>`; vane connects there instead of the origin when `--mitm` is set.
- A generated CA cert is placed on the 9p share and installed into the guest's trust
  store (`SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS` for Node, system store for completeness).
- Use cases: inspect npm/Pi traffic, record/replay registry responses for offline runs,
  bandwidth shaping tests. **Not** a covert channel: it is a documented, flag-gated dev
  tool; logs live in `harness/mitm/`.
- Per the global LuLu note: new host binaries hitting the network may need LuLu
  approval; a network prompt is not "no network".

### 4.4 Gates

- `run-net-echo-gate.sh` — host echo server + guest `nc`/busybox client (P1–P3).
- DNS probe baked into the echo gate (resolve then connect).

---

## 5. Guest environment workstream (G5)

### 5.1 Disk image (guest-owned, virtio-blk)

Build script `harness/build-agent-image.sh` (runs under Homebrew QEMU or from
prebuilt riscv64 artifacts; fail-closed on missing prerequisites):

1. Base: Alpine riscv64 rootfs (consistent with the existing minirootfs artifacts).
2. Node.js: `unofficial-builds.nodejs.org` `linux-riscv64` tarball pinned by version +
   SHA256 in `harness/node/VERSIONS`; verify glibc-vs-musl compatibility (Alpine is musl —
   prefer the musl unofficial build if present, else switch base to a glibc rootfs; **open
   question Q2**).
3. npm cache pre-warm: `npm install -g @earendil-works/pi-coding-agent` and
   `pi install github:gkgoat1/pi-surplus-intelligence` inside QEMU so the image ships
   with Pi + extension pre-installed (G5).
4. Pi config: pre-seeded `~/.pi` settings for headless/non-interactive runs; model
   credentials come from the environment at gate time, never baked into the image
   (§11, Sec-1).
5. Gate helpers: `/opt/gates/` scripts the harness invokes over the console.

### 5.2 9p share (host-owned)

- Mounted at `/mnt/host`; default exports: this repo (read-write for gate logs), a
  `harness/share/` fixtures dir, the MITM CA bundle (read-only), and the extension
  source checkout for live development (edit on host, run in guest).
- Path confinement per PLAN_VIRTIO_WEVAL §5 (reject `..` escapes; allow-listed roots).

### 5.3 Environment contract for agents in the VM

Documented in `harness/guest/README.md` (to be written): where things live
(`/mnt/host`, `/opt/gates`, `node`/`npm`/`pi` on PATH), how networking works (all TCP
tunneled; DNS host-side; possible MITM CA), and the performance caveats (slow first run,
snapshot restores).

---

## 6. Snapshot workstream (G6)

- Snapshot points: **S0** post-boot idle shell → **S1** S0 + Node verified →
  **S2** S1 + warm npm cache → **S3** S2 + Pi + extension installed/configured.
- Format/restore per PLAN_VIRTIO_WEVAL Workstream E (guest RAM sparse + swap-aware, hart,
  devices incl. virtio-net/smoltcp state, COW disk per derived snapshot).
- **Network state across restore**: smoltcp socket state does not survive restore
  meaningfully (host fds are gone) — snapshots are taken with **no open guest
  connections**; gates open fresh connections after restore. Add a "quiesce" step to the
  snapshot procedure (flush block device, close sockets, sync).
- Wizer-style pre-init (from the W3 redesign note) is the fast-boot complement for the
  vane module itself; machine snapshots cover guest state.
- Gate scripts take `--from-snapshot S<n>` (default: latest for that stage) / `--cold`.

---

## 7. Test chain (G4) — stage detail

Each stage: goal, gate script, pass criteria, artifacts. All scripts live in `harness/`,
follow the existing `run-*-gate.sh` conventions, and record timing into
`harness/results/<date>-<stage>.json`.

| Stage | Gate | Pass criteria |
| --- | --- | --- |
| 0 Boot | existing `run-linux-boot-gate.sh` | shell prompt on console |
| 1 Net | `run-net-echo-gate.sh` | P1–P3 |
| 2 Node | `run-node-gate.sh` | `node -v` prints expected version; `node -e 'crypto…'` exercises crypto; a compute microbenchmark records instr/s (§9) |
| 3 HTTPS | `run-node-https-gate.sh` | fetch `https://registry.npmjs.org/npm` (and a host-controlled TLS endpoint), cert validation on; repeat `--mitm` |
| 4 npm | `run-npm-gate.sh` | cold-cache `npm install` of a pinned tiny package (e.g. `left-pad`-scale) into a guest tmpdir; `node -e require()` of it |
| 5 Pi | `run-pi-gate.sh` | `pi --version`; extension list shows `pi-surplus-intelligence`; a non-interactive run (`pi -p` / print mode or RPC) completes a trivial task with a **stub/mock provider endpoint** (host-served, so gates don't burn API quota) and optionally a real provider when credentials are present |
| 6 E2E | `run-agent-e2e-gate.sh` | full chain from S3 snapshot: Pi edits a file on the 9p share, result visible on host |

Stage 5 detail:
- Extension install verification both ways: from the pre-installed image (G5) and a
  live `pi install github:gkgoat1/pi-surplus-intelligence` in the guest (exercises real
  GitHub over the tunnel).
- The mock provider is a tiny host HTTPS server speaking the provider API shape, so the
  Pi gate is deterministic and offline-capable.

---

## 8. WASI threads plan (G2)

1. Generalize the existing `vane-cli` `wasix` stdin-pump thread into an **I/O pump
   thread** owning: host socket poll, smoltcp poll, device ticks, UART drain.
2. Communication: channels into the machine loop (established pattern); the hart loop
   remains single-threaded and polls a "pending device work" flag.
3. wasm32-wasip1-threads build (`wasi-threads`): feature-gate the pump spawn; require
   shared linear memory — check `Mem` page store and device structs for cross-thread
   access and wrap accordingly (or keep page store on the hart thread and pass buffers).
4. Test: P2 gate variants; assert pump thread actually spawned (log line + thread count
   where the runtime exposes it).

---

## 9. Performance budget (G8) — living table

Interpreter baseline: ~1M instr/s (harness README). Budgets are macOS-host, wasmer,
from-snapshot unless noted. **Fill in measured columns as gates land.**

| Stage | Budget (target) | Measured (date) | Notes |
| --- | --- | --- | --- |
| Cold boot → shell | ≤ 45 min interp / ≤ 5 min accelerated | — | snapshot S0 taken here |
| Restore S0 → shell | ≤ 2 min | — | |
| `node -v` from S1 | ≤ 5 min | — | Node startup is syscall-heavy |
| HTTPS fetch (stage 3) | ≤ 5 min | — | TLS handshake in-guest is pure CPU |
| npm install tiny pkg (stage 4) | ≤ 20 min cold cache | — | tarball download is small; install is fs-heavy |
| Pi boot + trivial task (stage 5, mock) | ≤ 20 min | — | |
| Full chain stage 2→5 from S3 | ≤ 45 min | — | |

Acceleration levers, in order of expected payoff:
1. Snapshots (§6) — removes boot from every run.
2. StackOp→WASM trace precompilation / Weval / patcher (PLAN_VIRTIO_WEVAL W2–W4) —
   apply to the hot loops observed during gates; record which pass helped.
3. Reduce syscall overhead in the tunnel (batched `sock_recv`, vectored writes).
4. Guest-side: `npm ci --prefer-offline`, pre-warmed caches, musl vs glibc choice.

If a budget is missed, either fix perf or revise the budget **in this table with a
reason** — do not let numbers silently rot.

---

## 10. Milestones

Status column is kept current (`todo` / `doing` / `done` / `blocked(reason)`).

| # | Milestone | Depends on | Status |
| --- | --- | --- | --- |
| M0 | This plan reviewed; harness dir structure + results conventions | — | done |
| M1 | virtio-net device + smoltcp `NetBackend` guest-side (PLAN_VIRTIO_WEVAL D) | V1 transport | done (device by vane terminal; smoltcp tunnel landed with M2) |
| M2 | `SocketBackend` trait + std/WASIX binding (`StdSockets`), smoltcp tunnel `vane-net`, loopback echo tests, `--net`/`--net-map` CLI wiring, echo gate P1 authored (execution pending boot artifacts) | M1 | done (gate run pending) |
| M3 | I/O pump on a thread: `PumpedTunnel` (std::thread = WASIX thread on wasm32-wasmer-wasi), `--net-pump`, pumped echo test | M2 | done (native; WASIX-target verification pending) |
| M4 | Socket-extension binding + runtime probe; P3 | M2 | done: `WasixSockets` validated live on wasmer 7.5 (`examples/wasix_probe.rs`, P1-equivalent echo over the raw binding) |
| M5 | DNS forwarding (tunnel DNS responder at 10.0.2.3, host-side resolve, synthetic answers) | M2 | done |
| M6 | Machine snapshots: `ReactorSnapshot`/VSNP existed; added CLI `--snapshot-save/--snapshot-marker/--snapshot-restore/--exit-after-snapshot` | boot gate | done (CLI); gate integration pending |
| M7 | Agent disk image build (Node v24.22.0 glibc pinned) under QEMU; node gate P4 | M2 | doing: `build-agent-image.sh` authored (fail-closed, QEMU nocloud+seed); not yet executed |
| M8 | HTTPS gate P5 (+P6 MITM optional mode) | M2,M7 | doing: gate + guest script authored; `--mitm` stubbed fail-closed; pending image + disk-boot flow |
| M9 | npm gate P7; snapshots S1–S2 | M8 | doing: gate + guest script authored; pending image + disk-boot flow |
| M10 | Pi + extension pre-installed in image; mock provider; pi gate P8; snapshot S3 | M9 | doing: mock provider done; image provisioner installs pi+extension; gate authored; pending image + disk-boot flow |
| M11 | E2E gate P9–P10; performance table filled; runbook final | M10 | todo |
| M12 | wasi-threads pump variant (wasmtime) | M3 | stretch |

Parallelization notes: M7 (image) can start once Node artifacts are decided, independent
of M1–M5; M6 is on the PLAN_VIRTIO_WEVAL critical path owned by the `vane` direction.

---

## 11. Security / handling notes

- **Sec-1. No credentials in images or snapshots.** API keys enter at gate runtime via
  env; snapshots must be taken in a credential-free state. Verify with a grep of the
  snapshot/disk for known key patterns in the image build script.
- **Sec-2. MITM CA is dev-only**, generated per-checkout under `harness/mitm/`, never
  committed, and the guest trust install is part of the `--mitm` flow only.
- **Sec-3. 9p confinement** per PLAN_VIRTIO_WEVAL §5; the share is the only host fs
  surface the guest can touch.
- **Sec-4. LuLu**: first run of wasmer/QEMU/proxy binaries may trigger host firewall
  prompts; treat prompts as approval flow, not connectivity failure.

---

## 12. Living-document protocol

1. **Status flips** in §10 and **measurements** in §9 are committed with the gate/change
   that produced them (`[AI] PLAN_PI_IN_VM: …` commit prefix, matching repo convention).
2. **Blockers** go into §13 the moment they're found, with owner direction and date; a
   blocker leaves the log only by being fixed or explicitly descoped (record which).
3. **Cross-terminal coordination**: when another link terminal (e.g. `vane`, `spday*`,
   `aero2`) changes something this plan depends on (snapshot format, device model,
   os-emulation surface), update §2/§13 here and say so in the link message.
4. **Large direction changes** (e.g. switching base distro, moving from smoltcp to
   passthrough sockets) get a short "Decision" entry appended to §14, not an inline
   rewrite of history.

---

## 13. Blocker / risk log (living)

| # | Risk/blocker | Impact | Mitigation | Status |
| --- | --- | --- | --- | --- |
| R1 | Node riscv64 unofficial build is glibc-only; Alpine rootfs is musl | stage 2 | no musl build exists → Debian glibc base (Q2) | closed 2026-10-19 |
| R2 | Pi or its deps ship native modules without riscv64 prebuilds | stage 5 blocked | audit `npm ls` after image build; stub/replace or build natives in QEMU | open |
| R3 | Unofficial socket-extension coverage varies by runtime | G3 | **wasmer 7.5 validated 2026-10-19**: `wasix_probe.wasm` (wasm32-wasip1 + raw `wasix_32v1` imports) under `wasmer run --net` — open/resolve/connect/send/recv/shutdown all pass. **ABI finding**: wasmer's `Socktype` is Unknown=0/Stream=1/Dgram=2 — the wasix *crate* 0.12.5 table (Dgram=0) is stale; trust the runtime. Runtime requires `--net`. | closed |
| R4 | Interpreter perf makes npm/Pi stages exceed budget | G8 | snapshots + trace acceleration; budget table is the tracking mechanism | open |
| R5 | Shared-memory requirements of wasi-threads conflict with `Mem`'s page store | M12 | keep page store hart-local; pass buffers across the channel | open |
| R6 | smoltcp socket state on snapshot restore | G6 | quiesce-with-no-open-sockets rule (§6) | designed, unverified |
| R7 | Guest TLS cert validation vs system time (no RTC) | stage 3 flake | **mitigated 2026-10-19**: `vane_epoch=<unix>` cmdline convention — host gates append it (`lib-gate.sh::gate_epoch_arg`), guest preamble `00-preamble.sh` runs `date -s @epoch`; no emulator change. RTC device remains a nice-to-have | closed |
| R8 | Disk-rootfs boot flow | stages 2-5 | **2026-10-19 findings**: (1) Debian netboot kernel never probes virtio (modules) — t-34d8 confirmed at the root mount (`unknown-block(0,0)`); (2) the cloud-image shortcut is dead too — Debian riscv64 7.2.11 builds VIRTIO_MMIO/VIRTIO_BLK/VIRTIO_NET/9P/EXT4 **all modular**, and EXT2_FS is unset entirely; (3) chosen path: stock kernel + gate initramfs carrying the module set (`harness/linux/fetch-virtio-modules.sh`, 10 modules verified) insmod'd in /init, root on **ext4** (not ext2!); initramfs-modules gate authored here (`build-initramfs-virtio.sh` + `run-disk-rootfs-gate.sh`: /init insmods the module set, mounts /dev/vda via ext4 driver — ext2 images work through EXT4_USE_FOR_EXT2=y — switch_root to the disk /init); t-34d8 runs the long boot | doing (gate authored; first run on t-34d8) |
| R9 | `WasixSockets` connect polls `sock_status` in a busy loop | wasm perf | **fixed 2026-10-19**: `poll_oneoff` clock-subscription sleep (10ms steps, 30s budget), validated on wasmer 7.5 (probe rerun: WASIX-PROBE-OK) | closed |

## 14. Open questions / decisions

- Q1: Node version — **DECIDED 2026-10-19: v24.22.0** (latest v24 LTS with a
  `linux-riscv64` unofficial build), pinned with SHA256 in
  `harness/node/VERSIONS`.
- Q2: musl vs glibc — **DECIDED 2026-10-19: glibc.** The unofficial riscv64
  Node builds are glibc-only (no musl variant exists), so the agent image is
  Debian riscv64 userland, not the Alpine musl minirootfs of the boot gates.
- Q3: Mock provider fidelity — **DECIDED 2026-10-19: replayable recorded session;**
  **IMPLEMENTED** in `harness/mock-provider/server.py` (replay in order, match on
  method+path, 409 with diagnostics on mismatch/exhaustion; `--record UPSTREAM`
  captures a real session to JSONL, redacting auth headers).
  The mock provider serves a recorded request/response session (fixtures under
  `harness/mock-provider/recordings/`), matched in order; recording mode captures a real
  provider session for later replay. This keeps the Pi gate deterministic, offline-
  capable, and realistic about multi-turn traffic.
- Q4: Do gates run under CI (needs wasmer + QEMU in CI, long timeouts) or stay
  runbook-only? Leaning runbook-first, CI for stages ≤ 3.

## 15. Change log

- 2026-10-19 — Initial draft (vane-agent terminal, branch `mile/agent-in-vane`).
- 2026-10-19 — Plan approved (LGTM); Q3 decided: replayable recorded session. M0 done.
  Implementation begins, committing after each milestone and part-milestone.
- 2026-10-19 — M1 confirmed done (virtio-net device + tests, courtesy of the `vane`
  terminal). M2 core landed: `vane-host::SocketBackend` trait, `vane-net` crate
  (smoltcp tunnel: guest 10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3 answering
  with synthetic 10.0.2.16+ addresses via smoltcp AnyIP, per-flow host sockets),
  `StdSockets` binding (native + wasm32-wasmer-wasi = WASIX path), and a
  loopback echo test (guest smoltcp stack → DNS → synthetic IP → TCP echo →
  close) passing in ~1.3 s. smoltcp gotchas found: interface address table is
  fixed-capacity 2 (hence AnyIP), neighbor discovery is rate-limited 1 s
  cache-wide.
- 2026-10-19 — M2b+M3+M6 landed: `--net/--net-map/--net-pump`,
  `PumpedTunnel` (pumped echo test passes), snapshot save/restore CLI flags.
  Q1/Q2 decided (Node v24.22.0, glibc/Debian base); R1 closed. R8 mitigated
  in code: the DNS responder socket binds 10.0.2.3 explicitly so replies
  carry the queried source address.
- 2026-10-19 — M4 binding authored: `vane-net::WasixSockets` behind the
  `wasix-raw` feature (raw `wasix_32v1` imports; ABI verified against
  wasix 0.12.5 sources and wasmer's `net::read_ip_port` — port is
  native-endian in memory). Compile-checked for wasm32-wasip1. Not yet run
  against a live runtime (R3); support matrix in `harness/net/README.md`.
  M7: `build-agent-image.sh` (QEMU nocloud + NoCloud seed, fail-closed) and
  the guest environment contract (`harness/guest/README.md`).
- 2026-10-19 — Mock provider (M10 part): `harness/mock-provider/server.py` with
  replay/record modes; JSONL recording format; smoke-tested (match → 200,
  mismatch/exhaustion → 409). `recordings/smoke.jsonl` documents the format.
- 2026-10-19 — Stage gates authored end-to-end: `lib-gate.sh` (fail-closed
  helpers + results JSON), `run-{node,node-https,npm,pi}-gate.sh`, in-guest
  `/opt/gates/02..05` scripts. Image build now publishes a raw `.img`
  (vane-cli FileBlock is raw-only). Gates are wired but blocked on R8
  (disk-rootfs boot flow) and the fetched artifacts; they fail closed with
  the exact missing piece named.
- 2026-10-19 — R8 handed to the vane terminal (t-34d8) with the confirmed
  contract: minimal offline gate = partitionless ext2 raw image, `--disk`,
  cmdline `root=/dev/vda rw rootfstype=ext2 console=ttyS0 panic=-1`, /init
  prints `=== VANE DISK-ROOTFS OK ===`; agent-image stages will use
  `root=/dev/vda1` (GPT) + /opt/gates/NN.sh; gate marker strings
  (`=== GATE <STAGE> OK ===`) are the UART contract for --snapshot-marker.
  Also confirmed on the link: VSNP format stable, virtio_net device model
  owned by t-baab and unchanged.
- 2026-10-19 — R3 closed: wasmer 7.5 (brew) runs `wasix_probe.wasm` with
  `--net` — full socket lifecycle passes against the host echo server.
  Found and fixed a real ABI bug this way: wasmer's Socktype numbering
  (Unknown=0, Stream=1, Dgram=2) differs from the wasix crate 0.12.5
  constants (Dgram=0); the runtime is ground truth. Note: `wasmer run`
  needs `--net` or sock_open returns NOTSUP(58).
- 2026-10-19 — R7 closed: `vane_epoch` kernel-cmdline clock convention wired
  through host gates, guest preamble, and the busybox net-gate init; guest
  README documents it.
- 2026-10-19 — R9 closed: connect now sleeps via poll_oneoff clock
  subscriptions instead of spinning on sock_status; revalidated live on
  wasmer 7.5. R8 in progress on the vane terminal (t-34d8, commit 6046707:
  offline ext2 rootfs builder + gate authored, boot running).
- 2026-10-19 — R8 deep-dive: t-34d8's minimal gate failed exactly at
  `root=/dev/vda` (virtio_blk modular in the netboot kernel); checked the
  cloud-image kernel as a shortcut — also fully modular (even EXT4_FS=m),
  and EXT2_FS unset (so ext2 images can never boot this kernel — switched
  the contract to ext4). Decision: no custom kernel build; the gate
  initramfs carries the module set instead. `fetch-virtio-modules.sh`
  landed (extracts crc32c/virtio_mmio/virtio_blk/mbcache/jbd2/ext4/
  virtio_net/9p* from linux-modules-7.2.11, plus load-modules.sh).
- 2026-10-19 — R8 integrated gate authored: `build-initramfs-virtio.sh` packs
  minirootfs + module set + loader /init (insmod → mount /dev/vda → marker
  readback → switch_root to the disk /init); `run-disk-rootfs-gate.sh`
  asserts the full chain and records timing JSON. Contract with t-34d8:
  their ext2 image unchanged (mounts via ext4 driver, EXT4_USE_FOR_EXT2=y);
  they run the first long boot.
- 2026-10-19 — R8 iteration: first boot ran the initramfs flow correctly but
  insmod hit version-magic mismatch (netboot kernel 7.0.13 vs my 7.2.11
  module set). fetch-virtio-modules.sh is now version-aware (magic read
  from the kernel image, package located via the sid index, snapshot.d.o
  fallback note). Also fixed a silent-death pipefail+grep-SIGPIPE bug.
  t-34d8 re-running with matched modules.
- 2026-10-19 — **Device-model bug found + fixed (vane 01ae931/7aa5f8a, ported
  here)**: the legacy virtio-mmio register map lacked DRIVER_FEATURES{,_SEL}
  (0x020/0x024), shifting the queue registers 8 bytes low — real kernels read
  QueueNumMax=0 and bailed (-ENOENT). The unit tests were self-consistent
  with the wrong map; only the real-kernel boot exposed it. Would equally
  have broken virtio-9p and virtio-net — re-verify their gates when the boot
  lands. Ported verbatim (files were identical to 01ae931^); all tests green.
- 2026-10-19 — R8: virtio_blk probe validated on the real boot (vda at image
  capacity); module-set gap found + fixed (ext4 needs crc16.ko, added before
  ext4 in load-disk.sh, commit 544fa36); loaders tiered disk/net/9p to cut
  interpreter boot time (689f5c7).
- 2026-10-19 — R8: crc16 gap fixed the ext4 load; new finding — the 128MiB
  default RAM OOMs the initramfs /init ("sh: out of memory" → panic);
  disk gate now uses --ram-size 0x20000000. Sizing guidance for the
  agent-image gates (1-2 GiB) recorded in harness/lib-gate.sh.

# Plan: VirtIO devices (9p / disk / network) + machine acceleration (Weval & custom patcher)

Status: **proposed** (2026-10-17). Builds on `PLAN_LINUX_WASM.md` (M0–M7 done/gated): the
machine boots OpenSBI→Linux on the StackOp trace engine with encrypted paging, a browser
surface, and a WASIX threaded host. This plan adds real I/O devices and a machine-level
acceleration pipeline.

Two independent thrusts, sharing the "derived snapshot" artifact:

- **VirtIO devices**: virtio-mmio transport plus virtio-blk (disk), virtio-9p (host
  filesystem sharing), and virtio-net (network bridged to native where possible). Device
  backends are **pluggable via the `os-emulation` host interface** so one device model
  runs native, under WASI, and in the browser by swapping the host backend.
- **Machine acceleration**: specialize a booted machine for speed with **Weval** (wasm
  partial evaluation) and a **custom binary patcher**, applied **potentially many times**
  to **derived snapshots** (boot → snapshot → accelerate → run → snapshot → accelerate …).

---

## 1. Goals, non-goals, success criteria

### 1.1 Goals

- G1. A shared **virtio-mmio** transport in `vane-riscv::machine` (or a new `vane-virtio`
  crate): split virtqueues, the MMIO register block, PLIC interrupt delivery. Legacy v1
  first (container2wasm/TinyEMU-style; mainline Linux supports it), modern v2 where
  required.
- G2. **virtio-blk**: a block device backed by a host image. Native: a file. Browser:
  OPFS. Pluggable block backend.
- G3. **virtio-9p**: host-directory sharing, backed by the `os-emulation`
  `AsyncUnixHost` fs surface (openat/getdents64/fstat/read/write/lseek), so any
  os-emulation backend (native tunneled, WASI, filtered) can serve it.
- G4. **virtio-net**: an Ethernet device **bridged to native networking where possible**
  (TUN/TAP or a userspace TCP/IP stack), pluggable via the os-emulation interface.
- G5. A **machine snapshot** format (guest RAM + hart + device + accelerator state) that
  can be saved, restored, and *derived* (a snapshot taken mid-run from a parent).
- G6. **Weval acceleration**: partially evaluate the machine against a snapshot to
  produce a faster specialized module, **wrapping Weval so structural data is never
  stripped**.
- G7. A **custom binary patcher** that patches specialized code into a module **without
  stripping any structural data** (purely additive/in-place), plugging into the existing
  compiled-trace registry.
- G8. **Multi-pass acceleration**: apply G6/G7 repeatedly across a chain of derived
  snapshots, each pass specializing the machine further.

### 1.2 Non-goals

- Full SMP (single hart, as today).
- Modern-only virtio features (vDPA, packed virtqueues) — split queues + legacy/modern
  mmio only.
- A from-scratch TCP/IP stack where a native bridge suffices (native bridge is preferred;
  smoltcp is the fallback for WASI/browser).
- GPU / input / sound devices.

### 1.3 Success criteria (acceptance tests)

- S-V1. Linux probes and uses a **virtio-blk** root device (`/dev/vda`); a guest can
  mount and read a file from a host-backed disk image. (Extends B1–B3 boot gates.)
- S-V2. **virtio-9p**: the guest mounts a host directory (`mount -t 9p`) and round-trips
  a file (write from guest, read back on host), served through the os-emulation fs
  interface. Verified with at least two os-emulation backends (native + one other).
- S-V3. **virtio-net**: the guest brings up an interface, gets an address, and reaches a
  host-local endpoint (e.g. a ping or TCP echo) through the native bridge.
- S-W1. A **snapshot** save→restore round-trip resumes a booted machine bit-identically
  (UART output and subsequent execution match the uninterrupted run).
- S-W2. **Weval** acceleration: a weval'd machine produces identical output to the
  reference run on a fixed workload, faster, with all structural data intact.
- S-W3. The **custom patcher** accelerates a module with *zero* structural-data loss
  (byte-level audit: every data segment/global/export of the input survives).
- S-W4. **Multi-pass**: ≥2 successive acceleration passes over derived snapshots each
  preserve correctness (identical output) and compose (the second pass runs on the first
  pass's module).

---

## 2. Current state (ground truth)

### 2.1 vane machine model (`crates/vane-riscv/src/machine.rs`)

- MMIO layout: test finisher `0x10_0000`, ACLINT `0x200_0000`, PLIC `0xC00_0000`, UART
  `0x1000_0000` (PLIC source 10), RAM `0x8000_0000`. `Machine::is_mmio` routes below-RAM
  accesses. `Machine::tick` pumps devices and sets `irq_dirty`; the PLIC aggregates
  sources and raises MEIP/SEIP. **PLIC sources 1..=8 are reserved for virtio**; UART is
  10.
- No virtio yet (§7.4 of PLAN_LINUX_WASM planned it for M4; only UART/ACLINT/PLIC/syscon
  landed). virtio-mmio target: `0x1000_1000+`, one 0x1000-sized slot per device.
- Device framework hook: MMIO dispatch + `tick()` at trace-loop boundaries (where WASIX
  threads / browser timers already pump async IO — the coroutine seam).

### 2.2 os-emulation host interface (`../os-emulation`)

- `os-host-api::HostApi` (sync): `read`/`write`/`close`/`exit`/`syscall`/`osfuncall` +
  link/manifest surface. Backends: `TunneledHostApi` (native libc/libSystem),
  `RedirectingHostApi`, `FilteredHostApi` (allow-list). This is the native-bridging seam.
- `os-unix-emulation::AsyncUnixHost` (async): `read`/`write`/`close`/`exit`/`openat`/
  `lseek`/`fstat`/`fstatat`/`getdents64`. **This is the virtio-9p backend surface.**
- `os-async::AsyncHostApi`: async `read`/`write`/`close`/`exit`/`syscall`.
- **No socket/network trait exists yet** — G4 requires either adding one to os-emulation
  or a smoltcp userspace stack driving a host fd via the existing read/write surface.

### 2.3 Acceleration substrate

- Compiled-trace registry (`crates/vane/src/lib.rs::register_compiled_trace`,
  `TraceFn = Rc<dyn Fn(&mut LocalStackHost<R>) -> Exit>`): the dispatch loop consults a
  per-pc registry of compiled callables before interpreting. **This is where Weval /
  JIT / patcher artifacts plug in.**
- `GuestCoroutine` + asyncify/JSPI (`docs/COROUTINES_ASYNCIFY.md`): host-agnostic
  suspend/resume for non-local hosts.
- Snapshot-adjacent pieces already exist: the encrypted swap format (paged-out state +
  key), `vane-swap`, `Reactor::restore_regs`, `os-page::snapshot::dump` (a debug memory
  dump). A full machine snapshot (RAM + hart + devices + accelerator state) does **not**
  yet exist.

### 2.4 Weval

Weval (WebAssembly partial evaluator) specializes a wasm module against a context
(concrete inputs/state) by constant-folding and dead-code-eliminating. Its normal output
can strip data segments/globals it deems unreachable — which would delete the machine's
structural state. Hence the **wrapper** (G6) that re-attaches/preserves structural data,
and the **custom patcher** (G7) that never strips.

---

## 3. Workstream A — virtio-mmio transport

A shared `vane-virtio` crate (or `vane-riscv::virtio` module):

- **Split virtqueue** driver: descriptor table, available ring, used ring; descriptor
  chain walking (in/out buffers), used-ring completion, suppression flags.
- **MMIO register block** (legacy v1 layout to start): magic `0x74726976`, version,
  device/vendor id, `device_features`/`guest_features`, `queue_sel`/`queue_num_max`/
  `queue_pfn` (legacy) **and** the modern `queue_desc`/`queue_driver`/`queue_device`
  (v2) register sets, `queue_notify`, `interrupt_status`/`interrupt_ack`, `status`, and
  the device-specific config region. Support both because kernels differ.
- **Device trait**: `VirtioDevice { fn device_id(); fn num_queues(); fn config_read/write(); fn notify(queue, &mut DeviceIo) }`, driven by the MMIO dispatch; raises its
  PLIC source. One instance per mmio slot at `0x1000_1000 + 0x1000*i`, PLIC source `1+i`.
- **DeviceIo abstraction**: guest-memory read/write for descriptor/ring access — goes
  through the existing swap-aware byte path, and is the point where a **non-local host**
  (GuestCoroutine/asyncify) can park if guest RAM is remote.
- DTB: emit virtio-mmio nodes (`compatible = "virtio,mmio"`, reg, interrupts) so Linux
  probes them.

## 4. Workstream B — virtio-blk (disk)

- Single-request virtqueue; read/write/flush; capacity from the backend; geometry in the
  config region.
- `BlockBackend` trait: `read_sectors(lba, &mut [u8])`, `write_sectors(lba, &[u8])`,
  `capacity_sectors()`, `flush()`. Backends:
  - **Native**: a host file (mirror `vane-swap`'s `FileSwap` pattern — works native and
    wasm32-wasip1 preopened files). Support raw images; QCOW2 is a stretch goal.
  - **Browser**: OPFS sync-access-handle (same bridge as the M6 swap store).
  - **Copy-on-write**: a COW layer over a read-only base image so snapshots/derived
    machines don't mutate the base (feeds Workstream 7).
- Test: S-V1 — boot with a virtio-blk rootfs, mount, read a file.

## 5. Workstream C — virtio-9p (host fs sharing)

- virtio-9p device speaking **9P2000.L** over one virtqueue: `lopen`/`lcreate`/`read`/
  `write`/`getattr`/`readdir`/`walk`/`clunk`, etc.
- **Backend = `os_unix_emulation::AsyncUnixHost`** (the pluggable interface): each 9P op
  maps to an `AsyncUnixHost` call (`openat`, `getdents64`, `fstat`, `read`, `write`,
  `lseek`). Any os-emulation backend — native `TunneledHostApi`, WASI, or a `Filtered`
  allow-listed tree — serves the device unchanged. The async surface plugs into the
  device `tick()`/coroutine seam.
- Security: root the 9p share at a host directory and confine all paths beneath it
  (reject `..` escapes), reusing `FilteredHostApi`'s allow-list idea at the fs layer.
- Test: S-V2 with two os-emulation backends.

## 6. Workstream D — virtio-net (bridged to native, pluggable)

- virtio-net device: rx/tx virtqueues, Ethernet frames, MAC in the config region,
  `VIRTIO_NET_F_STATUS`; optional merge-rx / checksum offloads as stretch.
- `NetBackend` trait: `send_frame(&[u8])`, `poll_recv() -> Option<Frame>`, plus link
  status. Bridging, in preference order:
  1. **Native bridge** (preferred, "bridged to native if possible"): TUN/TAP —
     macOS `utun`, Linux `/dev/net/tun` — giving the guest a real L2 presence on the
     host network. Root/sudo-gated; provide a setup wizard (`skills: wizard`).
  2. **os-emulation interface**: where a native L2 bridge isn't available, route frames
     through a userspace TCP/IP (smoltcp) whose sockets are host connections opened via
     the os-emulation host interface (needs a small socket extension to os-emulation, or
     drive a host fd through the existing read/write). This keeps the device identical
     across native/WASI/browser — only the backend (the os-emulation host) differs.
  3. **Browser**: WebSocket/WebTransport uplink to a host-side relay that owns the
     native bridge (the PLAN_LINUX_WASM §9 note), through the non-local-host seam.
- Test: S-V3 (guest reaches a host endpoint).

## 7. Workstream E — machine snapshots (derived)

- A `MachineSnapshot` capturing: guest RAM (sparse, swap-aware — reuse the `vane-swap`
  page enumeration), hart state (pc, regs, CSRs, priv, satp), device state (UART/PLIC/
  ACLINT + each virtio device), and accelerator state (trace cache keys / compiled-
  registry pcs — *not* the callables, which are re-derivable).
- **Derived snapshot**: snapshot a *running* machine without halting the parent's lineage
  — the child starts from the parent's state and can diverge (COW disk from Workstream B
  keeps disk state per-snapshot). Chainable: snapshot → accelerate → run → snapshot → …
- Serialization: a versioned, checksummed format; the existing swap key file + `os-page`
  dump conventions inform the memory section.
- Test: S-W1 save→restore bit-identical.

## 8. Workstream F — Weval acceleration (wrapped, structure-preserving)

- Pipeline: take a `MachineSnapshot` + the vane interpreter wasm → run **Weval** to
  specialize the interpreter against the snapshot's hot state (constant-fold boot-time
  config, dead-code-eliminate cold paths) → a faster specialized module.
- **Wrapper** (`vane-weval`, or a tool crate): runs Weval on the *code* but **re-attaches
  all structural data** — guest-RAM image, device/config data segments, the compiled-
  registry table, exports — that Weval's reachability would otherwise strip. The output
  is a complete, runnable machine, not a husk.
- The specialized module registers its hot traces into the **compiled-trace registry**
  (the `TraceFn` seam), so the specialized code is what the dispatch loop runs.
- Test: S-W2 (identical output, faster, structure intact).

## 9. Workstream G — custom binary patcher (never strips)

- A custom wasm binary patcher (the "Wizened"-style backend) that patches specialized
  code **in place** — replacing trace dispatch entries / inserting specialized sections —
  **without removing any structural data**: it is purely additive or in-place, so by
  construction no data segment, global, or export is dropped (contrast with Weval, which
  needs the wrapper).
- Same output contract as Workstream F: patched hot traces register into the compiled-
  trace registry. The patcher is the right tool when the structural data must be
  provably untouched (S-W3's byte-level audit) or when re-attaching after Weval is
  awkward.
- Test: S-W3 (byte-level structure preservation) + correctness parity.

## 10. Workstream H — multi-pass acceleration over derived snapshots

- Compose E–G: boot → snapshot S0 → accelerate (Weval-wrapped or patcher) → run to a
  later checkpoint → snapshot S1 (derived from the accelerated machine) → accelerate
  again → … Each pass specializes against the *current* hot state, so later passes can
  fold state that only became concrete after earlier passes ran.
- Invariants: every pass preserves observable behavior (S-W1/S-W2/S-W3 correctness); the
  snapshot chain is inspectable (each derived snapshot names its parent); acceleration is
  idempotent-safe (re-accelerating an already-accelerated module is a no-op or strictly
  better).
- Tooling: a driver that walks a snapshot chain and applies N passes, verifying output
  parity after each.
- Test: S-W4 (≥2 passes, composed, correct).

---

## 11. Milestones

| # | Milestone | Deliverables | Acceptance | Status |
|---|---|---|---|---|---|
| V1 | virtio-mmio framework | `vane-riscv::virtio`: QueueMem, legacy-v1 virtqueue, VirtioDevice, VirtioMmio; Machine slots + PLIC + DTB; process_virtio | Linux probes a stub device | **DONE** (3f2cd13) |
| V2 | virtio-blk | VirtioBlk + BlockBackend + CowBlock; vane-cli `--disk` + FileBlock; ext2 image builder + disk-rootfs gate (6046707) | S-V1 (mount rootfs) | **DONE** (device + DTB validated); the Linux-mount E2E is blocked on a kernel-config finding: the Debian netboot kernel has virtio_mmio/virtio_blk as *modules*, so `root=/dev/vda` needs a virtio-builtin kernel (see §12 R-V4) |
| V3 | virtio-9p | Virtio9p (9P2000.L subset) + Host9p trait; vane-cli `--share` + confined DirFs | S-V2 (two backends) | **DONE** (154beae); device-level 9P roundtrip tested |
| V4 | virtio-net | VirtioNet + NetBackend + RX chain deferral (t-baab, 901f9ec) | S-V3 | **DONE** (t-baab) |
| W1 | Snapshots | HartSnapshot/MachineSnapshot/ReactorSnapshot save/restore/fork (t-baab, 901f9ec) | S-W1 | **DONE** (t-baab) |
| W1.5 | Snapshot serialization | `snapshot_ser` (VSNP envelope) + encode/decode + snapshot_to/from_bytes (b84043f) | round-trip + checksum | **DONE** |
| W2 | Weval | vane-patch `weval_specialize` + `reattach_stripped_data` (c0cd138) | S-W2 | **infra DONE**; real Weval tool network-gated. Snapshot embedding is Wizer's job (Wizer is a library now), not this crate's. |
| W3 | Custom patcher | vane-patch: `compile_trace`/`compile_trace_pack`/`compile_trace_module` recompile StackOp traces → WASM, placed in the dispatch table's slots; `VaneWasmJit` is an os-jit-core `JitBackend` (bytes→trace→WASM artifact spliced into the wasmi table); `audit_no_stripping` (ff66ff1, ac21cf4) | S-W3 | **DONE** (offline, walrus + wasm-encoder) |
| W4 | Multi-pass | vane-patch `multi_pass` snapshot-chain driver + per-pass audit (e3baf0b) | S-W4 | **DONE** |

Ordering: V1→V2/V3/V4 (devices are independent after the framework); W1 is independent
and can start anytime; W2/W3 need W1 (snapshots) and the existing registry; W4 needs
W1+W2+W3.

## 12. Testing strategy

- Extend the machine gate harness with virtio variants (blk rootfs, 9p mount, net echo)
  alongside the existing UART/payload gates.
- os-emulation backend matrix for 9p (native tunneled + WASI + filtered).
- Snapshot round-trip tests (S-W1) as a unit + an end-to-end resume.
- Acceleration parity: run the ALU / swap-stress payloads under a weval'd and a patched
  module, assert byte-identical output to the reference engine, and record speedups.
- Structure-preservation audit for the patcher (S-W3): diff data segments/globals/exports
  input vs output.

## 13. Risks

| R | Risk | Impact | Mitigation |
|---|------|--------|-----------|
| R-V1 | virtio-mmio legacy/modern mismatch with the guest kernel | device not probed | implement both register sets; test against the actual Debian kernel; fall back to legacy v1 (container2wasm-proven) |
| R-V2 | 9P2000.L corner cases (locking, xattr, mmap) | partial fs | scope to the Linux 9p client's common op set first; differential-test against a host dir |
| R-V3 | Native L2 bridge needs root; smoltcp adds a dependency | net unusable w/o setup | wizard-guided TUN/TAP setup; smoltcp optional feature; browser relay for the no-root case |
| R-W1 | Snapshot misses a piece of state (device, hart, swap) | resume diverges | S-W1 bit-identity test; checklist of every stateful component; version the format |
| R-W2 | Weval strips structural data | specialized module won't run | the wrapper re-attaches data; S-W3-style audit on the weval path too |
| R-W3 | Multi-pass compounds an acceleration bug | late, hard-to-find divergence | per-pass output parity check in the W4 driver; idempotency test |

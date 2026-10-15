# Plan: Linux on WASM — no-JIT execution, WASI/WASIX targets, encrypted paging, RISC-V privileged mode

Status: **DRAFT for review**
Scope: this repo (`vane`) and the sibling decoder crate `rv-utils` (`rv-asm`).
Ultimate goal: **boot mainline RISC-V Linux inside a browser tab and inside WASI/WASIX runtimes, run real userspace programs, with an 8 GiB guest RAM of which ~4 GiB is paged out to an encrypted backing store.**

---

## 1. Goals, non-goals, success criteria

### 1.1 Goals

1. **No-JIT execution** — vane must run correctly and fast in environments where *all* runtime code generation is forbidden (`eval`, `new Function`, `WebAssembly.compile`, `WebAssembly.instantiate(buffer)`): Cloudflare Workers, Deno Deploy, pages with a CSP that omits `'wasm-unsafe-eval'`, and JIT-less embedded WASM runtimes (e.g. WAMR interpreter mode).
2. **WASI + WASIX targets** — vane must build and run as a self-driving program under `wasm32-wasip1`, and under WASIX (`wasm32-wasmer-wasi` via `cargo wasix`) where threads/sockets/process support are needed, in addition to the existing `wasm32-unknown-unknown` + wasm-bindgen browser target.
3. **Encrypted paging of guest RAM to disk** — guest physical RAM (8 GiB) lives in a paged store; cold pages are evicted to an encrypted backing file (~4 GiB resident cap, ~4 GiB+ on "disk"). Confidentiality *and* integrity of swapped pages, on every host target (browser OPFS, WASI filesystem, native).
4. **RISC-V privileged architecture** — full M/S/U privilege levels, Sv39 (Sv48 optional) virtual memory, traps and interrupts, so that OpenSBI + mainline Linux `riscv64` boots.

### 1.2 Non-goals (for this plan)

- Multi-hart SMP. Single hart first; SMP is explicitly deferred (WASIX threads / `SharedArrayBuffer` make it *possible* later — see §9).
- Vector (V) extension, hypervisor (H) extension, AIA, IOMMU.
- Networking for the guest (virtio-net) in phase 1; the plan notes where it slots in.
- Being cycle-accurate or matching any specific silicon.

### 1.3 Success criteria (acceptance tests)

| # | Criterion | How verified |
|---|-----------|--------------|
| S1 | Full `riscv-tests` + `riscv-arch-test` (RV64, `p` and `v` variants) pass on the interpreter alone, with `new Function` unavailable (shimmed to throw) | CI job with codegen disabled |
| S2 | `cargo build --target wasm32-wasip1` and `cargo wasix build` produce a CLI that boots a Linux kernel under wasmtime and wasmer | CI + local runbook |
| S3 | Linux reaches a busybox shell in a headless browser (Chrome/Firefox; Safari noted as stretch) with UART output on an xterm.js console | Browser E2E (Playwright) |
| S4 | A stress workload (e.g. `stress-ng --vm` or a known allocator benchmark) runs on an 8 GiB guest while the resident set is capped at 4 GiB; ≥ 4 GiB of pages cycle through the encrypted backing store; correctness verified by the workload's own checksums | Scripted benchmark in `harness/` |
| S5 | Swapped pages on disk are ciphertext: tampering with any byte of the backing file is detected (AEAD tag failure) and reported as a guest-impacting error, not silent corruption | Fault-injection test |
| S6 | Booting the same kernel under `wasmer` (WASIX) works and can use a WASIX thread for the timer/IO loop | CI job |

---

## 2. Current state (ground truth)

### 2.1 vane (this repo)

- `crates/vane` — cdylib; wasm-bindgen `Reactor` with `interp` (async, tree-walking over decoded `rv_asm::Inst`), `jit_code`/`jit_run` (generates JS source, executes via `new Function`). RV64IMAC + F/D described as RV64GC.
- `crates/vane-arch` — `Mem` (`BTreeMap<u64, Box<[u8; 65536]>>` — 64 KiB pages, allocated on demand, inside the WASM host's address space), `PagingMode` (Legacy/Shared/Both), `TemplateJit`, `CoreJS` (JS prologue with inline `data(addr)` translation), and — critically for workstream 1 — **`interp.rs`: a native `StackOp` interpreter ("Mode C")** with sync `StackHost` and `AsyncStackHost` traits. This already exists and is the seed of the no-JIT path.
- `crates/vane-target-core` — `StackOp` IR (stack machine: control flow, reg file, i32/i64 arith, float, memory, `CheckCode`, `Ecall`, `Trap`, `TailCall`).
- `crates/vane-target-wasm` — **stub**: `WasmJit`/`WasmJitCtx` traits, `render_wasm.rs` emits WASM via `wasm-encoder`, no end-to-end WASM JIT yet.
- `crates/vane-riscv` — per-instruction JS codegen (`RiscvDisplay`), HINT detection.
- `crates/vane-aarch64` — second frontend (disarm64), proves the multi-arch shape.
- `crates/vane-cross-fuzz` — differential fuzzing vs Unicorn (native-only, excluded from workspace). **Unicorn has no privileged mode** — privileged-mode differential testing needs a different oracle (Spike/riscv-isa-sim or rvvm).
- Testing today: `cargo test` for pure-Rust crates, `wasm-pack test --node`, Node driver in `tools/` for ELF execution, corpus tests in `crates/vane/_tests/`.

### 2.2 rv-utils (`rv-asm` decoder)

- RV32/RV64 `Xlen` support; I/M/A (Zalrsc+Zaamo)/C/Zihintpause/Zicsr/F/D decode+encode+display; `no_std`, no alloc; `Inst::decode` guaranteed non-panicking (exhaustive 32-bit test).
- **Has**: `Ecall`, `Ebreak`, all six CSR ops, `Csr` type.
- **Missing for Linux**: `Mret`, `Sret`, `Wfi`, `Sfence.Vma` (SFENCE.VMA is mandatory for any Sv39 kernel), plus the privileged CSR enumeration (`mstatus`, `misa`, `mie`, `mtvec`, `mscratch`, `mepc`, `mcause`, `mtval`, `mip`, `medeleg`, `mideleg`, `mcounteren`, `pmpcfg*`, `pmpaddr*`, `sstatus`, `sie`, `stvec`, `sscratch`, `sepc`, `scause`, `stval`, `sip`, `satp`, `scounteren`, `menvcfg`, …). Also needs a final RV64-only instruction audit (LD/LWU/SD, *W ops, compressed RV64 forms) — partial support exists today.

### 2.3 External facts established during research

- **No-JIT environments**: Cloudflare Workers throw `EvalError: Code generation from strings disallowed` for `eval`/`new Function` and `CompileError: Wasm code generation disallowed by embedder` for `WebAssembly.compile*`/`instantiate(buffer)` *inside request handlers*; only pre-compiled WASM modules are allowed. CSP without `'wasm-unsafe-eval'` blocks WASM compilation in pages. ⇒ The only portable no-JIT engine is a **pure interpreter**; any runtime codegen (JS *or* WASM) must be an optional acceleration layer.
- **WASI**: `wasm32-wasip1` (preview1) is stable Tier 2 in rustc and is what wasmtime/wasmer/wazero/WAMR all run; `wasm32-wasip2` (component model) is newer and has thinner tooling. **Target preview1 first**, wrap for p2 later if needed.
- **WASIX**: preview1 + POSIX extensions — threads (pthreads), sockets, fork/exec, pipes, TTY, DNS, asyncify-based longjmp. Rust support via `cargo install cargo-wasix` + `cargo wasix build` (target `wasm32-wasmer-wasi`); runtime = wasmer (native and in-browser via `@wasmer/wasi`). Best current fit for "real programs" that expect threads/subprocesses.
- **Browser memory**: a `wasm32` linear memory caps at 4 GiB. `memory64` is shipped in Chrome/Edge/Firefox but **not Safari** (as of early 2026 caniuse data). Per-tab caps vary by browser/OS and are well below 8 GiB in practice on 32-bit-pointer builds. ⇒ Guest RAM pages must live **outside the main linear memory** (host-side `ArrayBuffer` pool in browser, host heap in WASI runtimes) — which vane's `Mem` page-map design already structurally supports.
- **Browser storage**: OPFS `createSyncAccessHandle()` gives synchronous, in-place read/write on a file **but only inside a dedicated Worker**; it is the only browser API fast enough for a swap device. IndexedDB is the async fallback. Storage is subject to origin quota — must request `navigator.storage.persist()`.
- **Reference proof this goal is reachable**: `container2wasm` boots riscv64 Linux (TinyEMU compiled to WASI) in browsers and WASI runtimes today, mounting host dirs into the guest via virtio-9p. Our differentiators: no runtime codegen required, encrypted RAM paging, and a Rust-native codebase.
- **Linux boot contract (docs.kernel.org/arch/riscv/boot.html)**: kernel entered in S-mode (or M-mode) with `a0` = hartid, `a1` = physical address of the DTB, `satp` = 0 (MMU off), kernel image 2 MiB-aligned. Practical path: emulate the QEMU `virt` machine — ns16550a UART, ACLINT (mtime/mtimecmp + msip), PLIC, virtio-mmio transports, FDT — with **OpenSBI** (`fw_jump`) as the M-mode firmware. This gives us a well-specified, well-tested target that mainline kernel configs boot on unmodified.

---

## 3. Architecture overview (target state)

```
┌────────────────────────────── host frontends ──────────────────────────────┐
│ browser (wasm-bindgen) │ WASI CLI (wasip1) │ WASIX (wasmer) │ native tests │
└───────────┬──────────────────────┬───────────────────┬─────────────────────┘
            │                      │                   │
┌───────────▼──────────────────────▼───────────────────▼─────────────────────┐
│ vane-arch (core, no_std+alloc)                                             │
│  • Hart: regs, CSR file, priv mode, trap engine, Sv39/48 MMU + TLB         │
│  • Devices: UART16550, ACLINT, PLIC, virtio-mmio (blk/console/rng)         │
│  • Execution engines (pluggable):                                          │
│      E0 StackOp interpreter   (always available — no-JIT baseline)         │
│      E1 JS tracing JIT        (feature `jit-js`, needs new Function)       │
│      E2 WASM template JIT     (feature `jit-wasm`, needs WebAssembly.*)    │
│  • Mem: page cache (resident set) ◄── SwapStore trait ──► encrypted disk   │
└────────────────────────────────────────────────────────────────────────────┘
        ▲                                        ▲
   rv-asm decoder (RV64GC + privileged)     SwapStore impls:
                                             OPFS(worker)/IndexedDB/WASI fs/native fs
```

Key design rule: **all engine, storage, and host-IO dependencies enter through traits**, so `no-JIT`, `browser`, `WASI`, `WASIX`, and `native` are just different assemblies of the same core. The core must stay `no_std` + `alloc` (it already nearly is: `libm`, `spin`, `either` are all compatible).

---

## 4. Workstream 1 — No-JIT execution

### 4.1 Problem

Today the *good* performance path is `jit_run` → `new Function(...)`. In no-JIT environments that throws, leaving only the naive per-instruction `interp` loop. The WASM JIT (`vane-target-wasm`) can't save us: it needs `WebAssembly.compile`, equally banned.

### 4.2 Plan

1. **Promote the `StackOp` interpreter to the primary engine.**
   - `vane-arch/src/interp.rs` already implements sync + async interpretation of `StackOp` traces with host-pluggable state. Make `Reactor` able to run a trace cache of `Vec<StackOp>` directly, keyed by guest PC, with `CheckCode` self-validation exactly like the JS path (encoding guard at trace head).
   - Decode/lower pipeline is shared with codegen (`rv_emit` in `vane-riscv`), so JIT and interpreter can never disagree about semantics *by construction* — the same `StackOp` stream feeds both.
2. **Trace/block dispatch, not per-instruction dispatch.** Interpret whole basic blocks (or traces up to a branch/back-edge budget) per dispatch, mirroring the JIT's granularity. This is where most of the win over `interp` comes from.
3. **Interpreter fast paths** (ordered by expected payoff):
   - Pre-decode cache keyed by physical page + PC offset, invalidated on page write (self-modifying code via the existing `CheckCode` discipline — the store path must poison affected traces; today only JIT roots are deleted, this must be unified).
   - Register file in a fixed `[u64; 33]` + state slots in a flat struct, no hashing.
   - Compute `StateSlot` reads/writes as direct field accesses where the slot is statically known.
   - Optional "superinstruction" fusion pass over `StackOp` streams (e.g. LoadReg+Add64+StoreReg) behind a feature flag; measure before keeping.
4. **Runtime engine selection & enforcement.**
   - `Engine` enum on `Reactor`: `Interp` (default in no-JIT builds), `JsJit`, `WasmJit` (when implemented). Compile-time features `jit-js`/`jit-wasm` default **on** for the browser package, **off** for `nojit` builds and for WASI/WASIX (no JS host there anyway).
   - CI job that runs the test suite with `new Function` and `WebAssembly.compile` shimmed to throw — proves no hidden dependency (S1).
5. **Keep the JITs as first-class engines — on the same code path as the interpreter (decision D5).** The JS JIT (and later the WASM JIT) are *not* optional extras to be starved of maintenance: they stay in the tree and keep improving. The hard rule is **one shared pipeline**: every engine consumes the *same* `StackOp` streams produced by the same decode/lower pass (`rv_emit`), and any `CheckCode`/trace-miss fallback goes to the interpreter (today's `delete $.p[root]; return J(pc)` shape). Tests must exercise the *same traces* through interpreter and JIT alike and require identical architectural outcomes (§4.3). A phase-2 idea that becomes legitimate under this rule: compile trace templates to WASM at *build* time for hosts like Cloudflare Workers that accept only precompiled modules.

### 4.3 Deliverables

- `Reactor::run_nojs()` (name TBD) — async trace-interpreter loop with the same exit semantics as `interp`.
- Unified trace cache + invalidation shared by all engines.
- `nojit` build profile + CI gate.
- **Engine-parity test suite (D5):** for every test trace/program in the corpus, run it through the StackOp interpreter *and* every enabled JIT engine, then compare full architectural state (regs, memory digest, exit reason). Runs in CI with JITs both enabled and disabled, so JIT and no-JIT paths can never silently diverge. This suite doubles as the JIT regression suite going forward.
- Benchmarks: interpreter vs JS JIT on the existing corpus (target: within 3–5× of JIT on kernel-ish integer code; FP paths via `libm` unchanged).

---

## 5. Workstream 2 — WASI & WASIX targets

### 5.1 Problem

`crates/vane` is a wasm-bindgen cdylib; its public API returns `JsValue` futures and only exists in a JS host (documented in TESTING.md). Nothing builds for `wasm32-wasip1` today: `wasm-bindgen`, `js-sys`, `web-sys`, `getrandom`-via-JS are all in the dependency cone.

### 5.2 Plan

1. **Host abstraction layer** (new crate `vane-host`, `no_std`+alloc):
   - `HostIo` trait: UART in/out (byte streams), monotonic clock (for ACLINT `mtime`), wall clock (for virtio-rng seed and `rtc`), block-device backing = the `SwapStore` from §6, plus a `yield_now().await` / timer-callback hook (needed because the interpreter loop must not starve the WASI event loop — and browser builds need it for OPFS fallback paths).
   - `Exit`/`PowerOff` reporting (Linux `reboot`/`poweroff` via SBI SRST).
2. **Feature-gate the wasm-bindgen surface.** Everything in `crates/vane` that mentions `wasm_bindgen`/`JsValue` moves behind `--feature js-host` (on by default only for the browser package build). The core `Reactor`/Hart API becomes plain Rust. **Decision D1 (resolved):** extend `vane-meta-gen` so the macro emits *both* surfaces — the wasm-bindgen impls and a plain-Rust twin — behind cfg gates, rather than retiring the macro or handwritting a second glue layer.
3. **New crate `vane-cli`** (bin target, std allowed):
   - Args: `--kernel Image --opensbi fw_jump.bin --initrd … --disk rootfs.img --mem 8G --resident 4G --swap swap.vane --swap-key <file|env> --append "console=ttyS0 …"`.
   - Compiles for: `x86_64/aarch64` native (dev/test), `wasm32-wasip1` (wasmtime/wasmer/wazero), `wasm32-wasmer-wasi` (WASIX via `cargo wasix`).
   - Host IO impls: stdio UART, WASI clocks, preopened-dir files for disk/swap. **No wasm-bindgen anywhere.**
4. **WASIX-specific extras** (feature `wasix`): spawn the timer tick + UART pump + swap flusher on WASIX threads instead of cooperative yields; later, guest sockets via a virtio-net + WASIX sockets bridge. WASIX's asyncify/longjmp support is *not* needed by our design (we never block inside the interpreter).
5. **Browser remains first-class**: the existing wasm-bindgen package keeps working, gains the new engines/devices, and drives OPFS/IndexedDB swap backends (§6). Browser and WASI must boot the *same* kernel image — that sameness is a core acceptance property.
6. **Versioning/CI**: add `wasm32-wasip1` and (toolchain permitting) `cargo wasix check` to CI. Note WASIX requires its own rust toolchain download (`cargo wasix download-toolchain`); gate that job so a toolchain outage doesn't block unrelated PRs.

### 5.3 Deliverables

- `vane-host` trait crate; `vane-cli` booting a stub payload under wasmtime in CI.
- `--no-default-features --features wasi` build of the core with zero `js-sys` in the tree (`cargo tree` assertion in CI).
- Runbook in TESTING.md: build, run under wasmtime/wasmer, expected serial output.

---

## 6. Workstream 3 — Encrypted paging of guest RAM to disk

### 6.1 Problem / constraints

- Guest RAM target: 8 GiB = 131 072 pages × 64 KiB. Resident cap: 4 GiB. ⇒ up to ~4 GiB must live in an encrypted backing store.
- Browser: pages can't live in the 4 GiB `wasm32` linear memory; they must move to host-side `ArrayBuffer`s (browser) or host heap (WASI runtimes). `Mem`'s page-map design already isolates this behind `get_page` — good.
- Swap must be **encrypted and authenticated** (threat model: backing store = browser origin storage or host file the guest shouldn't trust; also a confidentiality feature for multi-tenant hosts).
- WebCrypto is async-only and can't be called from inside a synchronous interpreter step; OPFS sync handles are worker-only. ⇒ **The paging layer's fault path must be async**, or must use OPFS sync handles from the worker the emulator already runs in. Our interpreter is already async (`AsyncStackHost`, `interp` is async) — the fault path is allowed to `.await`.

### 6.2 Design

1. **Page store split.**
   - `PageCache` (resident set): fixed-capacity pool of 64 KiB frames. Browser: `ArrayBuffer` arena (or `SharedArrayBuffer` when COOP/COEP allow, prepping for SMP); WASI/native: heap arena. The `data(addr)` fast path in JS JIT'ed code must tolerate faults: on a miss, bail to the host (`return J(pc)`-style) which services the fault asynchronously and re-enters. Interpreter path faults inline with `.await`.
   - `SwapStore` trait (async): `read(page_no, &mut [u8])`, `write(page_no, &[u8])`, `capacity`, plus optional `trim`/`sync`. Implementations: `OpfsSwap` (sync access handle, dedicated worker), `IdbSwap` (IndexedDB, one object store, key = page number, batched transactions — slower, fallback only), `FileSwap` (WASI preopened file / native file; sparse, direct I/O where available).
2. **Eviction policy.** Start with clock/second-chance over the resident pool with dirty tracking (a page written since load is written back on evict; clean pages are dropped if still present in swap). Keep an explicitly **pinned** set: kernel image, DTB, OpenSBI, active Sv39 page-table pages (the walker must not fault recursively — see §7.4, this interacts with the MMU). LRU-K or W-TinyLFU is a v2 refinement; measure with the §S4 workload first.
3. **Crypto format.** **Decision D2 (resolved): XChaCha20-Poly1305.**
   - Cipher: **XChaCha20-Poly1305** via the `chacha20poly1305` crate (pure Rust, `no_std`, constant-time-friendly in software, identical code path on *every* target — browser, WASI, WASIX, native, no-JIT). There is deliberately **no WebCrypto fast path** (WebCrypto has no XChaCha20): one implementation everywhere keeps the swap code path identical across hosts and removes a whole class of per-target divergence (same spirit as D5). ChaCha20 is also fast in software WASM and does not depend on host AES hardware.
   - Per-page record on disk: `[ nonce (24 B) | ciphertext (65536 B) | tag (16 B) ]` ⇒ 65 576 B per page; backing file sized `pages × 65576` (4 GiB of pages ⇒ ~4.001 GiB file).
   - **Nonce discipline**: the 192-bit nonce is drawn fresh at random from the host CSPRNG **on every page write**. XChaCha20's nonce is large enough that random nonces are collision-safe for the lifetime of a key, so there is *no generation counter to persist, journal, or get wrong* — this is the main reason for choosing it over GCM. The nonce travels with the page record (see layout above).
   - **Key management**: 256-bit key generated per VM instance from host CSPRNG (`getrandom` / `crypto.getRandomValues`), optionally derived from a user-supplied key file via HKDF (CLI flag). Key never touches disk; hibernation/persistence of *encrypted* pages across sessions is allowed only with a user-supplied key (a fresh random key per session trivially invalidates old swap — by design).
   - Integrity failure on read ⇒ hard VM error (S5). No silent zeros.
   - Optional (default ON, benchmark-gated): `lz4`/`zstd` compression before encryption; zero-page detection is mandatory (all-zero pages are never written to swap, re-materialized on fault — huge win for fresh 8 GiB guests).
4. **Write batching & readahead.** Group dirty-page writebacks (e.g. 64 pages / 4 MiB per flush); speculative readahead of ±N pages on fault is a tunable, default small (browser OPFS) to medium (WASI file).
5. **Accounting & control.** `Reactor` API: `mem_stats()` → resident/faults/evictions/writeback bytes/crypto overhead; configurable resident cap. These feed the §S4 report and browser UI.

### 6.3 Deliverables

- `PageCache` + `SwapStore` + crypto layer (`vane-swap` crate, `no_std` core logic).
- Three backends (OPFS, IndexedDB, File) behind features; fault-injection tests (bitflip in file ⇒ Poly1305 tag error, S5).
- `stress` harness: 8 GiB guest, 4 GiB resident, workload drives ≥ 4 GiB through swap; correctness + throughput numbers recorded (S4).
- Doc: `PAGING_SWAP.md` with the on-disk format and the random-nonce/key-lifetime invariant.

---

## 7. Workstream 4 — RISC-V privileged architecture (+ rv-utils)

Target platform: **QEMU `virt`-compatible**, single hart, RV64GC, OpenSBI `fw_jump` in M-mode, mainline `riscv64` Linux in S-mode.

### 7.1 rv-utils (decoder) work

1. Add privileged SYSTEM instructions: `Mret`, `Sret`, `Wfi`, `Sfence.Vma` (decode/encode/display), with correct funct7/rs2 gating.
2. Expand the `Csr` type to the full privileged set used by OpenSBI+Linux (list in §2.2), including PMP CSRs (`pmpcfg0..15`, `pmpaddr0..63`) even if the emulator implements PMP minimally.
3. RV64 audit: all `*W` op forms, `LD/LWU/SD`, compressed RV64 loads/stores/shifts, `Addw` family — verify against the spec with the existing exhaustive-test machinery extended to these forms.
4. Keep the no-panic and `no_std` guarantees; regenerate exhaustive roundtrip tests through clang (existing method).

### 7.2 vane core: privilege & traps

1. **Hart state**: current privilege mode (M/S/U), `mstatus` (with `MPP`/`SPP`/`MPIE`/`MIE` and the `sstatus` view mask), delegation (`medeleg`, `mideleg`), trap vectors (`mtvec`/`stvec`, both direct and vectored), exception CSRs (`mepc`/`mcause`/`mtval`, S-mode twins), interrupt pending/enable (`mip`/`mie`, `sip`/`sie`, write masks per spec), `mscratch`/`sscratch`, `mcounteren`/`scounteren`, `menvcfg` (minimal), `misa` (fixed RV64GC+S+U), `mhartid` = 0, cycle/instret counters.
2. **Trap engine**: synchronous exceptions (illegal instr, misaligned, ecall-from-U/S/M, breakpoint, load/store/instruction page-fault & access-fault) with correct priority, delegation, `*epc/*cause/*tval` fill, and xRET semantics. Asynchronous interrupts: `MSIP/MTIP/MEIP` and `SSIP/STIP/SEIP` with delegation; WFI wake on locally-enabled pending interrupts.
3. **Instruction semantics**: all CSR ops honor privilege + read-only-CSR illegal-instruction rules; `sfence.vma` flushes TLB (and is a no-op trap-or-fence when `TVM=1`); `mret`/`sret` privilege restore per spec; `wfi` implemented as wait-for-interrupt hint.
4. **PMP**: minimal-but-correct: implement the CSRs, default to "all of memory accessible" with a matching entry; enforce on M-mode data accesses *only if* configured (OpenSBI programs PMP — mostly to carve its own region — so enforcement must at least not break the default OpenSBI config; full enforcement can follow once booting).

### 7.3 Sv39 MMU (+ optional Sv48)

1. Page-table walker for Sv39 (3-level, 4 KiB pages, megapages at L2/L1), PTE bit semantics (V/R/W/X/U/G/A/D, `PBMT`/`N` ignored), `mxr`/`sum` handling, access-fault vs page-fault distinction, A/D updates (choose: hardware A/D update in the walker — what QEMU does and what Linux expects).
2. TLB: keyed by `(asid, vpn)` when we implement ASIDs, flushed on `sfence.vma` (full or per-ASID/addr) and on `satp` writes; start with a simple direct-mapped or small fully-associative map + generation counter.
3. **Critical interaction with §6**: the walker reads guest-physical memory that may be swapped out ⇒ walker faults must be serviceable (async) *without* the TLB/MMU recursing into itself. Rule: **Sv39 page-table pages and the swap metadata are pinned** (never evicted); walker uses a side-channel raw-physical-read that asserts pinned. This is the single most important cross-workstream invariant.
4. Sv48 behind a runtime flag once Sv39 boots (kernel picks per DTB/ACPI — we advertise Sv39 first).

### 7.4 Devices (QEMU `virt` clone, virtio-mmio)

| Device | Model | Notes |
|---|---|---|
| UART | ns16550a @ `0x1000_0000` | console I/O; THRE/RX interrupts; host bridges to stdio / xterm.js |
| Timer+SWI | ACLINT (`mtime`/`mtimecmp` @ `0x200_0000`, `mswi` @ `0x2F0_0000`) | modern replacement for CLINT; `mtime` driven by host monotonic clock with a configurable freq (10 MHz) |
| Interrupt ctrl | PLIC @ `0xC00_0000` | priority/threshold/claim-complete; sources: UART(10), virtio(1..8) |
| Block | virtio-mmio @ `0x1000_1000+` | rootfs/disk image; **must implement the modern (v1+) and/or legacy v1 interface Linux's virtio-mmio driver expects — start with what TinyEMU/container2wasm use (legacy v1 mmio) since it's simpler and mainline still supports it** |
| Console | virtio-console (optional; UART suffices for phase 1) | — |
| RNG | virtio-rng | needed early by modern kernels for entropy/KASLR seed |
| Syscon | SiFive test finisher @ `0x10_0000` | poweroff/reboot hook (SBI SRST lands here via OpenSBI) |
| 9p (stretch) | virtio-9p | host-dir sharing, mirroring container2wasm's UX |

Device framework: memory-mapped dispatch in `Mem` on reserved physical ranges → `Device` trait (`read/write/tick/irq_line`), IRQ lines aggregated by the PLIC, which raises `MEIP/SEIP` into the trap engine. A device `tick()` runs at trace-loop boundaries (this is also where WASIX threads / browser timers can pump async IO).

### 7.5 Firmware & boot flow

1. **DTB generator** (Rust, compile-time template + runtime patch of memory size, bootargs, initrd range): `/cpus`, `memory@80000000`, ACLINT, PLIC, UART, virtio-mmio nodes, `chosen { bootargs, linux,initrd-* }`. Emit a real `.dtb` (write a tiny FDT serializer; do **not** shell out to `dtc` at runtime).
2. **Reset state**: PC = `0x1000` (ROM jump) or directly `0x8000_0000` with OpenSBI loaded at DRAM base and kernel at `+2 MiB`; `a0=0`, `a1=<dtb paddr>`, `satp=0`, M-mode. This matches the documented Linux boot contract.
3. **OpenSBI**: use stock `fw_jump.bin` (riscv64). This is why full M/S + delegation correctness matters — OpenSBI exercises `medeleg`/`mideleg`, PMP, `menvcfg`, SBI TIME/IPI/RFENCE/HSM/SRST. We implement no SBI ourselves initially; everything goes through `ecall` → M-mode → OpenSBI. (Fallback plan if OpenSBI surprises us: a minimal in-tree M-mode stub implementing SBI v2.0 BASE/TIME/RFENCE/SRST — keep as risk mitigation R4.)
4. **Kernel**: mainline `riscv64` defconfig + virtio drivers + `initramfs` (buildroot/busybox) for the first boot milestone; virtio-blk rootfs (Alpine or Debian riscv64) for "real programs".

### 7.6 Testing strategy for privileged mode

- **Unit**: CSR access tables, trap delegation matrix, Sv39 walker against hand-built page tables (incl. fault cases, A/D, MXR/SUM).
- **isa tests**: `riscv-tests` `rv64u[imafd]c-{p,v}` and `riscv-arch-test` (RISCOF) — the `-v` variants run under virtual memory and are the first real Sv39 exercise.
- **Differential (decision D3 — deferred):** Unicorn stays as the *user-mode-only* oracle exactly as today. **Do not build Spike/rvvm integration up front** — add a privileged-capable differential oracle *when needed*, i.e. when a bug escapes the unit/isa/boot tests above or when M3's failure rate justifies it. Until then, M3 correctness rests on the CSR/trap/MMU unit tests, `riscv-tests -p`/`-v`, RISCOF, and the boot milestones. When the time comes, first candidate is Spike via `--log-commits` parsing (no API-stability risk); cosim interface only if throughput demands it.
- **Boot milestones as tests** (each becomes a CI E2E):
  - B1: OpenSBI banner over UART.
  - B2: Linux early console (`earlycon=sbi`) — decompression + `setup_vm` (this exercises Sv39 hard).
  - B3: initramfs busybox shell; `cat /proc/cpuinfo`.
  - B4: virtio-blk rootfs; run `python3` or `gcc -o /dev/null` tier "real program".
  - B5: S4 stress with swap pressure; mem cgroup limit inside guest to force guest swapping on top of ours (torture case).
  - B6: same image in browser (S3) and WASIX (S6).

### 7.7 Deliverables

- rv-utils release with privileged ISA (used via the existing git dependency; coordinate versioning — it's our own repo).
- `vane-arch`: `Hart` (privilege+CSR+trap+MMU), `devices/*`, `dtb` module.
- Kernel/OpenSBI binaries vendored via `harness/` fetch script (pinned hashes), built reproducibly (Docker cross toolchain per existing harness conventions).

---

## 8. Memory budget & performance model (the 8 GiB story)

- Guest physical: 8 GiB ⇒ 131 072 pages. Resident cap: 4 GiB ⇒ 65 536 frames. Swap file: up to 4 GiB+ of page records.
- Interpreter cost model: page fault (swap-in) ≈ OPFS read + XChaCha20-Poly1305 decrypt of 64 KiB ≈ tens of µs on desktop (pure-software ChaCha20 is fast and identical on all hosts — no AES-NI dependence); a Linux boot touches ~1–2 GiB ⇒ minutes-scale boot in browser is expected for phase 1; WASI/native with real files should be much faster. Numbers to be replaced by §6.3 measurements.
- The JS JIT (where allowed) must degrade gracefully under swap pressure: fault-in-JIT → host re-entry path (§6.2.1) is on the hot path; benchmark kernel build-ish workload with JIT on/off.
- Zero-page fast path + kernel/initramfs pinning keeps worst-case thrash (early boot touches everything once) tolerable.

---

## 9. Deferral hooks (not in this plan, but designed-for)

- **SMP**: `Hart` is already (will be) a struct; ACLINT MSWI and PLIC contexts are per-hart shaped; `SharedArrayBuffer` resident pool + WASIX threads are the two carrier mechanisms. Atomic extension semantics are per-hart correct (LR/SC reservation model must become reservation-per-hart when SMP lands).
- **virtio-net**: WASIX sockets or browser WebSocket/WebTransport bridge.
- **WASI p2 component**: wrap `vane-cli` with `wasm-component` tooling once wasip2 runtimes are as ubiquitous as p1.
- **Hibernation/migration**: the encrypted swap format + key file already is a snapshot format for paged-out state; needs resident-set + hart-state serialization.

---

## 10. Milestones (dependency-ordered)

| M | Name | Contents | Exit gate |
|---|------|----------|-----------|
| M0 | Core hygiene | Feature-gate bindgen (`--features js-host`), `no_std` audit, `vane-host` traits, CI build matrix (browser/wasip1/native) | wasip1 "hello UART" stub runs under wasmtime |
| M1 | No-JIT engine | §4 complete: StackOp trace interpreter as primary, unified trace cache, no-JIT CI gate | S1 green; interp-vs-JIT benchmark recorded |
| M2 | Decoder | §7.1 in rv-utils; exhaustive tests green | rv-asm release pinned in Cargo.toml |
| M3 | Privilege core | §7.2 + §7.3: CSRs, traps, Sv39, TLB; riscv-tests `-p`/`-v` green; Spike differential green | M3 test suite green |
| M4 | Devices & boot | §7.4–7.5: UART/ACLINT/PLIC/virtio-mmio-blk/rng, DTB, OpenSBI→Linux | B1–B3 in CI |
| M5 | Swap | §6: PageCache/SwapStore/crypto + File backend (WASI/native first) | S4 + S5 on native & wasmtime |
| M6 | Browser productize | OPFS backend, xterm.js demo page, storage.persist, browser E2E | S3 + S4 in browser |
| M7 | WASIX productize | cargo-wasix build, threaded IO loop, wasmer CI | S6 |

(M3 depends on M1 only lightly; M2→M3 is the hard dependency. M5 can start after M0 and run parallel to M3/M4 with a stub Mem. Suggested staffing: one thread M2→M3→M4, one thread M0→M1→M5→M6/M7.)

---

## 11. Risk register

| R | Risk | Impact | Mitigation |
|---|------|--------|-----------|
| R1 | Interpreter too slow for an 8 GiB Linux boot to be tolerable in-browser | Boot takes many minutes; demo value drops | §4.3 fast paths; JIT fallback where allowed; pre-warmed swap images (ship a post-boot snapshot — see §9 hibernation hook) |
| R2 | MMU↔swap recursion bugs (walker faults) | Deadlock/corruption | Hard invariant: pinned page-table pages + separate raw-physical read path; assertion tests in M3 |
| R3 | Swap-crypto misuse (nonce collision, key reuse across sessions) | Total crypto break | XChaCha20 random 192-bit nonces make collision negligible by construction (D2 — no generation counter to mishandle); fresh random key per session unless user explicitly supplies one; tamper/bitflip tests (S5) |
| R4 | OpenSBI uses a CSR/PMP corner we stubbed wrong | Boot hangs pre-console | Fallback: minimal in-tree SBI stub; implement PMP enforcement properly early; if a bug escapes testing, that is the trigger for the deferred Spike differential oracle (D3) |
| R5 | OPFS quota/eviction under browser storage pressure | Swap write fails mid-boot | `storage.persist()`, quota preflight (`estimate()`), graceful "shrink guest RAM" degradation path |
| R6 | WASIX toolchain churn (out-of-tree rustc fork) | WASIX CI flaky | Gate WASIX job as non-blocking; pin `cargo wasix` toolchain version; wasip1 remains the guaranteed tier |
| R7 | Safari (no memory64, OPFS quirks) | Browser matrix gap | Pages live in JS ArrayBuffers anyway (no memory64 need); Safari = stretch goal in S3, tested explicitly in M6 |
| R8 | virtio-mmio legacy vs modern interface mismatch with mainline driver | Block device never probes | Mirror TinyEMU's proven v1 mmio first; add modern v1 status/queue fields second; test against unmodified mainline defconfig |

---

## 12. Decisions (resolved 2026-10-15)

- **D1 — `vane-meta-gen` fate → (a): extend the macro.** `vane-meta-gen` will emit both surfaces — the wasm-bindgen impls and a plain-Rust twin — behind cfg gates. The macro stays the single source of truth for the `Reactor`/`Core` API; no handwritten duplicate glue, no browser-only lock-in. (Applies at §5.2.)
- **D2 — Swap crypto primitive → XChaCha20-Poly1305.** Chosen over AES-256-GCM: 192-bit random nonces eliminate the generation-counter machinery entirely, it's constant-time-friendly in pure-Rust software, and it's identical on every target (no WebCrypto/AES-NI fast paths — deliberately). `chacha20poly1305` crate, `no_std`. (Applies at §6.2.)
- **D3 — Privileged differential oracle → Spike, when needed.** No up-front Spike/rvvm integration. Unicorn remains user-mode-only; privileged correctness rests on unit tests + `riscv-tests -p`/`-v` + RISCOF + boot milestones. A Spike oracle (first via `--log-commits` parsing) is built only when a bug escapes those nets. (Applies at §7.6, R4.)
- **D4 — Resident-set location in browser → host-side arena; never assume memory64.** The resident page pool lives in JS `ArrayBuffer`s in the browser (host heap under WASI/WASIX/native). No design element may require memory64 — it is absent in Safari and unnecessary given `Mem`'s page-map architecture. (Applies at §6.1, §8.)
- **D5 — JITs stay, on the interpreter's code path.** The JS JIT (and future WASM JIT) remain first-class and keep improving, but every engine consumes the same `StackOp` streams from the same decode/lower pipeline, and CI runs the *same traces* through interpreter and JITs with full-state comparison (§4.3 engine-parity suite). Build-time-precompiled WASM trace modules for Workers-style hosts are a legitimate phase-2 under this rule.

---

## 13. Appendix: key references

- Linux RISC-V boot requirements: `docs.kernel.org/arch/riscv/boot.html` (a0=hartid, a1=dtb, satp=0, 2 MiB alignment)
- QEMU `virt` machine: `qemu.org/docs/master/system/riscv/virt.html` (ACLINT option, device map)
- WASIX: `wasix.org/docs`, `docs.wasmer.io/runtime/wasix` (`cargo wasix build`, target `wasm32-wasmer-wasi`, import namespace `wasix_32v1`)
- WASI targets: rustc book `wasm32-wasip1`/`wasm32-wasip2` pages
- Cloudflare no-codegen rules: `developers.cloudflare.com/workers/runtime-apis/web-standards/` (eval/new Function/WebAssembly.compile*/instantiate(buffer) disallowed)
- CSP: MDN CSP `wasm-unsafe-eval`
- OPFS sync access handles: Chrome Intent-to-Ship (worker-only sync variant); opfs.dev benchmark comparisons vs IndexedDB
- memory64 support matrix: caniuse `wasm-memory64` (Chrome/Edge/Firefox yes, Safari no)
- container2wasm (prior art: TinyEMU riscv64 → WASI/browser, virtio-9p host dirs): `github.com/container2wasm/container2wasm`
- Repo docs: `README.md` (architecture), `PAGING.md`/`PAGING_API.md` (current Mem/JIT paging), `TESTING.md` (host split), `CROSS_FUZZ_PLAN.md` (differential harness)

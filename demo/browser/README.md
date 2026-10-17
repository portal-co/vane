# vane browser demo (M6)

Boots RISC-V Linux (OpenSBI + kernel + busybox initramfs) in a dedicated Web
Worker, with guest RAM paged out to an **encrypted OPFS backing store** when
the resident cap is below the RAM size. This is the M6 deliverable of
[PLAN_LINUX_WASM.md](../../PLAN_LINUX_WASM.md) §6 (workstream 3) and §8.

## Architecture

```
main thread (index.html + main.js)
  • console UI (a <pre>; swap in xterm.js for a real terminal)
  • navigator.storage.persist()
  • relays keystrokes -> worker, UART output <- worker
        │ postMessage
        ▼
dedicated worker (worker.js)
  • loads pkg/vane_web.js (wasm-bindgen, no-JS-JIT interp engine)
  • opens OPFS file, gets a *sync access handle* (worker-only API)
  • WebMachine::boot() → OpenSBI → Linux, machine mode + Sv39
  • encrypted swap: vane-swap PageCache ↔ OPFS via a sync JS store object
```

The OPFS sync access handle is only available inside a dedicated worker, which
is exactly where the emulator runs — so swap I/O is synchronous and the
interpreter's fault path stays synchronous.

## Build

```sh
# 1. Build the wasm package (needs wasm-pack; offline works — deps cached).
(cd crates/vane-web && wasm-pack build --target web)

# 2. Stage the demo: copy the pkg and the boot artifacts next to the page.
demo/browser/stage.sh
```

`stage.sh` copies `crates/vane-web/pkg/*` into `demo/browser/pkg/` and the
boot artifacts (`opensbi.bin`, `linux`, `initrd.gz`) from `harness/linux/`
(see `harness/linux/README.md` for how those are fetched).

## Run

```sh
cd demo/browser
python3 -m http.server 8000
# open http://localhost:8000
```

The page boots Linux; boot progress and a busybox shell appear on the console.
The stats line shows swap activity (resident frames, faults, evictions,
encrypted bytes written/read, integrity failures).

> **Note on speed**: the no-JIT interpreter runs at roughly a few hundred
> thousand instructions/sec, so a full Linux boot to a shell takes a long
> time. The trace engine (M4.5) + the planned sequence-fusion pass narrow this.
> For a quick smoke test, boot the much smaller `payload-swap-stress` instead
> of the full kernel.

## Browser support

- OPFS sync access handles: Chrome/Edge ≥ 108, Firefox ≥ 111, Safari ≥ 16.4
  (all inside a dedicated worker). The demo warns and disables swap if the
  API is absent.
- `wasm32-unknown-unknown` module: no JIT required (the interp engine never
  calls `new Function`), so it works under CSP without `wasm-unsafe-eval`.

## E2E (Playwright)

`demo/browser/e2e.spec.mjs` is a Playwright test that loads the page, waits
for the OpenSBI banner and a shell prompt, and asserts the swap stats show
encrypted traffic. It needs `npm i -D playwright` + `npx playwright install`
(network) and is run as a nightly/browser job, not per-commit CI.

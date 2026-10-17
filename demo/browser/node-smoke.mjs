// Offline smoke test for the M6 wasm surface (vane-web): runs the
// swap-stress guest through the *wasm* module with a JS-provided in-memory
// swap store (the same shape the OPFS bridge uses in the worker). Validates
// the full stack — WebMachine, loadElf, tick loop, UART drain, encrypted
// swap via the JS store — without a browser.
//
//   (cd crates/vane-web && wasm-pack build --target nodejs --out-dir pkg-node)
//   node demo/browser/node-smoke.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';

const here = fileURLToPath(new URL('.', import.meta.url));
const root = join(here, '..', '..');
const { WebMachine } = await import(join(root, 'crates/vane-web/pkg-node/vane_web.js'));

const payload = readFileSync(
  join(root, 'harness/payload-swap-stress/target/riscv64gc-unknown-none-elf/release/payload-swap-stress'),
);

// In-memory SwapStore with the exact shape the OPFS bridge uses:
//   slots() -> bigint, read(slot) -> Uint8Array|null, write(slot, rec) -> bool
function makeStore(slots) {
  const map = new Map();
  return {
    map,
    slots: () => slots,
    read: (slot) => {
      const r = map.get(Number(slot));
      return r ? new Uint8Array(r) : null;
    },
    write: (slot, record) => {
      map.set(Number(slot), new Uint8Array(record));
      return true;
    },
  };
}

const RAM = 32n * 1024n * 1024n; // 32 MiB guest (stress region is at 16-22 MiB)
const RESIDENT = 512n * 1024n;   // 512 KiB resident cap (8 frames) -> heavy swap
const slots = Number(RAM / 0x10000n);
const store = makeStore(slots);

const machine = new WebMachine();
machine.enableSwap(RESIDENT, RAM, store);
machine.loadElf(payload);

let out = '';
let done = false;
let code = -1;
let ticks = 0;
const MAX_TICKS = 200_000;
while (!done && ticks < MAX_TICKS) {
  const r = await machine.tick(20000n);
  out += r.out;
  if (r.swapError) {
    console.error('SWAP ERROR:', r.swapError);
    process.exit(1);
  }
  done = r.done;
  code = r.code;
  ticks++;
}

const stats = machine.swapStats();
console.log('ticks:', ticks, 'done:', done, 'code:', code);
console.log('swap stats:', JSON.stringify({
  resident: Number(stats.resident), cap: Number(stats.cap),
  faults: Number(stats.faults), evictions: Number(stats.evictions),
  writebacks: Number(stats.writebacks),
  swapMiBWritten: Number(stats.swapBytesWritten) / 1e6,
  swapMiBRead: Number(stats.swapBytesRead) / 1e6,
  zeroPages: Number(stats.zeroPages),
  integrityFailures: Number(stats.integrityFailures),
}));
console.log('guest output:', JSON.stringify(out.trim()));

if (!done || code !== 0) { console.error('FAIL: guest did not power off cleanly'); process.exit(1); }
if (!out.includes('S4:342551bdf2c401d6:OK')) { console.error('FAIL: expected S4 OK checksum'); process.exit(1); }
if (stats.evictions === 0n) { console.error('FAIL: no swap evictions'); process.exit(1); }
if (stats.swapBytesWritten === 0n) { console.error('FAIL: no encrypted writebacks'); process.exit(1); }
if (stats.integrityFailures !== 0n) { console.error('FAIL: integrity failures'); process.exit(1); }
console.log('vane-web Node smoke: PASS');

// vane browser demo — dedicated worker (M6).
//
// Runs the emulator (machine mode + OPFS-backed encrypted swap) off the main
// thread. The OPFS *sync access handle* is only available in a dedicated
// worker, which is exactly where the emulator runs.
//
// Protocol with the main thread (postMessage):
//   main -> worker: { type: 'boot', opensbi, kernel, initrd, append, ramSize, resident }
//   main -> worker: { type: 'input', data: string }        (console input)
//   worker -> main: { type: 'out', data: string }          (UART output)
//   worker -> main: { type: 'stats', stats }               (swap accounting)
//   worker -> main: { type: 'done', code }                 (guest poweroff)
//   worker -> main: { type: 'error', message }

import init, { WebMachine } from './pkg/vane_web.js';

const PAGE = 0x10000n;
const RECORD = 65576; // vane-swap RECORD_BYTES: nonce(24) + page(65536) + tag(16)

// Open (creating) an OPFS swap file and return the SwapStore object the
// wasm module calls synchronously.
async function openSwapStore(filename, slots) {
  const root = await navigator.storage.getDirectory();
  const fh = await root.getFileHandle(filename, { create: true });
  const handle = await fh.createSyncAccessHandle();
  handle.truncate(slots * RECORD);
  return {
    slots: () => slots,  // count fits in a Number
    read: (slot) => {
      const buf = new Uint8Array(RECORD);
      const n = handle.read(buf, { at: Number(slot) * RECORD });
      return n === RECORD ? buf : null;
    },
    write: (slot, record) => {
      const w = handle.write(record, { at: Number(slot) * RECORD });
      return w === RECORD;
    },
    // Periodically flush (durability); called from the tick loop.
    flush: () => handle.flush(),
    close: () => { handle.flush(); handle.close(); },
  };
}

let machine = null;

async function boot(msg) {
  await init();
  machine = new WebMachine();

  const ramSize = BigInt(msg.ramSize);
  const resident = BigInt(msg.resident);
  const slots = Number(ramSize / PAGE);

  // OPFS-backed encrypted swap (only when a resident cap smaller than RAM is
  // requested).
  let store = null;
  if (resident < ramSize) {
    store = await openSwapStore('vane-swap.bin', slots);
    machine.enableSwap(resident, ramSize, store);
  }

  machine.boot(
    new Uint8Array(msg.opensbi),
    new Uint8Array(msg.kernel),
    msg.initrd ? new Uint8Array(msg.initrd) : null,
    msg.append,
    ramSize,
  );

  postMessage({ type: 'out', data: '[vane-web] booted; resident ' + resident.toString() + ' of ' + ramSize.toString() + (store ? ' (OPFS swap)' : '') + '\n' });

  let ticks = 0;
  // Tick loop: run a bounded quantum, forward UART output, service input
  // messages (via the macrotask yield), check power/swap errors.
  for (;;) {
    const r = await machine.tick(50000n);
    if (r.out) postMessage({ type: 'out', data: r.out });
    if (r.swapError) {
      postMessage({ type: 'error', message: 'swap integrity/IO error: ' + r.swapError });
      break;
    }
    if (r.done) {
      postMessage({ type: 'done', code: r.code });
      break;
    }
    if (++ticks % 20 === 0) {
      const s = machine.swapStats();
      if (s) postMessage({ type: 'stats', stats: s });
      if (store) store.flush();
    }
    // Yield so queued 'input' messages are delivered.
    await new Promise((res) => setTimeout(res, 0));
  }
  if (store) store.close();
}

self.onmessage = (e) => {
  const m = e.data;
  if (m.type === 'boot') {
    boot(m).catch((err) => postMessage({ type: 'error', message: String(err && err.stack || err) }));
  } else if (m.type === 'input' && machine) {
    machine.uartInput(new TextEncoder().encode(m.data));
  }
};

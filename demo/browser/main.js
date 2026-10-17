// vane browser demo — main thread (M6). Owns the console UI and relays I/O
// to the dedicated worker that runs the emulator.
//
// Uses a plain <pre> console so the demo is fully self-contained (no npm /
// CDN). Swapping in xterm.js is a drop-in upgrade (see README).

const consoleEl = document.getElementById('console');
const statusEl = document.getElementById('status');
const statsEl = document.getElementById('stats');
const inputEl = document.getElementById('input');

function appendConsole(text) {
  consoleEl.textContent += text;
  consoleEl.scrollTop = consoleEl.scrollHeight;
}

// Ask the browser to persist origin storage so the OPFS swap file survives
// eviction under storage pressure (PLAN §6.1). Best-effort.
if (navigator.storage && navigator.storage.persist) {
  navigator.storage.persist().then((granted) => {
    statusEl.textContent = 'storage.persist: ' + (granted ? 'granted' : 'denied');
  });
}

async function fetchBytes(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error('fetch ' + url + ': ' + r.status);
  return r.arrayBuffer();
}

async function start() {
  // Boot artifacts served alongside the demo (see README for fetching).
  const [opensbi, kernel, initrd] = await Promise.all([
    fetchBytes('opensbi.bin'),
    fetchBytes('linux'),
    fetchBytes('initrd.gz'),
  ]);

  const worker = new Worker('worker.js', { type: 'module' });
  worker.onmessage = (e) => {
    const m = e.data;
    if (m.type === 'out') appendConsole(m.data);
    else if (m.type === 'stats') {
      const s = m.stats;
      statsEl.textContent =
        `swap: resident ${s.resident}/${s.cap} frames, ` +
        `${s.faults} faults, ${s.evictions} evictions, ` +
        `${(Number(s.swapBytesWritten) / 1e6).toFixed(1)} MiB written, ` +
        `${(Number(s.swapBytesRead) / 1e6).toFixed(1)} MiB read, ` +
        `${s.integrityFailures} integrity failures`;
    } else if (m.type === 'done') {
      appendConsole(`\n[guest powered off, code ${m.code}]\n`);
      statusEl.textContent = 'done';
    } else if (m.type === 'error') {
      appendConsole(`\n[error] ${m.message}\n`);
      statusEl.textContent = 'error';
    }
  };

  // Console input: send each line (or character) to the guest UART.
  inputEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      worker.postMessage({ type: 'input', data: inputEl.value + '\n' });
      inputEl.value = '';
    }
  });

  // RAM: 128 MiB guest, 8 MiB resident cap → the rest pages through OPFS.
  worker.postMessage({
    type: 'boot',
    opensbi, kernel, initrd,
    append: 'console=ttyS0 panic=-1',
    ramSize: 128 * 1024 * 1024,
    resident: 8 * 1024 * 1024,
  });
}

start().catch((e) => {
  appendConsole('[boot error] ' + (e && e.stack || e) + '\n');
  statusEl.textContent = 'boot error';
});

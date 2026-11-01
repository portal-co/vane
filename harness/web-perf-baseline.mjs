#!/usr/bin/env node
/**
 * vane-web perf baseline (t-1963 ask #1): the web-consumed paths.
 *   A. tick() throughput — payload-alu (register-only loop), no swap
 *   B. tick() throughput — same payload, swap enabled (memory store)
 *   C. swap store raw latency — memory store vs fs-backed store
 *   D. displayDrain() throughput — full-frame flushes
 *   E. swap under real traffic — payload-swap-stress, small resident cap
 *
 * Usage: node harness/web-perf-baseline.mjs
 */
import fs from 'fs';
import { createRequire } from 'module';
import { performance } from 'perf_hooks';
import path from 'path';
import { fileURLToPath } from 'url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, '..');
const require_ = createRequire(import.meta.url);
const vane = require_(path.join(root, 'crates/vane-web/pkg-node/vane_web.js'));

const PAYLOAD_ALU = path.join(root, 'harness/payload-alu/target/riscv64gc-unknown-none-elf/release/payload-alu');
const PAYLOAD_SWAP = path.join(root, 'harness/payload-swap-stress/target/riscv64gc-unknown-none-elf/release/payload-swap-stress');
const TICK_FUEL = 50_000n;
const RAM = 16 * 1024 * 1024; // 16 MiB guest RAM (256 swap slots)

const fmt = (n, d = 1) => n.toLocaleString('en-US', { maximumFractionDigits: d });
const mb = (b) => b / (1024 * 1024);

/** A JS swap store backed by a Map (pure overhead baseline). */
function memoryStore() {
  const slots = new Map();
  return {
    slots() { return 256; },
    read(slot) { return slots.get(Number(slot)) ?? null; },
    write(slot, bytes) { slots.set(Number(slot), bytes.slice()); return true; },
  };
}

/** A JS swap store backed by a sparse file (approximates a real device). */
function fsStore(file) {
  try { fs.unlinkSync(file); } catch {}
  const fd = fs.openSync(file, 'w+');
  return {
    slots() { return 256; },
    read(slot) {
      const stat = fs.fstatSync(fd);
      const off = Number(slot) * 65536;
      if (off >= stat.size) return null;
      const buf = Buffer.alloc(65536);
      const n = fs.readSync(fd, buf, 0, 65536, off);
      return n === 65536 ? new Uint8Array(buf) : null;
    },
    write(slot, bytes) {
      fs.writeSync(fd, bytes, 0, bytes.length, Number(slot) * 65536);
      return true;
    },
  };
}

async function runTicks(mach, label, per = 10_000) {
  const t0 = performance.now();
  let ticks = 0;
  for (;;) {
    const r = await mach.tick(BigInt(per));
    ticks++;
    if (r.done) {
      const t = (performance.now() - t0) / 1000;
      return { label, seconds: t, ticks, code: r.code, out: r.out };
    }
  }
}

async function benchTickThroughput() {
  console.log('── A. tick() throughput: payload-alu (register-only, 30M iterations), no swap');
  const mach = new vane.WebMachine();
  mach.loadElf(fs.readFileSync(PAYLOAD_ALU));
  const r = await runTicks(mach, 'alu');
  const iterPerSec = 30_000_000 / r.seconds;
  console.log(`   ${r.label}: ${fmt(r.seconds, 2)}s, ${r.ticks} ticks, exit=${r.code}, out="${r.out.trim()}"`);
  console.log(`   → ${fmt(iterPerSec / 1e6, 1)}M payload iterations/s (≈ ${fmt((iterPerSec * 10) / 1e6, 0)}M guest insns/s at ~10 insns/iter)`);
  return r.seconds;
}

async function benchTickWithSwap(resident, label, note) {
  console.log(`── B. tick() throughput: payload-alu + swap enabled (memory store, resident ${label})${note}`);
  const mach = new vane.WebMachine();
  mach.enableSwap(BigInt(resident), BigInt(RAM), memoryStore());
  mach.loadElf(fs.readFileSync(PAYLOAD_ALU));
  const r = await runTicks(mach, label);
  const iterPerSec = 30_000_000 / r.seconds;
  console.log(`   ${r.label}: ${fmt(r.seconds, 2)}s, ${r.ticks} ticks, exit=${r.code}, out="${r.out.trim()}"`);
  console.log(`   → ${fmt(iterPerSec / 1e6, 1)}M payload iterations/s`);
  return { seconds: r.seconds, ticks: r.ticks };
}

function benchStoreRaw() {
  console.log('── C. swap store raw latency (one 64 KiB record read+write, 256 rounds)');
  for (const [name, store] of [['memory', memoryStore()], ['fs-sparse', fsStore('/tmp/vane-bench-swap.bin')]]) {
    const bytes = new Uint8Array(65536).fill(0xAB);
    // Warm (allocate).
    store.write(0n, bytes);
    const t0 = performance.now();
    const ROUNDS = 256;
    for (let i = 0; i < ROUNDS; i++) {
      const slot = BigInt(i);
      const got = store.read(slot);
      if (got) store.write(slot, got);
      else store.write(slot, bytes);
    }
    const t = (performance.now() - t0) / 1000;
    console.log(`   ${name}: ${fmt(t * 1e6 / ROUNDS, 2)} µs/roundtrip → ${fmt(ROUNDS / t, 0)} roundtrips/s`);
  }
  fs.unlinkSync('/tmp/vane-bench-swap.bin');
}

async function benchDisplayDrain() {
  console.log('── D. displayDrain() throughput (full-frame 1280x720 RGBA flushes)');
  const mach = new vane.WebMachine();
  mach.displayTestPattern();
  const bytes = 1280 * 720 * 4;
  const ROUNDS = 100;
  // Warm.
  mach.displayDrain();
  mach.displayTestPattern();
  const t0 = performance.now();
  for (let i = 0; i < ROUNDS; i++) {
    mach.displayDrain();
    mach.displayTestPattern();
  }
  const t = (performance.now() - t0) / 1000;
  const total = (bytes * ROUNDS);
  console.log(`   ${ROUNDS} pattern+drain cycles: ${fmt(t, 2)}s → ${fmt(mb(total) / t, 0)} MB/s drained (${fmt(total / ROUNDS / 1024)} KiB/frame)`);
}

async function benchSwapStress() {
  console.log('── E. swap under real traffic: payload-swap-stress (6 MiB region, 1 MiB resident)');
  const mach = new vane.WebMachine();
  mach.enableSwap(0x100_000n, BigInt(RAM), memoryStore());
  mach.loadElf(fs.readFileSync(PAYLOAD_SWAP));
  const r = await runTicks(mach, 'swap-stress');
  console.log(`   ${r.label}: ${fmt(r.seconds, 2)}s, ${r.ticks} ticks, exit=${r.code}, out="${r.out.trim()}"`);
}

(async () => {
  console.log(`vane-web perf baseline — pkg-node ${(new Date(fs.statSync(path.join(root, 'crates/vane-web/pkg-node/vane_web_bg.wasm')).mtimeMs)).toISOString()}`);
  const tA = await benchTickThroughput();
  const fair = await benchTickWithSwap(0x100_000, '1 MiB resident', ' — working set fits');
  const thrash = await benchTickWithSwap(0x1_0000, '64 KiB resident', ' — PATHOLOGICAL: evicts the code page');
  console.log(`   swap-enabled overhead (fair): ${fmt((fair.seconds / tA - 1) * 100, 1)}%`);
  console.log(`   swap-thrash (code page eviction): ${fmt(thrash.ticks / Math.max(1, 1), 0)} ticks vs 11 — constant trace re-emission`);
  benchStoreRaw();
  await benchDisplayDrain();
  await benchSwapStress();
})().catch((e) => {
  console.error('BENCH FAILED:', e);
  process.exit(1);
});

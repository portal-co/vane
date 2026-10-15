#!/usr/bin/env node
// M1 no-JIT gate (PLAN_LINUX_WASM.md §4, S1): prove vane runs guest code
// with *no runtime code generation available*, and that the JIT engine
// produces byte-identical observable behavior when it IS available (D5
// engine parity at the E2E level).
//
// Usage: node harness/nojit-gate.mjs <path-to-payload-elf>
//
// Expects the wasm-pack nodejs pkg at crates/vane/pkg (the driver script
// builds it).
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const allowCodegen = argv.includes('--allow-codegen');
const elfPath = argv.find((a) => !a.startsWith('--'));
if (!elfPath) {
  console.error('usage: node harness/nojit-gate.mjs [--allow-codegen] <payload.elf>');
  process.exit(2);
}

// --- Disable runtime code generation before anything loads. --------------
// Mirrors Cloudflare Workers' in-request behavior: string codegen throws
// EvalError, runtime WASM compilation throws CompileError. Skipped with
// --allow-codegen (the reference run the driver compares against).
if (!allowCodegen) {
  const forbidden = () => {
    throw new EvalError('Code generation from strings disallowed for this context');
  };
  globalThis.Function = class ForbiddenFunction {
    constructor() { forbidden(); }
  };
  globalThis.eval = forbidden;
  WebAssembly.compile = () => Promise.reject(new Error('Wasm code generation disallowed by embedder'));
  WebAssembly.compileStreaming = WebAssembly.compile;
}

// (WebAssembly.instantiate with bytes stays allowed: this Node loader needs
// it to load vane itself. Workers-style hosts accept *precompiled* modules,
// which vane already is — the gate's subject is vane's GUEST execution.)

// --- Load the pkg ---------------------------------------------------------
const pkgDir = path.join(here, '..', 'crates', 'vane', 'pkg');
const jsFile = (await import('node:fs')).readdirSync(pkgDir).find((f) => f.endsWith('.js'));
const pkg = await import(path.join(pkgDir, jsFile));
if (typeof pkg.default === 'function') await pkg.default();

// Minimal ELF64 loader (matches tools/vane-tester-wasm.ts).
function parseELF64(buf) {
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const u16 = (o) => dv.getUint16(o, true);
  const u32 = (o) => dv.getUint32(o, true);
  const u64 = (o) => BigInt(u32(o)) | (BigInt(u32(o + 4)) << 32n);
  if (u32(0) !== 0x464c457f) throw new Error('not an ELF');
  const entry = u64(24), phoff = Number(u64(32)), phentsize = u16(54), phnum = u16(56);
  const loads = [];
  for (let i = 0; i < phnum; i++) {
    const ph = phoff + i * phentsize;
    if (u32(ph) !== 1) continue;
    loads.push({ offset: Number(u64(ph + 8)), vaddr: u64(ph + 16), filesz: Number(u64(ph + 32)) });
  }
  return { entry, loads };
}

const elfBytes = new Uint8Array(readFileSync(elfPath));
const elf = parseELF64(elfBytes);

// Run the payload once. output is captured, exit code returned.
async function runOnce(engine) {
  const reactor = new pkg.Reactor();
  for (const seg of elf.loads) {
    for (let i = 0; i < seg.filesz; i++) {
      reactor.write_byte(seg.vaddr + BigInt(i), elfBytes[seg.offset + i]);
    }
  }
  let output = '';
  reactor.set_host_api({
    async write(fd, view) {
      if (fd === 1 || fd === 2) output += Buffer.from(view).toString('utf8');
      return view.length;
    },
    async read() { return 0; },
    async exit() { return 0; },
  });
  reactor.set_engine(engine);
  const outcome = await reactor.run(elf.entry);
  const code = outcome?.kind === 'exit' ? Number(outcome.code) : null;
  return { output, code };
}

const EXPECTED = 'hello from vane';
const results = {};
for (const engine of ['interp', 'jit']) {
  results[engine] = await runOnce(engine);
  if (!results[engine].output.includes(EXPECTED) || results[engine].code !== 0) {
    console.error(`FAIL: engine=${engine} allowCodegen=${allowCodegen}:`, JSON.stringify(results[engine]));
    process.exit(1);
  }
  console.log(`engine=${engine} allowCodegen=${allowCodegen} OK (exit=${results[engine].code}): ${results[engine].output.trim()}`);
}

// D5 engine parity: identical observable behavior across engines.
if (results.interp.output !== results.jit.output || results.interp.code !== results.jit.code) {
  console.error('FAIL: engine outputs diverge', JSON.stringify(results));
  process.exit(1);
}
console.log('engine parity OK');
process.exit(0);

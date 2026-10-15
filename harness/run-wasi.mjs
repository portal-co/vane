#!/usr/bin/env node
// Minimal WASI preview1 runner for vane-cli.wasm (M0 gate).
//
// Usage: node harness/run-wasi.mjs <module.wasm> [args...]
//
// The current working directory is preopened so the module can read payload
// paths passed via --elf. Used because wasmtime/wasmer are not installed on
// this machine; the gate is "runs as a WASI preview1 program", and Node's
// node:wasi is a conformant preview1 host for our purposes.
import { WASI } from 'node:wasi';
import { readFileSync } from 'node:fs';

const [,, modulePath, ...args] = process.argv;
if (!modulePath) {
  console.error('usage: node harness/run-wasi.mjs <module.wasm> [args...]');
  process.exit(2);
}

const wasi = new WASI({
  version: 'preview1',
  args: [modulePath, ...args],
  env: {},
  preopens: { '.': '.' },
  returnOnExit: true,
});

const bytes = readFileSync(modulePath);
const module = await WebAssembly.compile(bytes);
const instance = await WebAssembly.instantiate(module, wasi.getImportObject());
const code = wasi.start(instance);
process.exit(code ?? 0);

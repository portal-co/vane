#!/usr/bin/env node
/*
Node.js tester that uses the wasm-bindgen build of `vane` (expects `crates/vane/pkg` produced by `wasm-pack build --target nodejs`).

Features:
- Loads an ELF binary, parses PT_LOAD segments and writes them into the wasm `Mem` export.
- Exposes common flags (jit/interp, test_mode, paging, shared addresses, 32bit/multilevel paging).
- Robustly checks for expected wasm exports and prints helpful errors if they are missing.

Usage:
  node tools/vane-tester-wasm.js --input ./path/to/binary [--jit true|false] [--test_mode] [--paging legacy|shared|both]

Before using, build the wasm package from the `crates/vane` crate (from repo root):
  cd crates/vane
  wasm-pack build --target nodejs --out-dir pkg

This file is intentionally dependency-free and uses only Node core APIs.
*/

const fs = require('fs').promises;
const path = require('path');

function parseArgs(argv) {
  const out = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith('-')) {
      out._.push(a);
      continue;
    }
    const key = a.replace(/^-+/, '');
    const next = argv[i + 1];
    if (next === undefined || next.startsWith('-')) {
      out[key] = true;
    } else {
      out[key] = next;
      i++;
    }
  }
  return out;
}

// Minimal ELF (little-endian) PT_LOAD segment loader for 32/64-bit
function parseELF(buffer) {
  if (buffer.length < 16) throw new Error('File too small to be ELF');
  const EI_CLASS = buffer[4]; // 1=32-bit, 2=64-bit
  const EI_DATA = buffer[5]; // 1=little,2=big
  if (buffer[0] !== 0x7f || buffer[1] !== 0x45 || buffer[2] !== 0x4c || buffer[3] !== 0x46) {
    throw new Error('Not an ELF file');
  }
  if (EI_DATA !== 1) throw new Error('Only little-endian ELF supported by this loader');
  const is64 = EI_CLASS === 2;
  if (!is64 && EI_CLASS !== 1) throw new Error('Unknown ELF class');

  const dv = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  const readU32 = (off) => dv.getUint32(off, true);
  const readU16 = (off) => dv.getUint16(off, true);
  const readU64 = (off) => {
    // JavaScript cannot represent full 64-bit integer precisely, but RISC-V addresses generally fit into 53 bits here.
    const low = dv.getUint32(off, true);
    const high = dv.getUint32(off + 4, true);
    return high * 0x100000000 + low;
  };

  let e_phoff, e_phentsize, e_phnum, e_entry;
  if (is64) {
    e_entry = readU64(24);
    e_phoff = readU64(32);
    e_phentsize = readU16(54);
    e_phnum = readU16(56);
  } else {
    e_entry = readU32(24);
    e_phoff = readU32(28);
    e_phentsize = readU16(42);
    e_phnum = readU16(44);
  }

  const segments = [];
  for (let i = 0; i < e_phnum; i++) {
    const off = Number(e_phoff) + i * e_phentsize;
    if (is64) {
      const p_type = readU32(off);
      const p_flags = readU32(off + 4);
      const p_offset = readU64(off + 8);
      const p_vaddr = readU64(off + 16);
      const p_paddr = readU64(off + 24);
      const p_filesz = readU64(off + 32);
      const p_memsz = readU64(off + 40);
      const p_align = readU64(off + 48);
      segments.push({ p_type, p_offset, p_vaddr, p_filesz, p_memsz, p_flags, p_align });
    } else {
      const p_type = readU32(off);
      const p_offset = readU32(off + 4);
      const p_vaddr = readU32(off + 8);
      const p_paddr = readU32(off + 12);
      const p_filesz = readU32(off + 16);
      const p_memsz = readU32(off + 20);
      const p_flags = readU32(off + 24);
      const p_align = readU32(off + 28);
      segments.push({ p_type, p_offset, p_vaddr, p_filesz, p_memsz, p_flags, p_align });
    }
  }

  return { entry: Number(e_entry), segments };
}

async function loadWasmPkg(pkgPath) {
  // pkgPath is directory containing the wasm-bindgen `pkg` output (e.g. crates/vane/pkg)
  const jsFiles = await fs.readdir(pkgPath);
  const jsFile = jsFiles.find((f) => f.endsWith('.js'));
  if (!jsFile) throw new Error(`No .js wrapper found in ${pkgPath}. Did you run "wasm-pack build --target nodejs"?`);
  const jsPath = path.join(pkgPath, jsFile);
  // Use dynamic import to allow initialization promise
  const pkg = require(jsPath);
  // wasm-bindgen node target often exports an init function or returns a promise when required.
  // If pkg is a function, call it.
  if (typeof pkg === 'function') {
    // Some builds export an init function as default export
    await pkg();
    // Re-import after initialization
    // eslint-disable-next-line security/detect-non-literal-require
    return require(jsPath);
  }
  // If pkg.__wbindgen_start exists, it may auto-init on require; return pkg.
  return pkg;
}

async function main() {
  const argv = parseArgs(process.argv.slice(2));
  const input = argv.input || argv.i || (argv._ && argv._[0]);
  if (!input) {
    console.error('Missing --input <path> argument.');
    process.exit(2);
  }
  const useJit = (argv.jit === undefined) ? true : (argv.jit === 'true' || argv.jit === true);
  const test_mode = !!argv.test_mode || !!argv.testMode || !!argv.test;
  const paging = argv.paging || 'legacy';
  const shared_page_table_vaddr = argv.shared_page_table_vaddr ? Number(argv.shared_page_table_vaddr) : undefined;
  const shared_security_directory_vaddr = argv.shared_security_directory_vaddr ? Number(argv.shared_security_directory_vaddr) : undefined;
  const use_32bit_paging = !!argv.use_32bit_paging || !!argv.use32bitPaging;
  const use_multilevel_paging = !!argv.use_multilevel_paging || !!argv.useMultilevelPaging;

  // Locate pkg directory
  const pkgDir = path.join(__dirname, '..', 'crates', 'vane', 'pkg');
  if (!await exists(pkgDir)) {
    console.error(`Expected wasm pkg at ${pkgDir} - build it first:
  cd crates/vane
  wasm-pack build --target nodejs --out-dir pkg`);
    process.exit(3);
  }

  console.log('Loading wasm package from', pkgDir);
  const pkg = await loadWasmPkg(pkgDir);

  // Inspect exports
  const hasMemClass = !!pkg.Mem || !!pkg.mem || !!pkg.default?.Mem;
  const hasReactor = !!pkg.Reactor || !!pkg.reactor || !!pkg.default?.Reactor;
  if (!hasMemClass || !hasReactor) {
    console.error('The wasm package does not expose expected exports. Expected at least `Mem` and `Reactor` classes on the package exports.');
    console.error('Exports found:', Object.keys(pkg));
    process.exit(4);
  }

  const MemCtor = pkg.Mem || pkg.mem || (pkg.default && pkg.default.Mem);
  const ReactorCtor = pkg.Reactor || pkg.reactor || (pkg.default && pkg.default.Reactor);

  // Create memory (Mem) and load binary
  const bin = await fs.readFile(input);
  const elf = parseELF(bin);
  console.log(`ELF entry=${elf.entry.toString(16)} segments=${elf.segments.length}`);

  // instantiate Mem
  let mem;
  try {
    mem = new MemCtor();
  } catch (e) {
    // Some wasm-bindgen outputs export factory functions
    if (typeof MemCtor === 'function') {
      mem = MemCtor();
    } else {
      throw e;
    }
  }

  // Determine how to write into memory. Prefer `write_byte` style method.
  const writeByteFn = mem.write_byte || mem.writeByte || mem.write || mem.write_u8;
  const memIsBufferBacked = !writeByteFn && (pkg.memory || globalThis.memory || mem.memory || null);

  if (!writeByteFn && !memIsBufferBacked) {
    console.error('Unable to find a method to write bytes into `Mem` instance. Expected `write_byte` or similar or an exported memory.');
    process.exit(5);
  }

  // Helper to write bytes
  const writeByte = (addr, val) => {
    if (writeByteFn) {
      writeByteFn.call(mem, BigInt(addr), val);
    } else {
      // direct memory access
      const wasmMemory = pkg.memory || mem.memory || (pkg.default && pkg.default.memory);
      if (!wasmMemory) throw new Error('No wasm memory found');
      const U8 = new Uint8Array(wasmMemory.buffer);
      U8[addr] = val;
    }
  };

  for (const seg of elf.segments) {
    if (seg.p_type !== 1) continue; // PT_LOAD
    const off = Number(seg.p_offset);
    const fsize = Number(seg.p_filesz);
    const msize = Number(seg.p_memsz);
    const vaddr = Number(seg.p_vaddr);
    console.log(`Loading PT_LOAD: vaddr=0x${vaddr.toString(16)} filesz=${fsize} memsz=${msize} offset=${off}`);
    for (let i = 0; i < fsize; i++) {
      writeByte(vaddr + i, bin[off + i]);
    }
    for (let i = fsize; i < msize; i++) {
      writeByte(vaddr + i, 0);
    }
  }

  // Create Reactor via available constructors
  let reactor;
  try {
    // Try new_with_mem mem
    if (ReactorCtor.new_with_mem) {
      reactor = ReactorCtor.new_with_mem(mem);
    } else if (typeof ReactorCtor === 'function') {
      // Some wasm exports wrap constructors differently
      reactor = new ReactorCtor(mem);
    } else if (ReactorCtor.from_mem) {
      reactor = ReactorCtor.from_mem(mem);
    } else {
      // try calling no-arg constructor then set memory (less likely)
      reactor = new ReactorCtor();
      if (reactor.set_mem) reactor.set_mem(mem);
    }
  } catch (e) {
    console.error('Failed to construct Reactor instance:', e);
    console.error('Available Reactor props:', Object.keys(ReactorCtor));
    process.exit(6);
  }

  // Configure reactor flags if methods exist
  if (reactor.set_test_mode) try { reactor.set_test_mode(test_mode); } catch(e){}
  if (reactor.set_paging_mode) try { reactor.set_paging_mode(paging); } catch(e){}
  if (reactor.set_use_32bit_paging) try { reactor.set_use_32bit_paging(use_32bit_paging); } catch(e){}
  if (reactor.set_use_multilevel_paging) try { reactor.set_use_multilevel_paging(use_multilevel_paging); } catch(e){}
  if (reactor.set_shared_page_table_vaddr && shared_page_table_vaddr !== undefined) try { reactor.set_shared_page_table_vaddr(shared_page_table_vaddr); } catch(e){}
  if (reactor.set_shared_security_directory_vaddr && shared_security_directory_vaddr !== undefined) try { reactor.set_shared_security_directory_vaddr(shared_security_directory_vaddr); } catch(e){}

  // Run
  console.log('Starting execution at entry 0x' + elf.entry.toString(16));
  try {
    if (useJit && reactor.jit_run) {
      const res = reactor.jit_run(elf.entry);
      // wasm-bindgen methods may return promises
      if (res && typeof res.then === 'function') {
        await res;
      }
    } else if (reactor.interp) {
      const res = reactor.interp(elf.entry);
      if (res && typeof res.then === 'function') {
        await res;
      }
    } else {
      throw new Error('No interp or jit_run method found on Reactor');
    }
    console.log('Execution completed (OK)');
  } catch (e) {
    console.error('Execution failed:', e);
    // wasm-bindgen errors may be JS Error or JsValue; print generically
    process.exit(7);
  }
}

async function exists(p) {
  try { await fs.stat(p); return true; } catch { return false; }
}

main().catch((err) => {
  console.error('Fatal error:', err);
  process.exit(1);
});

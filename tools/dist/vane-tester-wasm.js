#!/usr/bin/env node
/* TypeScript version of the wasm-based tester. Expects wasm pkg in ../crates/vane/pkg

Usage: node tools/dist/vane-tester-wasm.js --input ./path/to/binary [--jit true|false] [--test_mode]
*/
import fs from 'fs/promises';
import path from 'path';
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
        }
        else {
            out[key] = next;
            i++;
        }
    }
    return out;
}
function parseELF(buffer) {
    if (buffer.length < 16)
        throw new Error('File too small to be ELF');
    const EI_CLASS = buffer[4];
    const EI_DATA = buffer[5];
    if (buffer[0] !== 0x7f || buffer[1] !== 0x45 || buffer[2] !== 0x4c || buffer[3] !== 0x46)
        throw new Error('Not an ELF file');
    if (EI_DATA !== 1)
        throw new Error('Only little-endian ELF supported');
    const is64 = EI_CLASS === 2;
    if (!is64 && EI_CLASS !== 1)
        throw new Error('Unknown ELF class');
    const dv = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    const readU32 = (off) => dv.getUint32(off, true);
    const readU16 = (off) => dv.getUint16(off, true);
    const readU64 = (off) => { const low = dv.getUint32(off, true); const high = dv.getUint32(off + 4, true); return high * 0x100000000 + low; };
    let e_phoff, e_phentsize, e_phnum, e_entry;
    if (is64) {
        e_entry = Number(readU64(24));
        e_phoff = Number(readU64(32));
        e_phentsize = readU16(54);
        e_phnum = readU16(56);
    }
    else {
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
            const p_offset = readU64(off + 8);
            const p_vaddr = readU64(off + 16);
            const p_filesz = readU64(off + 32);
            const p_memsz = readU64(off + 40);
            segments.push({ p_type, p_offset, p_vaddr, p_filesz, p_memsz });
        }
        else {
            const p_type = readU32(off);
            const p_offset = readU32(off + 4);
            const p_vaddr = readU32(off + 8);
            const p_filesz = readU32(off + 16);
            const p_memsz = readU32(off + 20);
            segments.push({ p_type, p_offset, p_vaddr, p_filesz, p_memsz });
        }
    }
    return { entry: e_entry, segments };
}
async function loadWasmPkg(pkgPath) {
    const dirents = await fs.readdir(pkgPath);
    const jsFile = dirents.find((f) => f.endsWith('.js'));
    if (!jsFile)
        throw new Error(`No .js wrapper found in ${pkgPath}. Run wasm-pack build --target nodejs`);
    const jsPath = path.join(pkgPath, jsFile);
    // Use dynamic ESM import for the generated pkg
    const imported = await import(jsPath);
    // wasm-pack node target commonly default-exports an init function or namespace; try to handle both
    if (typeof imported === 'function') {
        await imported();
        const reimport = await import(jsPath);
        return reimport;
    }
    // If default export is a function, call it
    if (imported && typeof imported.default === 'function') {
        await imported.default();
        const reimport = await import(jsPath);
        return reimport;
    }
    return imported;
}
async function exists(p) { try {
    await fs.stat(p);
    return true;
}
catch {
    return false;
} }
async function main() {
    const argv = parseArgs(process.argv.slice(2));
    const input = argv.input || argv.i || (argv._ && argv._[0]);
    if (!input) {
        console.error('Missing --input <path>');
        process.exit(2);
    }
    const useJit = (argv.jit === undefined) ? true : (argv.jit === 'true' || argv.jit === true);
    const test_mode = !!argv.test_mode || !!argv.testMode || !!argv.test;
    const paging = argv.paging || 'legacy';
    const shared_page_table_vaddr = argv.shared_page_table_vaddr ? Number(argv.shared_page_table_vaddr) : undefined;
    const shared_security_directory_vaddr = argv.shared_security_directory_vaddr ? Number(argv.shared_security_directory_vaddr) : undefined;
    const use_32bit_paging = !!argv.use_32bit_paging || !!argv.use32bitPaging;
    const use_multilevel_paging = !!argv.use_multilevel_paging || !!argv.useMultilevelPaging;
    const pkgDir = path.join(__dirname, '..', 'crates', 'vane', 'pkg');
    if (!await exists(pkgDir)) {
        console.error(`Expected wasm pkg at ${pkgDir} - build it first`);
        process.exit(3);
    }
    const pkg = await loadWasmPkg(pkgDir);
    const MemCtor = pkg.Mem || pkg.mem || pkg.default?.Mem;
    const ReactorCtor = pkg.Reactor || pkg.reactor || pkg.default?.Reactor;
    if (!MemCtor || !ReactorCtor) {
        console.error('pkg missing Mem/Reactor exports:', Object.keys(pkg));
        process.exit(4);
    }
    const bin = await fs.readFile(input);
    const buf = new Uint8Array(bin);
    const elf = parseELF(buf);
    console.log(`ELF entry=0x${elf.entry.toString(16)} segments=${elf.segments.length}`);
    // Instantiate Mem; wasm-bindgen may export a class or factory
    let mem;
    try {
        mem = new MemCtor();
    }
    catch {
        mem = MemCtor();
    }
    const writeByteFn = mem.write_byte || mem.writeByte || mem.write;
    const wasmMemory = pkg.memory || mem.memory;
    if (!writeByteFn && !wasmMemory) {
        console.error('No method to write into Mem instance found');
        process.exit(5);
    }
    const writeByte = (addr, val) => {
        if (writeByteFn)
            writeByteFn.call(mem, BigInt(addr), val);
        else {
            const memBuf = wasmMemory.buffer;
            if (!memBuf)
                throw new Error('WASM memory buffer not available');
            const U8 = new Uint8Array(memBuf);
            U8[addr] = val;
        }
    };
    for (const seg of elf.segments) {
        if (seg.p_type !== 1)
            continue;
        const off = Number(seg.p_offset);
        const fsize = Number(seg.p_filesz);
        const msize = Number(seg.p_memsz);
        const vaddr = Number(seg.p_vaddr);
        console.log(`Loading PT_LOAD vaddr=0x${vaddr.toString(16)} filesz=${fsize} memsz=${msize} off=${off}`);
        for (let i = 0; i < fsize; i++)
            writeByte(vaddr + i, buf[off + i]);
        for (let i = fsize; i < msize; i++)
            writeByte(vaddr + i, 0);
    }
    let reactor;
    try {
        if (typeof ReactorCtor.new_with_mem === 'function')
            reactor = ReactorCtor.new_with_mem(mem);
        else
            reactor = new ReactorCtor(mem);
    }
    catch (e) {
        try {
            reactor = ReactorCtor(mem);
        }
        catch (err) {
            console.error('Failed to instantiate Reactor:', err);
            process.exit(6);
        }
    }
    if (reactor.set_test_mode)
        try {
            reactor.set_test_mode(test_mode);
        }
        catch { }
    if (reactor.set_paging_mode)
        try {
            reactor.set_paging_mode(paging);
        }
        catch { }
    if (reactor.set_use_32bit_paging)
        try {
            reactor.set_use_32bit_paging(use_32bit_paging);
        }
        catch { }
    if (reactor.set_use_multilevel_paging)
        try {
            reactor.set_use_multilevel_paging(use_multilevel_paging);
        }
        catch { }
    if (reactor.set_shared_page_table_vaddr && shared_page_table_vaddr !== undefined)
        try {
            reactor.set_shared_page_table_vaddr(shared_page_table_vaddr);
        }
        catch { }
    if (reactor.set_shared_security_directory_vaddr && shared_security_directory_vaddr !== undefined)
        try {
            reactor.set_shared_security_directory_vaddr(shared_security_directory_vaddr);
        }
        catch { }
    console.log('Starting execution at entry 0x' + elf.entry.toString(16));
    try {
        if (useJit && reactor.jit_run) {
            const r = reactor.jit_run(elf.entry);
            if (r && typeof r.then === 'function')
                await r;
        }
        else if (reactor.interp) {
            const r = reactor.interp(elf.entry);
            if (r && typeof r.then === 'function')
                await r;
        }
        else
            throw new Error('No interp or jit_run on Reactor');
        console.log('Execution completed (OK)');
    }
    catch (e) {
        console.error('Execution failed:', e);
        process.exit(7);
    }
}
main().catch((e) => { console.error('Fatal', e); process.exit(1); });

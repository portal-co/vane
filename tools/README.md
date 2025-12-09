# Vane WASM Tester

This folder contains a TypeScript ESM tester that loads the `vane` wasm-bindgen package and runs RISC-V ELF binaries inside it.

Prerequisites
- Node.js (16+)
- npm
- Rust toolchain with the `wasm32-unknown-unknown` target
- `wasm-pack` installed (used to build the wasm package)

Build wasm package
From repo root:

```bash
cd crates/vane
wasm-pack build --target nodejs --out-dir pkg
```

Build TypeScript tester
From repo root:

```bash
npm install
npm run -w tools build
```

Run the tester

```bash
node tools/dist/vane-tester-wasm.js --input ./path/to/riscv.elf --jit true --test_mode --paging legacy
```

Flags
- `--input` (required): path to the ELF file to run
- `--jit` (true|false): run JIT (`true`) or interpreter (`false`) (default: `true`)
- `--test_mode`: enable test hints
- `--paging` (legacy|shared|both): paging mode (default: `legacy`)
- `--shared_page_table_vaddr`: optional numeric virtual address
- `--shared_security_directory_vaddr`: optional numeric virtual address
- `--use_32bit_paging` (flag)
- `--use_multilevel_paging` (flag)

Troubleshooting
- If the tester cannot find `crates/vane/pkg`, ensure you ran `wasm-pack build` in `crates/vane`.
- If TypeScript build fails, run `npm run -w tools build` to see errors.


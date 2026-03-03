# vane

## Description
"RISC-V emulator for the Web, WASM, and anything based on them". A JIT/emulator for RISC-V that compiles to `wasm32-unknown-unknown` and is driven from JavaScript/Node.js via `wasm-bindgen`.

## Goals
- [ ] Emulate RISC-V in WASM/Web
- [ ] Provide JIT compilation

## Progress
- [x] Workspace setup with JIT and emulation crates
- [x] WASM-in-JS tester (`tools/vane-tester-wasm.ts`)

## Testing

See [`TESTING.md`](TESTING.md) for the full guide.  The short version: build the WASM
package with `wasm-pack`, then use the Node.js tester in `tools/`.  A native CLI is
not provided — the public API is `wasm-bindgen`-generated and only meaningful inside a
WASM host.

---
*AI assisted*

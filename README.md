# vane

A RV64GC interpreter and JavaScript JIT compiler written in Rust, compiled to `wasm32-unknown-unknown` and exposed to JavaScript/Node.js via `wasm-bindgen`.

The primary execution model is a tracing JIT: for each program counter address, vane generates a JavaScript function string (via `j(pc)`), compiles it with `new Function(...)`, caches the result, and falls back to the interpreter (`interp`) on code generation failure. There is no native binary output; the "JIT" target is JavaScript source code that runs inside the same WASM host.

## What it actually does

- Interprets RV64IMAC instructions (the `interp` async method on `Reactor`) inside WebAssembly
- Compiles RV64IMAC instruction sequences to JavaScript source code strings at runtime (the `j` / `jit_code` method)
- Executes those generated JS strings via `new Function(...)` (the `jit_run` method)
- Manages a 64KB-page-based virtual memory system (`Mem`) with on-demand page allocation
- Exports the `Reactor` type via `wasm-bindgen` so JavaScript can create and drive emulator instances

The WASM JIT target (`vane-target-wasm`) exists as a stub with only a trait definition; it does not generate real WASM output yet.

## Crate structure

| Crate | Purpose |
|---|---|
| `vane` | Root cdylib. Wires together `vane-riscv` and `vane-meta-gen` to produce the `Reactor` WASM-bindgen type. Contains the interpreter loop (`interp`). |
| `vane-arch` | Architecture-agnostic infrastructure: `Mem` (paged memory), `JitCtx` trait, `TemplateJit` (JS codegen driver), `CoreJS` (JS wrapper/prologue generator), `PagingMode`, and the `renders!` / `cfg_target_js!` / `cfg_target_wasm!` macros. |
| `vane-riscv` | RV64 back-end. Implements the `RiscvDisplay` trait on `TemplateJit` to emit JavaScript for each RISC-V instruction. Also contains the `hint` module for detecting RISC-V HINT instructions. |
| `vane-aarch64` | Explicitly selected AArch64 frontend using `disarm64` 0.1.26. `Aarch64Reactor` executes StackOp traces against a caller-provided state host without changing the RV64 `Reactor` ABI. |
| `vane-target-js` | JS codegen helpers. Defines the `Flate` trait used to optionally minify identifier names in generated JS (`ReleaseFlate` substitutes short names like `f`, `g`, `s`, `u`, `d`; `DebugFlate` is a passthrough). |
| `vane-target-wasm` | Stub crate. Declares `WasmJit`, `WasmJitCtx`, and `JitOpcode` traits/types. The actual WASM JIT implementation is not yet written. |
| `vane-meta-gen` | Procedural-macro-like glue. Exports the `vane_meta!` declarative macro that generates the `Reactor` and `Core` structs along with all `wasm-bindgen` method impls (register access, page table control, `jit_code`, `jit_run`, paging mode setters, etc.). |

## Memory system

`Mem` uses a `BTreeMap<u64, Box<[u8; 65536]>>` (64KB pages keyed by page number) with three selectable paging modes:

- **Legacy** (default): pages are allocated on demand when `get_page` is called
- **Shared**: an explicit two-level page table stored at a known virtual address, compatible with the `rift`/`r52x`/`speet` family
- **Both**: the shared page table is stored inside the legacy virtual address space (nested paging)

Both single-level and three-level (L3/L2/L1) variants exist, in 32-bit and 64-bit entry sizes. The JS codegen in `CoreJS` emits an inline `data(addr)` function that performs the appropriate translation for the selected paging mode.

## JavaScript code generation

For each `pc`, `jit_code(pc)` returns a JS string of the form:

```
return async function() {
  let f=$.f, g=0xffff_ffffn, s=(a=>BigInt.asIntN(64,a)), u=(a=>BigInt.asUintN(64,a)),
  data=(p=>{p=$.get_page(p);return new DataView($._sys(`memory`).buffer,p);});
  <label>: for(;;) {
    const p=<pc>n;
    if(data(p).getUint32(0,true)!==<expected_encoding>){delete $.p[`<root>`];return J(p);}
    <instruction JS>;
    <next label or break>;
  }
}
```

The generated code:
- Reads registers from a plain JS object (`$.r`) via string keys (`x1`..`x31`; `x0` always reads `0n`)
- Uses JavaScript `BigInt` for all 64-bit arithmetic
- Validates the instruction encoding at the top of each block and invalidates the cache entry if the code has changed (self-modifying code support)
- Falls back to `interp` via `J(pc)` on any decode error or unsupported instruction

## Instruction coverage

The interpreter covers the full RV64IMAC base plus M extension: all integer arithmetic, shifts, branches, loads/stores (byte through doubleword), JAL/JALR, LUI/AUIPC, FENCE (as a no-op), ECALL, and all W-suffix 32-bit word variants. Unrecognised instructions return a JS error.

The JS codegen covers the same instruction set. `MULH`, `MULHU`, `MULHSU` use JS BigInt 128-bit shift. Divide-by-zero returns the RISC-V-specified sentinel value.

Floating-point (F/D extensions) and atomics (A extension) are not implemented.

The shared StackOp runtime now supports scalar raw-bit F32/F64 operations.
RV64 supports `FLW`/`FLD`, `FSW`/`FSD`, `FADD.{S,D}`, `FSUB.{S,D}`,
`FMUL.{S,D}`, `FDIV.{S,D}`, and `FSQRT.{S,D}` in its trace frontend.
AArch64 supports the matching initial `FLOATDP1`/`FLOATDP2` scalar subset
(`FADD`, `FSUB`, `FMUL`, `FDIV`, `FNMUL`, `FMIN`/`FMINNM`, `FMAX`/`FMAXNM`,
`FABS`, `FNEG`, `FSQRT`, and register `FMOV`) for S and D values. F32
operations round through the F32 representation after each operation.

AArch64 remains an opt-in library frontend, not a replacement for the current
WASM-bindgen RV64 reactor. Its scalar FP support also includes the documented
non-fused `FMADD` family policy, immediate moves, compares/selects, and the
basic `FMOV`/`SCVTF`/`UCVTF`/`FCVTZS`/`FCVTZU` integer boundary forms. Native
execution is covered by raw-word tests and the generated JavaScript is checked
with Node's `Function` parser; a browser execution corpus is still pending.
It still traps rounding-mode variants and scalar FP memory; Vane's WASM JIT
remains out of scope.

## Testing

Tests require `wasm-bindgen-test` and run in-browser only (configured via `wasm_bindgen_test_configure!(run_in_browser)`). The test suite (`crates/vane/_tests/rv_corpus.rs`) loads ELF binaries from the `rv-corpus` git submodule, maps PT_LOAD segments into `Mem`, and runs them through either the interpreter or JIT. A test passes when execution terminates via `ecall` with `a7=93` (the Linux `exit` syscall number), which vane treats as a success sentinel.

Current test cases:
- RV64I basic 64-bit operations (via both JIT and interpreter)
- RV64IM multiply/divide (via JIT)
- RV32I NOP and HINT instructions (with and without `test_mode` HINT logging)

To run tests:
```
# Build the WASM package first
wasm-pack build crates/vane

# Run in-browser tests
wasm-pack test --headless --firefox crates/vane
```

The submodule at `crates/vane/_tests/rv-corpus` must be checked out (`git submodule update --init`).

## Build requirements

- Rust toolchain with `wasm32-unknown-unknown` target
- `wasm-pack` (for building the cdylib and running tests)
- `wasm-bindgen-test-runner` (configured in `.cargo/config.toml` as the wasm test runner)

The workspace sets `opt-level = "s"` in the release profile.

## Configurable JIT host names

Vane can compile JIT output for a consistently property-mangled `$` runtime
object. Canonical code generation remains the default; use the documented
name-aware rendering APIs only when the host uses a complete alternate scheme.
See [`JIT_HOST_METHOD_NAMES.md`](JIT_HOST_METHOD_NAMES.md).

## Status

- Interpreter: implemented and tested for RV64IM
- JS JIT: implemented and tested for RV64IM
- WASM JIT: stub only, not functional
- Floating-point: scalar raw-bit F32/F64 runtime plus the initial RV64 and AArch64 trace subsets; conversions, fused operations, and most AArch64 FP families remain unsupported
- Atomics: not implemented
- System/privilege modes: not implemented; `ecall` handling is minimal (only syscall 93 is meaningful)

License: MPL-2.0

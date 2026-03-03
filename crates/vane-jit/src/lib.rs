//! `vane-jit` is now a re-export shim. All functionality has moved to:
//! - [`vane-target-js`] — JS codegen target (Flate trait)
//! - [`vane-target-wasm`] — WASM JIT target (WasmJit, JitOpcode)
//! - [`vane-arch`] — Emulated architecture infrastructure (Mem, TemplateJit, …)
//! - [`vane-riscv`] — RISC-V emulation (hint, renders!(Riscv), …)

pub use vane_riscv::*;

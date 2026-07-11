pub use vane_arch::template::*;
vane_arch::cfg_target_wasm! {
    pub use vane_arch::{JitOpcode, WasmJitCtx};
}

pub mod riscv;

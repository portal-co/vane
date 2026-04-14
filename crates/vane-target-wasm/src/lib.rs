#![no_std]
extern crate alloc;

pub trait WasmJitCtx {}

#[derive(Clone)]
pub enum JitOpcode<'a> {
    Operator { op: wasmparser::Operator<'a> },
}

pub trait WasmJit {
    fn jit<'a>(
        &'a self,
        ctx: &'a (dyn WasmJitCtx + 'a),
    ) -> impl Iterator<Item = JitOpcode<'a>> + 'a;
}

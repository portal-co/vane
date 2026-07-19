#![no_std]
extern crate alloc;

use core::fmt::Display;

mod stack;
pub use stack::{
    FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StackOp, StackOpBackend, StateSlot,
};

/// Control-flow abstraction shared by all codegen targets.
///
/// The lifetime `'op` bounds the iterator arguments and return types of
/// [`CoreOpcode::r#loop`], ensuring that body iterators captured from an arch
/// renderer's context do not outlive that context.  `StackOp` itself is
/// `'static` (it holds no borrowed data), so `impl<'op> CoreOpcode<'op> for
/// StackOp` is valid for any `'op`.
pub trait CoreOpcode<'op>: Sized + 'op {
    /// Wrap `body` in a named loop construct.
    ///
    /// - JS: `label: for(;;){ <body> break label; }`
    /// - WASM: `loop … end`
    fn r#loop(
        body: impl Iterator<Item = Self> + 'op,
        name: impl Display,
    ) -> impl Iterator<Item = Self> + 'op;

    /// Emit an unconditional branch to a previously-opened loop.
    ///
    /// `relative_depth` is `depth_at_branch_site − depth_at_label_definition`
    /// (WASM semantics). `name` is the label identifier used by the JS shim
    /// (`continue name`); the WASM shim uses `relative_depth` and ignores it.
    fn br(relative_depth: u32, name: impl Display) -> impl Iterator<Item = Self>;
}

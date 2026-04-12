#![no_std]
extern crate alloc;

use core::fmt::Display;

mod stack;
pub use stack::{MemWidth, StackOp};

/// Control-flow abstraction shared by all codegen targets.
///
/// The lifetime `'op` is the lifetime of opaque data captured inside the lazy
/// structural variants (`StackOp::Loop::body`, `StackOp::If::{then,else}_ops`).
/// For a given arch renderer with context lifetime `'a`, instantiate as
/// `O = StackOp<'a>` and `O: CoreOpcode<'a>`.
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

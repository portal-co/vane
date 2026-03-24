#![no_std]

use core::fmt::Display;
extern crate alloc;
pub trait CoreOpcode {
    fn r#loop(
        i: impl Iterator<Item = Self>,
        name: impl Display,
    ) -> impl Iterator<Item = Self>;
    fn br(index: u32, name: impl Display) -> impl Iterator<Item = Self>;
}
#[cfg(feature = "wasm")]
impl<'a> CoreOpcode for wasmparser::Operator<'a> {
    fn r#loop(
        i: impl Iterator<Item = Self>,
        name: impl Display,
    ) -> impl Iterator<Item = Self> {
        core::iter::once(Self::Loop {
            blockty: wasmparser::BlockType::Empty,
        })
        .chain(i)
        .chain(core::iter::once(Self::End))
    }
    fn br(index: u32, name: impl Display) -> impl Iterator<Item = Self> {
        core::iter::once(Self::Br {
            relative_depth: index,
        })
    }
}

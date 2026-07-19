//! Explicit AArch64 execution embedding.
//!
//! This wrapper keeps the existing RV64 `vane::Reactor` contract untouched.
//! Embedders supply their own [`StackHost`] state, select an entry PC, and run
//! AArch64 StackOp traces through the native interpreter.

use vane_arch::{
    Heat,
    interp::{Exit, StackHost, interpret},
    template::{Flags, Params, TemplateJit},
};

use crate::template::aarch64::a64_emit;

/// Deliberately selected AArch64 trace runner for native/server embeddings.
pub struct Aarch64Reactor<'a, R> {
    /// Guest instruction source. `Mem` requires mutable page access during
    /// trace fetch, so embeddings intentionally provide an exclusive source.
    pub react: &'a mut R,
    /// Root instruction pointer for integrity-check invalidation.
    pub root: u64,
    /// Renderer flags carried into trace creation.
    pub flags: Flags,
}

impl<'a, R: vane_arch::JitCtx> Aarch64Reactor<'a, R> {
    pub fn new(react: &'a mut R) -> Self {
        Self {
            react,
            root: 0,
            flags: Flags::default(),
        }
    }

    /// Execute a trace beginning at `pc`, returning its dispatcher/host exit.
    pub fn run(&self, pc: u64, host: &mut impl StackHost) -> Exit {
        let jit = TemplateJit {
            params: Params {
                react: self.react,
                trial: &|_| Heat::New,
                #[cfg(feature = "target-js")]
                flate: &vane_arch::DebugFlate {},
                root: self.root,
                flags: self.flags,
            },
            pc,
            labels: Default::default(),
            depth: 0,
        };
        interpret(&a64_emit(&jit), host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use vane_arch::{Mem, StateSlot};

    #[test]
    fn explicit_reactor_executes_a64_without_rv64_reactor() {
        let mut mem = Mem::default();
        for (offset, word) in [0xd280_0020u32, 0xd61f_03c0].into_iter().enumerate() {
            for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
                mem.write_byte((offset * 4 + i) as u64, byte);
            }
        }
        struct Host {
            state: BTreeMap<StateSlot, u64>,
            code: BTreeMap<u64, u8>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, s: &StateSlot) -> u64 {
                self.state.get(s).copied().unwrap_or(0)
            }
            fn store_state(&mut self, s: &StateSlot, v: u64) {
                self.state.insert(s.clone(), v);
            }
            fn load_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: vane_arch::MemWidth, _: u64) {}
            fn byte(&mut self, addr: u64) -> u8 {
                self.code.get(&addr).copied().unwrap_or(0)
            }
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            state: BTreeMap::new(),
            code: [0xd280_0020u32, 0xd61f_03c0]
                .into_iter()
                .enumerate()
                .flat_map(|(offset, word)| {
                    word.to_le_bytes()
                        .into_iter()
                        .enumerate()
                        .map(move |(i, byte)| ((offset * 4 + i) as u64, byte))
                })
                .collect(),
        };
        host.state.insert(crate::state::x(30), 0xfeed);
        let exit = Aarch64Reactor::new(&mut mem).run(0, &mut host);
        assert!(matches!(exit, Exit::TailCall(0xfeed)), "{exit:?}");
        assert_eq!(host.state[&crate::state::x(0)], 1);
    }
}

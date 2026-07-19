use super::*;
use crate::state;
use alloc::{boxed::Box, collections::BTreeMap};
use vane_arch::{
    Mem,
    interp::{Exit, StackHost, interpret},
};

#[test]
fn scalar_riscv_fp_ops_execute_through_shared_state() {
    // fadd.s f3,f1,f2 ; fadd.d f6,f4,f5 ; fsqrt.s f8,f7 ; jalr x0,0(x1)
    let words = [0x0020_81d3u32, 0x0252_0353, 0x5803_8453, 0x0000_8067];
    let mut code = Mem::default();
    for (offset, word) in words.into_iter().enumerate() {
        for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
            code.write_byte((offset * 4 + i) as u64, byte);
        }
    }
    let code = Box::leak(Box::new(code));
    let jit = TemplateJit {
        params: Params {
            react: code,
            trial: &|_| Heat::New,
            #[cfg(feature = "target-js")]
            flate: &vane_arch::DebugFlate {},
            root: 0,
            flags: vane_arch::template::Flags::default(),
        },
        pc: 0,
        labels: Default::default(),
        depth: 0,
    };
    struct Host {
        regs: [u64; 32],
        state: BTreeMap<vane_arch::StateSlot, u64>,
        code: BTreeMap<u64, u8>,
    }
    impl StackHost for Host {
        fn load_reg(&mut self, reg: u8) -> u64 {
            self.regs[reg as usize]
        }
        fn store_reg(&mut self, reg: u8, value: u64) {
            self.regs[reg as usize] = value;
        }
        fn load_state(&mut self, slot: &vane_arch::StateSlot) -> u64 {
            self.state.get(slot).copied().unwrap_or(0)
        }
        fn store_state(&mut self, slot: &vane_arch::StateSlot, value: u64) {
            self.state.insert(slot.clone(), value);
        }
        fn load_mem(&mut self, _: u64, _: MemWidth, _: bool) -> u64 {
            0
        }
        fn store_mem(&mut self, _: u64, _: MemWidth, _: u64) {}
        fn byte(&mut self, addr: u64) -> u8 {
            self.code.get(&addr).copied().unwrap_or(0)
        }
        fn ecall(&mut self) -> bool {
            false
        }
    }
    let mut host = Host {
        regs: [0; 32],
        state: BTreeMap::new(),
        code: BTreeMap::new(),
    };
    for (offset, word) in words.into_iter().enumerate() {
        for (i, byte) in word.to_le_bytes().into_iter().enumerate() {
            host.code.insert((offset * 4 + i) as u64, byte);
        }
    }
    host.regs[1] = 0xfeed;
    host.state.insert(state::f(1), 1.25f32.to_bits() as u64);
    host.state.insert(state::f(2), 2.5f32.to_bits() as u64);
    host.state.insert(state::f(4), 1.5f64.to_bits());
    host.state.insert(state::f(5), 2.25f64.to_bits());
    host.state.insert(state::f(7), 9.0f32.to_bits() as u64);
    assert!(matches!(
        interpret(&rv_emit(&jit), &mut host),
        Exit::TailCall(0xfeed)
    ));
    assert_eq!(host.state[&state::f(3)], 3.75f32.to_bits() as u64);
    assert_eq!(host.state[&state::f(6)], 3.75f64.to_bits());
    assert_eq!(host.state[&state::f(8)], 3.0f32.to_bits() as u64);
}

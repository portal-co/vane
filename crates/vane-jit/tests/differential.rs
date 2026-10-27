//! Differential tests: the native JIT must produce identical register state,
//! memory state, and `Exit` as the interpreter (`interp_trace`), for the same
//! trace. This is the Phase-2 correctness gate — a native-encoding bug that
//! diverges here is caught before it can corrupt a guest.

use vane_arch::interp::{compile_trace_ptr, Exit, LocalReactor, LocalStackHost};
use vane_arch::{FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StackOp, StateSlot};
use vane_jit::compiled_trace;

/// A minimal in-memory reactor (register file + sparse memory).
struct MockReactor {
    regs: std::cell::RefCell<[u64; 32]>,
    mem: std::cell::RefCell<std::collections::BTreeMap<u64, u8>>,
}

impl LocalReactor for MockReactor {
    fn lr_load_reg(&self, r: u8) -> u64 {
        self.regs.borrow()[(r % 32) as usize]
    }
    fn lr_store_reg(&self, r: u8, v: u64) {
        self.regs.borrow_mut()[(r % 32) as usize] = v;
    }
    fn lr_read_byte(&self, addr: u64) -> u8 {
        self.mem.borrow().get(&addr).copied().unwrap_or(0)
    }
    fn lr_write_byte(&self, addr: u64, v: u8) {
        self.mem.borrow_mut().insert(addr, v);
    }
    fn lr_has_local_regfile(&self) -> bool {
        true
    }
}

fn make_host() -> LocalStackHost<MockReactor> {
    LocalStackHost {
        reactor: MockReactor {
            regs: std::cell::RefCell::new([0; 32]),
            mem: std::cell::RefCell::new(Default::default()),
        },
        test_mode: false,
        pc: 0,
        pending_ecall: false,
        undo_log: Vec::new(),
        undo_mark: 0,
        regs: [0; 32],
        regs_dirty: false,
        reg_cache: true,
        check_code_integrity: false,
    }
}

/// A comparable projection of `Exit` (Exit isn't PartialEq — it carries a
/// String).
fn exit_key(e: &Exit) -> (u8, u64) {
    match e {
        Exit::TailCall(pc) => (0, *pc),
        Exit::Trap(_) => (1, 0),
        Exit::Ecall => (2, 0),
        Exit::Bail(pc) => (3, *pc),
    }
}

/// Run a trace through both the interpreter and the native JIT; assert they
/// produce identical register state, memory state, and exit.
fn check(ops: Vec<StackOp>) {
    let shared = vane_arch::shared_new(ops);
    let ct_interp = compile_trace_ptr::<MockReactor>(shared.clone());
    let ct_native =
        compiled_trace::<MockReactor>(shared.clone()).expect("the trace should compile to native");
    let mut host_i = make_host();
    let mut host_n = make_host();
    let exit_i = (ct_interp.f)(&mut host_i, &ct_interp);
    let exit_n = (ct_native.f)(&mut host_n, &ct_native);
    assert_eq!(
        exit_key(&exit_i),
        exit_key(&exit_n),
        "exit divergence: interp {exit_i:?} vs native {exit_n:?}"
    );
    assert_eq!(
        host_i.regs, host_n.regs,
        "register divergence: interp {:?} vs native {:?}",
        host_i.regs, host_n.regs
    );
    assert_eq!(
        *host_i.reactor.mem.borrow(),
        *host_n.reactor.mem.borrow(),
        "memory divergence"
    );
}

#[test]
fn alu_and_registers() {
    check(vec![
        StackOp::CheckCode {
            addr: 0x1000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::LoadReg(1), // x1 = 0
        StackOp::PushImm(5),
        StackOp::Add64, // 5
        StackOp::StoreReg(2), // x2 = 5
        StackOp::LoadReg(2),
        StackOp::PushImm(3),
        StackOp::Mul64, // 15
        StackOp::StoreReg(3), // x3 = 15
        StackOp::LoadReg(3),
        StackOp::PushImm(0),
        StackOp::DivU64, // 15 / 0 = u64::MAX
        StackOp::StoreReg(4), // x4 = u64::MAX
        StackOp::LoadReg(3),
        StackOp::PushImm(0),
        StackOp::RemU64, // 15 % 0 = 15
        StackOp::StoreReg(5), // x5 = 15
        StackOp::PushImm(0x8000_0000_0000_0000u64),
        StackOp::PushImm(0),
        StackOp::DivS64, // i64::MIN / ... no: 0x8000..0 / 0 = u64::MAX
        StackOp::StoreReg(6),
        StackOp::PushImm(0x8000),
        StackOp::TailCall,
    ]);
}

#[test]
fn shifts_and_comparisons() {
    check(vec![
        StackOp::CheckCode {
            addr: 0x2000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(1),
        StackOp::PushImm(70), // > 63: the shift masks to 6 bits
        StackOp::Shl64,       // 1 << (70 & 63) = 1 << 6 = 64
        StackOp::StoreReg(1), // x1 = 64
        StackOp::PushImm(5),
        StackOp::PushImm(3),
        StackOp::LtU64, // 5 < 3 = 0
        StackOp::StoreReg(2), // x2 = 0
        StackOp::PushImm(5),
        StackOp::PushImm(3),
        StackOp::GeS64, // 5 >= 3 = 1
        StackOp::StoreReg(3), // x3 = 1
        StackOp::PushImm(0x9000),
        StackOp::TailCall,
    ]);
}

#[test]
fn memory_round_trip() {
    check(vec![
        StackOp::CheckCode {
            addr: 0x3000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(0x1000),
        StackOp::PushImm(0xdead_beef),
        StackOp::StoreMem {
            width: MemWidth::B8,
        },
        StackOp::PushImm(0x1000),
        StackOp::LoadMem {
            width: MemWidth::B8,
            signed: false,
        },
        StackOp::StoreReg(1), // x1 = 0xdead_beef
        StackOp::PushImm(0x1001),
        StackOp::LoadMem {
            width: MemWidth::B1,
            signed: true,
        },
        StackOp::StoreReg(2), // x2 = the byte at 0x1001, sign-extended
        StackOp::PushImm(0xa000),
        StackOp::TailCall,
    ]);
}

#[test]
fn if_else_control_flow() {
    check(vec![
        StackOp::CheckCode {
            addr: 0x4000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(1), // cond: nonzero
        StackOp::IfBegin,
        StackOp::PushImm(10),
        StackOp::StoreReg(1),
        StackOp::IfElse,
        StackOp::PushImm(20),
        StackOp::StoreReg(1),
        StackOp::IfEnd,
        StackOp::PushImm(0), // cond: zero
        StackOp::IfBegin,
        StackOp::PushImm(30),
        StackOp::StoreReg(2),
        StackOp::IfElse,
        StackOp::PushImm(40),
        StackOp::StoreReg(2),
        StackOp::IfEnd,
        StackOp::PushImm(0xb000),
        StackOp::TailCall,
    ]);
}

#[test]
fn float_ops_match_host_fp() {
    check(vec![
        StackOp::CheckCode {
            addr: 0x5000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(1.5f64.to_bits()),
        StackOp::PushImm(2.5f64.to_bits()),
        StackOp::FloatBinary {
            width: FloatWidth::F64,
            op: FloatBinOp::Add,
        },
        StackOp::StoreReg(1), // x1 = 4.0
        StackOp::PushImm(4.0f64.to_bits()),
        StackOp::FloatUnary {
            width: FloatWidth::F64,
            op: FloatUnaryOp::Sqrt,
        },
        StackOp::StoreReg(2), // x2 = 2.0
        StackOp::PushImm(1.25f32.to_bits() as u64),
        StackOp::PushImm(2.5f32.to_bits() as u64),
        StackOp::FloatBinary {
            width: FloatWidth::F32,
            op: FloatBinOp::Mul,
        },
        StackOp::StoreReg(3), // x3 = 1.25 * 2.5 (f32)
        StackOp::PushImm(0xc000),
        StackOp::TailCall,
    ]);
}

#[test]
fn state_slots_match() {
    // The production LocalStackHost's state slots are no-ops (load -> 0,
    // store discarded); the native JIT must match exactly.
    check(vec![
        StackOp::CheckCode {
            addr: 0x6000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(42),
        StackOp::StoreState(StateSlot::new("f5")),
        StackOp::LoadState(StateSlot::new("f5")),
        StackOp::StoreReg(1), // x1 = 0 (the no-op state load)
        StackOp::PushImm(0xd000),
        StackOp::TailCall,
    ]);
}

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
    /// Direct-tail wiring (the direct_tail_chains test): pc -> `(target
    /// machine code, unused)` — mirrors the real registry (only registered
    /// pcs chain; anything else exits to the dispatch loop).
    tail: std::cell::RefCell<std::collections::BTreeMap<u64, (usize, usize, usize)>>,
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
    fn lr_direct_tail_target(&mut self, pc: u64) -> (usize, usize, usize) {
        let (code, _, _) = self.tail.borrow().get(&pc).copied().unwrap_or((0, 0, 0));
        (code, 0usize, 0usize)
    }
}

fn make_host() -> LocalStackHost<MockReactor> {
    LocalStackHost {
        reactor: MockReactor {
            regs: std::cell::RefCell::new([0; 32]),
            mem: std::cell::RefCell::new(Default::default()),
            tail: std::cell::RefCell::new(Default::default()),
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
        StackOp::Add64,       // 5
        StackOp::StoreReg(2), // x2 = 5
        StackOp::LoadReg(2),
        StackOp::PushImm(3),
        StackOp::Mul64,       // 15
        StackOp::StoreReg(3), // x3 = 15
        StackOp::LoadReg(3),
        StackOp::PushImm(0),
        StackOp::DivU64,      // 15 / 0 = u64::MAX
        StackOp::StoreReg(4), // x4 = u64::MAX
        StackOp::LoadReg(3),
        StackOp::PushImm(0),
        StackOp::RemU64,      // 15 % 0 = 15
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
        StackOp::LtU64,       // 5 < 3 = 0
        StackOp::StoreReg(2), // x2 = 0
        StackOp::PushImm(5),
        StackOp::PushImm(3),
        StackOp::GeS64,       // 5 >= 3 = 1
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

/// A hot-loop microbenchmark: run ONE stable trace many times (no
/// invalidation), comparing the interpreter vs the native JIT. This is the
/// workload the JIT is for — unlike the cold-code-dominated, sfence-thrashed
/// Linux boot. Run: `cargo test -p vane-jit --release --test differential --
/// --nocapture hot_loop_microbench`.
#[test]
fn hot_loop_microbench() {
    // r1 = N; while r1 != 0 { r1--; r2 = r2*3 + 5 }. A realistic trace: a
    // CheckCode per loop iteration (as rv_emit emits one per instruction), so
    // the undo buffer resets each iteration (a CheckCode-less loop would
    // overflow it — see the compile-time undo bound).
    let n = 1000u64;
    let make_ops = || {
        let cc = || StackOp::CheckCode {
            addr: 0x1000,
            expected: Vec::new().into_boxed_slice(),
            miss_label: String::new(),
        };
        vec![
            StackOp::PushImm(n),
            StackOp::StoreReg(1),
            StackOp::LoopBegin { label: 0x1000 },
            cc(),
            StackOp::LoadReg(1),
            StackOp::IfBegin,
            StackOp::LoadReg(1),
            StackOp::PushImm(-1i64 as u64),
            StackOp::Add64,
            StackOp::StoreReg(1),
            StackOp::LoadReg(2),
            StackOp::PushImm(3),
            StackOp::Mul64,
            StackOp::PushImm(5),
            StackOp::Add64,
            StackOp::StoreReg(2),
            StackOp::Br {
                relative_depth: 1,
                label: 0x1000,
            },
            StackOp::IfElse,
            StackOp::IfEnd,
            StackOp::LoopEnd,
            StackOp::PushImm(0),
            StackOp::TailCall,
        ]
    };
    // Correctness first — this also gives the differential suite loop (Br)
    // coverage: the interpreter and the native JIT must produce identical
    // r1/r2, memory, and exit.
    check(make_ops());
    // Benchmark: run one stable trace back-to-back (no invalidation thrash).
    let shared = vane_arch::shared_new(make_ops());
    let ct_interp = compile_trace_ptr::<MockReactor>(shared.clone());
    let ct_native = compiled_trace::<MockReactor>(shared.clone()).expect("compiles");
    const RUNS: usize = 5000;
    let t0 = std::time::Instant::now();
    for _ in 0..RUNS {
        let mut host = make_host();
        (ct_interp.f)(&mut host, &ct_interp);
    }
    let interp = t0.elapsed();
    let t0 = std::time::Instant::now();
    for _ in 0..RUNS {
        let mut host = make_host();
        (ct_native.f)(&mut host, &ct_native);
    }
    let native = t0.elapsed();
    eprintln!(
        "hot loop: {} runs x {} iters — interp {:?}, native {:?} ({:.2}x)",
        RUNS,
        n,
        interp,
        native,
        interp.as_secs_f64() / native.as_secs_f64()
    );
}

/// Convert a width discriminant back to `MemWidth` (matches the shim's `mw`).
#[cfg(target_arch = "aarch64")]
fn mw(d: u8) -> MemWidth {
    match d {
        0 => MemWidth::B1,
        1 => MemWidth::B2,
        2 => MemWidth::B4,
        _ => MemWidth::B8,
    }
}

/// Run a trace through the interpreter AND the async (fiber) native JIT,
/// fulfilling each pending memory op against the host; assert they match. This
/// is the Phase-A async-host proof: a JIT'd trace suspends on a host op, the
/// driver fulfills it (a real non-local host would await here), the fiber
/// resumes, and the result is bit-exact with the interpreter.
#[cfg(target_arch = "aarch64")]
fn check_async(ops: Vec<StackOp>) {
    use vane_jit::arun::{AsyncTraceRun, PendingOp, Step};
    let shared = vane_arch::shared_new(ops);
    let ct_interp = compile_trace_ptr::<MockReactor>(shared.clone());
    let ct_native = compiled_trace::<MockReactor>(shared.clone()).expect("compiles");
    let mut host_i = make_host();
    let mut host_n = make_host();
    let exit_i = (ct_interp.f)(&mut host_i, &ct_interp);
    let exit_n = {
        let mut run = AsyncTraceRun::start(&mut host_n, &ct_native);
        let tag = loop {
            match run.step() {
                Step::Done(tag) => break tag,
                Step::Suspended(PendingOp::LoadMem {
                    addr,
                    width,
                    signed,
                }) => {
                    let v = host_n.reactor.lr_load_mem(addr, mw(width), signed);
                    run.resume_with(v);
                }
                Step::Suspended(PendingOp::StoreMem { addr, width, value }) => {
                    host_n.reactor.lr_store_mem(addr, mw(width), value);
                    run.resume_with(0);
                }
            }
        };
        run.finish(tag, &mut host_n, &ct_native)
    };
    assert_eq!(
        exit_key(&exit_i),
        exit_key(&exit_n),
        "async exit divergence"
    );
    assert_eq!(host_i.regs, host_n.regs, "async register divergence");
    assert_eq!(
        *host_i.reactor.mem.borrow(),
        *host_n.reactor.mem.borrow(),
        "async memory divergence"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn async_memory_ops_suspend_and_resume() {
    // A memory round-trip (the canonical "remote memory" async op): each
    // LoadMem/StoreMem suspends the fiber; the driver fulfills it; the trace
    // resumes. Multiple suspensions (three memory ops) must all work.
    check_async(vec![
        StackOp::CheckCode {
            addr: 0x7000,
            expected: Box::from([0u8; 4]),
            miss_label: String::new(),
        },
        StackOp::PushImm(0x2000),
        StackOp::PushImm(0xcafe_f00d),
        StackOp::StoreMem {
            width: MemWidth::B8,
        },
        StackOp::PushImm(0x2000),
        StackOp::LoadMem {
            width: MemWidth::B8,
            signed: false,
        },
        StackOp::StoreReg(1), // x1 = 0xcafe_f00d
        StackOp::PushImm(0x2001),
        StackOp::LoadMem {
            width: MemWidth::B1,
            signed: true,
        },
        StackOp::StoreReg(2), // x2 = the sign-extended byte at 0x2001
        StackOp::PushImm(0x2008),
        StackOp::LoadReg(1),
        StackOp::StoreMem {
            width: MemWidth::B4,
        },
        StackOp::PushImm(0xe000),
        StackOp::TailCall,
    ]);
}

/// The direct-tail chain gate: trace A's TailCall jumps straight into
/// trace B's MACHINE CODE with the current TraceCtx (no adapter round
/// trip); B runs entirely and its exit flows back through the shared ctx
/// to A's adapter. This is the bug class the boot SIGBUS came from
/// (jumping into native_adapter — an sret Rust fn — instead of the code).
#[test]
fn native_direct_tail_chains() {
    let b_ops = vec![
        StackOp::CheckCode {
            addr: 0x2000,
            expected: vec![].into(),
            miss_label: String::new(),
        },
        StackOp::PushImm(0x5B5B),
        StackOp::StoreReg(2),
        StackOp::PushImm(0x8000),
        StackOp::TailCall,
    ];
    let a_ops = vec![
        StackOp::CheckCode {
            addr: 0x1000,
            expected: vec![].into(),
            miss_label: String::new(),
        },
        StackOp::PushImm(0xAAA),
        StackOp::StoreReg(1),
        StackOp::PushImm(0x2000), // B's pc
        StackOp::TailCall,
    ];
    // B first: its machine-code entry is what A's lookup returns.
    let b = compiled_trace::<MockReactor>(vane_arch::shared_new(b_ops))
        .expect("trace B compiles");
    let b_code = b.native.as_ref().expect("native").code as usize;
    let a = compiled_trace::<MockReactor>(vane_arch::shared_new(a_ops))
        .expect("trace A compiles");

    let mut host = make_host();
    host.reactor
        .tail
        .borrow_mut()
        .insert(0x2000, (b_code, 0, 0));

    let exit = (a.f)(&mut host, &a);
    assert_eq!(exit_key(&exit), (0, 0x8000), "B's exit propagates");
    // The register CACHE (host.regs — where ctx.regs points) is the
    // harness's result surface (same as every differential test).
    assert_eq!(host.regs[1], 0xAAA, "A ran");
    assert_eq!(
        host.regs[2], 0x5B5B,
        "B ran via the direct tail (no adapter round trip)"
    );
}

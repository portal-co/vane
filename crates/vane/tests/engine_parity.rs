//! D5 engine-parity test (PLAN_LINUX_WASM.md §4.3): the same guest program
//! is executed by `interp_core` (per-instruction interpreter) and by
//! `trace_run` (StackOp trace interpreter), and their architectural states
//! — full register file and a written memory word — must agree exactly.
//!
//! This test only exists in the plain-Rust configuration: under `js-host`
//! the trace interpreter's register file lives behind JS imports that do
//! not exist in a native test process.

#![cfg(not(feature = "js-host"))]

use rv_asm::{Imm, Inst, Reg, Xlen};
use std::future::Future;
use std::task::{Context, Poll, Waker};
use vane::Reactor;
use vane_arch::interp::LocalReactor;

const BASE: u64 = 0x8000_0000;
// NOTE: must have bit 31 clear — RV64 LUI sign-extends its immediate.
const OUT_ADDR: u64 = 0x1001_0000;

/// Minimal block_on: our futures never actually pend (all host access is
/// synchronous), so a noop waker suffices.
fn block_on<F: Future>(f: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Assemble with rv-asm itself so the test cannot drift from the decoder's
/// own encoding tables.
fn assemble(program: &[Inst]) -> Vec<u8> {
    let mut out = Vec::new();
    for inst in program {
        out.extend_from_slice(&inst.encode_normal(Xlen::Rv64).to_le_bytes());
    }
    out
}

fn program() -> Vec<u8> {
    assemble(&[
        // x1 = 0x12345678
        Inst::Lui {
            uimm: Imm::new_u32(0x12345_000),
            dest: Reg(1),
        },
        Inst::Addi {
            imm: Imm::new_i32(0x678),
            dest: Reg(1),
            src1: Reg(1),
        },
        // x2 = 5 (loop counter)
        Inst::Addi {
            imm: Imm::new_i32(5),
            dest: Reg(2),
            src1: Reg(0),
        },
        // loop: x2 -= 1; x3 += x1; while x2 != 0
        Inst::Addi {
            imm: Imm::new_i32(-1),
            dest: Reg(2),
            src1: Reg(2),
        },
        Inst::Add {
            src2: Reg(1),
            dest: Reg(3),
            src1: Reg(3),
        },
        Inst::Bne {
            offset: Imm::new_i32(-8),
            src1: Reg(2),
            src2: Reg(0),
        },
        // x4 = OUT_ADDR; *x4 = x3
        Inst::Lui {
            uimm: Imm::new_u32(OUT_ADDR as u32),
            dest: Reg(4),
        },
        Inst::Sw {
            offset: Imm::new_i32(0),
            src: Reg(3),
            base: Reg(4),
        },
        // Terminate: the handler stops at the first ecall.
        Inst::Ecall,
    ])
}

#[test]
fn interp_core_and_trace_run_agree() {
    let prog = program();

    // Engine 1: per-instruction interpreter.
    let a = Reactor::new();
    a.write_bytes(BASE, &prog);
    let mut regs_a = [0u64; 32];
    let mut stop = async |_regs: &mut [u64; 32]| -> bool { true };
    let result_a = block_on(a.interp_core(BASE, &mut regs_a, &mut stop));
    assert!(result_a.is_ok(), "interp_core failed: {result_a:?}");

    // Engine 2: StackOp trace interpreter.
    let b = Reactor::new();
    b.write_bytes(BASE, &prog);
    let mut stop2 = async |_host: &mut vane_arch::interp::LocalStackHost<Reactor>| -> bool { true };
    let result_b = block_on(b.trace_run(BASE, &mut stop2));
    assert!(result_b.is_ok(), "trace_run failed: {result_b:?}");

    // Full register file must agree.
    for i in 1..32u8 {
        assert_eq!(
            regs_a[i as usize],
            b.lr_load_reg(i),
            "register x{i} diverged between engines",
        );
    }

    // The store's target word must agree (and match the expected value).
    let word = |r: &Reactor| -> u64 {
        (0..8).fold(0u64, |acc, i| {
            acc | ((r.read_guest_byte(OUT_ADDR + i) as u64) << (8 * i))
        })
    };
    const EXPECTED: u64 = 0x12345678 * 5;
    assert_eq!(word(&a), EXPECTED, "interp_core computed the wrong sum");
    assert_eq!(word(&a), word(&b), "stored memory diverged between engines");
}

#![cfg(not(feature = "js-host"))]
//! The compiled-trace registry: a hot trace can be compiled to a callable
//! (function pointer) and run instead of interpreting, with identical
//! results. This is the mechanism Weval/JIT/patcher backends plug into.
use rv_asm::{Imm, Inst, Reg, Xlen};
use std::future::Future;
use vane::Reactor;
use vane_arch::interp::LocalReactor;
fn block_on<F: Future>(f: F) -> F::Output {
    let w = std::task::Waker::noop(); let mut cx = std::task::Context::from_waker(&w);
    let mut f = std::pin::pin!(f);
    loop { match f.as_mut().poll(&mut cx) { std::task::Poll::Ready(v) => break v, std::task::Poll::Pending => std::thread::yield_now() } }
}
fn assemble(p: &[Inst]) -> Vec<u8> { p.iter().flat_map(|i| i.encode_normal(Xlen::Rv64).to_le_bytes()).collect() }
const BASE: u64 = 0x8000_0000;

#[test]
fn compiled_trace_matches_interpreter() {
    // x1 = 200; loop: x1 -= 1; x3 += 7; bne x1,x0,loop; ecall
    let prog = assemble(&[
        Inst::Addi { imm: Imm::new_i32(200), dest: Reg(1), src1: Reg(0) },
        Inst::Addi { imm: Imm::new_i32(-1), dest: Reg(1), src1: Reg(1) },
        Inst::Addi { imm: Imm::new_i32(7), dest: Reg(3), src1: Reg(3) },
        Inst::Bne { offset: Imm::new_i32(-8), src1: Reg(1), src2: Reg(0) },
        Inst::Ecall,
    ]);
    let run = |compile: bool| -> (u64, u64) {
        let r = Reactor::new();
        r.write_bytes(BASE, &prog);
        let mut stop = async |_h: &mut vane::vane_riscv::interp::LocalStackHost<Reactor>| -> bool { true };
        // Run once (interpret), then promote the loop trace to a callable.
        let mut pc = BASE;
        let mut compiled = false;
        for _ in 0..50u64 {
            if compile && !compiled {
                // Promote any cached traces to callables.
                let _ = r.compile_trace_at(pc);
                compiled = true;
            }
            match block_on(r.trace_run_fueled(pc, &mut stop, Some(500))) {
                Ok(next) => { if next == pc { break; } pc = next; }
                Err(_) => break,
            }
        }
        (r.lr_load_reg(1), r.lr_load_reg(3))
    };
    let interp = run(false);
    let compiled = run(true);
    assert_eq!(interp, compiled, "compiled trace must match interpreter");
    assert_eq!(interp.0, 0, "loop counter reached 0");
    assert_eq!(interp.1, 200 * 7, "accumulator correct");
}

#[test]
fn register_and_run_compiled() {
    let prog = assemble(&[
        Inst::Addi { imm: Imm::new_i32(5), dest: Reg(1), src1: Reg(0) },
        Inst::Ecall,
    ]);
    let r = Reactor::new();
    r.write_bytes(BASE, &prog);
    let mut stop = async |_h: &mut vane::vane_riscv::interp::LocalStackHost<Reactor>| -> bool { true };
    let _ = block_on(r.trace_run_fueled(BASE, &mut stop, Some(10)));
    assert!(r.compile_trace_at(BASE), "trace should be cached after a run");
    assert!(r.has_compiled_trace(BASE));
}

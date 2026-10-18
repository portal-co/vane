#![cfg(not(feature = "js-host"))]
//! The interpreter emits its hot trace cache (StackOp traces) for Wizer:
//! after running guest code through the trace engine, `dump_trace_cache`
//! serializes the cache to bytes the patcher reads from the snapshotted WASM.
use rv_asm::{Imm, Inst, Reg, Xlen};
use std::future::Future;
use vane::Reactor;
use vane_arch::interp::LocalReactor;

fn block_on<F: Future>(f: F) -> F::Output {
    let w = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(&w);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => break v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}
fn assemble(p: &[Inst]) -> Vec<u8> {
    p.iter()
        .flat_map(|i| i.encode_normal(Xlen::Rv64).to_le_bytes())
        .collect()
}
const BASE: u64 = 0x8000_0000;

#[test]
fn interpreter_dumps_its_trace_cache() {
    // x1 = 50; loop: x1 -= 1; bne x1,x0,loop; ecall
    let prog = assemble(&[
        Inst::Addi { imm: Imm::new_i32(50), dest: Reg(1), src1: Reg(0) },
        Inst::Addi { imm: Imm::new_i32(-1), dest: Reg(1), src1: Reg(1) },
        Inst::Bne { offset: Imm::new_i32(-4), src1: Reg(1), src2: Reg(0) },
        Inst::Ecall,
    ]);
    let r = Reactor::new();
    r.write_bytes(BASE, &prog);
    let mut stop =
        async |_h: &mut vane::vane_riscv::interp::LocalStackHost<Reactor>| -> bool { true };
    // Run the loop to populate the trace cache.
    let _ = block_on(r.trace_run_fueled(BASE, &mut stop, Some(1000)));

    // The cache holds the compiled trace(s); the dump serializes them.
    assert!(r.trace_cache_len() > 0, "trace cache populated");
    let bytes = r.dump_trace_cache();
    assert!(!bytes.is_empty());
    let traces = vane::vane_riscv::stackop_ser::deserialize_trace_cache(&bytes)
        .expect("deserialize");
    assert_eq!(traces.len(), r.trace_cache_len());
    // Every dumped trace is keyed by a guest pc in the program's range and is
    // non-empty.
    for (pc, ops) in &traces {
        assert!(*pc >= BASE && *pc < BASE + prog.len() as u64, "pc {pc:#x} in range");
        assert!(!ops.is_empty(), "trace at {pc:#x} non-empty");
    }
}

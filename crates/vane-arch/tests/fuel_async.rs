#![cfg(feature = "async-host")]
use vane_arch::interp::{Exit, LocalReactor, LocalStackHost, interpret_async_fueled};
use vane_arch::{MemWidth, StackOp};

struct H;
impl LocalReactor for H {
    fn lr_load_reg(&self, _r: u8) -> u64 {
        0
    }
    fn lr_store_reg(&self, _r: u8, _v: u64) {}
    fn lr_read_byte(&self, _a: u64) -> u8 {
        0
    }
    fn lr_write_byte(&self, _a: u64, _v: u8) {}
}

#[test]
fn spin_loop_yields_on_fuel() {
    // A trace for `spin: j spin` — LoopBegin, CheckCode(match), Br(0).
    let ops = vec![
        StackOp::LoopBegin { label: "x".into() },
        StackOp::CheckCode {
            addr: 0x1000,
            expected: Box::from([0u8; 4]),
            miss_label: "x".into(),
        },
        StackOp::Br {
            relative_depth: 1,
            label: "x".into(),
        },
        StackOp::LoopEnd,
    ];
    // H's memory reads 0; CheckCode expects [0;4] → matches (0==0).
    let mut fuel = 100u64;
    let exit = {
        let mut host = LocalStackHost {
            reactor: H,
            test_mode: false,
            pc: 0x1000,
            pending_ecall: false,
            undo_log: Vec::new(),
            undo_mark: 0,
            regs: [0; 32],
            regs_dirty: false,
            check_code_integrity: true,
        };
        host.load_regs();
        // drive the async fn with a noop waker
        let w = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(&w);
        let mut f = std::pin::pin!(interpret_async_fueled(&ops, &mut host, Some(&mut fuel)));
        loop {
            match f.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => break v,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    };
    let exit = exit.expect("fuel supplied");
    println!("exit: {exit:?}, remaining fuel: {fuel}");
    assert!(matches!(exit, Exit::TailCall(0x1000)));
    assert_eq!(fuel, 0);
}

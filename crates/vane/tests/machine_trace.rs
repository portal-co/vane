#![cfg(not(feature = "js-host"))]
//! Regression: the machine payload (devices, timer, PLIC, syscon) must run
//! identically under the trace engine (the StackOp port).
use std::convert::TryInto;
use std::future::Future;
use vane::Reactor;

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

#[test]
fn machine_payload_via_traces() {
    let bytes = std::fs::read(
        "../../harness/payload-machine/target/riscv64gc-unknown-none-elf/release/payload-machine",
    )
    .unwrap();
    let r = Reactor::new();
    // minimal ELF64 load
    let u64le =
        |o: usize| -> u64 { u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()) };
    let entry = u64le(24);
    let phoff = u64le(32) as usize;
    let phentsize = u16::from_le_bytes([bytes[54], bytes[55]]) as usize;
    let phnum = u16::from_le_bytes([bytes[56], bytes[57]]) as usize;
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        if u32::from_le_bytes(bytes[ph..ph + 4].try_into().unwrap()) != 1 {
            continue;
        }
        let off = u64le(ph + 8) as usize;
        let vaddr = u64le(ph + 16);
        let paddr = u64le(ph + 24);
        let filesz = u64le(ph + 32) as usize;
        let dest = if paddr != 0 { paddr } else { vaddr };
        r.write_bytes(dest, &bytes[off..off + filesz]);
    }
    r.set_hart_enabled(true);
    r.set_machine_enabled(true);
    r.with_machine(|m| m.uart.push_rx(b"V"));
    let mut pc = entry;
    let mut never =
        async |_h: &mut vane::vane_riscv::interp::LocalStackHost<Reactor>| -> bool { false };
    let mut out = String::new();
    let mut power = None;
    for _ in 0..400u64 {
        match block_on(r.trace_run_fueled(pc, &mut never, Some(5))) {
            Ok(next) => pc = next,
            Err(e) => panic!("trace engine failed at {pc:#x}: {e}"),
        }
        if let Some(tx) = r.with_machine(|m| m.uart.drain_tx()) {
            out.push_str(&String::from_utf8_lossy(&tx));
        }
        if let Some(Some(p)) = r.with_machine(|m| m.syscon.power) {
            power = Some(p);
            break;
        }
    }
    assert_eq!(
        power,
        Some(vane::vane_riscv::machine::PowerAction::PowerOff { code: 0 }),
        "the machine payload should power off cleanly; UART output: {out:?}"
    );
    assert!(out.contains("M1:TTTK"), "expected the full marker sequence, got {out:?}");
}

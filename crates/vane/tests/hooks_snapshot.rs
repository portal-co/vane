#![cfg(not(feature = "js-host"))]
//! Hook system + machine snapshot tests (PLAN_VIRTIO_WEVAL workstream E;
//! spday full-virtualization plan items B/C).
//!
//! - Hooked addresses must start their own trace even when reached by
//!   fallthrough (the emitter cuts the inlining chain), and the dispatch
//!   loop records one event per arrival with the live argument registers.
//! - Snapshot → restore into a forked reactor resumes bit-identically; a
//!   fork poked with different input diverges (trajectory forking).
use std::future::Future;
use vane::vane_riscv::hooks::HookSet;
use vane::vane_riscv::interp::{AsyncStackHost, LocalStackHost};
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

fn load_words(r: &Reactor, base: u64, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        r.write_bytes(base + 4 * i as u64, &w.to_le_bytes());
    }
}

/// A tiny driver: run one trace at a time, capturing bytes the guest
/// `write`s (legacy ecall ABI) and stopping at exit.
struct Drive {
    pc: u64,
    out: Vec<u8>,
    exited: bool,
}

fn drive(
    r: &Reactor,
    start_pc: u64,
    stop_after_writes: Option<usize>,
    max_traces: u64,
) -> Drive {
    let out = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let exited = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut pc = start_pc;
    for _ in 0..max_traces {
        let out2 = out.clone();
        let exited2 = exited.clone();
        let mut on_ecall = async move |h: &mut LocalStackHost<Reactor>| -> bool {
            if h.regs[17] == 64 {
                // write(fd, buf, len)
                let (buf, len) = (h.regs[11], h.regs[12]);
                for i in 0..len {
                    let b = AsyncStackHost::load_mem(h, buf + i, vane::vane_riscv::MemWidth::B1, false).await;
                    out2.borrow_mut().push(b as u8);
                }
                h.regs[10] = len;
                false
            } else {
                exited2.set(true);
                true
            }
        };
        match block_on(r.trace_run_fueled(pc, &mut on_ecall, Some(1))) {
            Ok(next) => pc = next,
            Err(e) => panic!("trace run failed at {pc:#x}: {e}"),
        }
        if exited.get() {
            break;
        }
        if let Some(n) = stop_after_writes {
            if out.borrow().len() >= n {
                break;
            }
        }
    }
    let out = out.borrow().clone();
    let exited = exited.get();
    Drive { pc, out, exited }
}

/// write(1, 0x2000, 1) ['A']; 40 nops; write(1, 0x2001, 1) ['B']; exit(0).
fn two_write_words() -> Vec<u32> {
    let mut words = vec![
        0x0010_0513u32, // addi a0, x0, 1   (fd = 1)
        0x0000_25b7, // lui a1, 2           (a1 = 0x2000)
        0x0010_0613, // addi a2, x0, 1      (len = 1)
        0x0400_0893, // addi a7, x0, 64     (write)
        0x0000_0073, // ecall
    ];
    words.extend(std::iter::repeat_n(0x0000_0013u32, 40)); // nops
    words.extend_from_slice(&[
        0x0010_0513, // addi a0, x0, 1
        0x0000_25b7, // lui a1, 2
        0x0015_8593, // addi a1, a1, 1      (a1 = 0x2001)
        0x0010_0613, // addi a2, x0, 1
        0x0400_0893, // addi a7, x0, 64
        0x0000_0073, // ecall
        0x0000_0513, // addi a0, x0, 0
        0x05d0_0893, // addi a7, x0, 93     (exit)
        0x0000_0073, // ecall
    ]);
    words
}

/// Hooked address reached by fallthrough: without the emission-side
/// boundary the whole sequence would inline into one trace and the hook
/// would never fire.
#[test]
fn hook_fires_at_fallthrough_symbol() {
    let words = [
        0x0070_0513u32, // 0x1000: addi a0, x0, 7
        0x0010_0593, // 0x1004: addi a1, x0, 1
        // 0x1008: hooked "function entry" (fallthrough target):
        0x05d0_0893, // 0x1008: addi a7, x0, 93 (exit)
        0x0000_0073, // 0x100c: ecall
    ];
    let r = Reactor::new();
    load_words(&r, 0x1000, &words);
    let mut hooks = HookSet::new();
    hooks.insert(0x1008, "my_symbol");
    r.install_hooks(hooks);
    assert_eq!(r.hooks_len(), 1);

    let d = drive(&r, 0x1000, None, 100);
    assert!(d.exited);

    let events = r.take_hook_events();
    assert_eq!(events.len(), 1, "the hook must fire exactly once");
    let ev = &events[0];
    assert_eq!(ev.name, "my_symbol");
    assert_eq!(ev.addr, 0x1008);
    assert_eq!(ev.pc, 0x1008);
    // Real symbol location ⇒ real argument registers: a0/a1 live at entry.
    assert_eq!(ev.args[0], 7);
    assert_eq!(ev.args[1], 1);
    // Drained.
    assert!(r.take_hook_events().is_empty());
}

/// Snapshot at a mid-run boundary: the parent and a fork both resume
/// bit-identically, and a poked fork diverges — the trajectory-forking
/// primitive.
#[test]
fn snapshot_fork_bit_parity_and_divergence() {
    let words = two_write_words();

    // Reference run: straight through.
    let r_ref = Reactor::new();
    load_words(&r_ref, 0x1000, &words);
    r_ref.write_bytes(0x2000, b"AB");
    let d_ref = drive(&r_ref, 0x1000, None, 100);
    assert!(d_ref.exited);
    assert_eq!(d_ref.out, b"AB");

    // Interrupted run: stop after the first write, snapshot.
    let r = Reactor::new();
    load_words(&r, 0x1000, &words);
    r.write_bytes(0x2000, b"AB");
    let d = drive(&r, 0x1000, Some(1), 100);
    assert_eq!(d.out, b"A", "stopped after the first write");
    assert!(!d.exited);
    let snap = r.snapshot(None, d.pc);

    // Parent continues to completion: identical to the reference.
    let d_parent = drive(&r, d.pc, None, 100);
    assert!(d_parent.exited);
    assert_eq!(d_parent.out, b"B");
    assert_eq!([d.out.clone(), d_parent.out.clone()].concat(), d_ref.out);

    // Fork from the snapshot: resumes bit-identically.
    let fork = Reactor::new();
    fork.restore(&snap).expect("restore fork");
    let d_fork = drive(&fork, snap.pc, None, 100);
    assert!(d_fork.exited);
    assert_eq!(d_fork.out, b"B", "fork resumes bit-identically");

    // A second fork, poked ('B' → 'C'): trajectories diverge.
    let fork2 = Reactor::new();
    fork2.restore(&snap).expect("restore fork2");
    fork2.write_bytes(0x2001, b"C");
    let d_fork2 = drive(&fork2, snap.pc, None, 100);
    assert!(d_fork2.exited);
    assert_eq!(d_fork2.out, b"C", "poked fork diverges");

    // Lineage: a snapshot of the fork names its parent, and diverged state
    // hashes differently.
    let child_snap = fork.snapshot(Some(&snap), d_fork.pc);
    assert_eq!(child_snap.parent, Some(snap.id));
    let fork2_snap = fork2.snapshot(Some(&snap), d_fork2.pc);
    assert_ne!(fork2_snap.id, child_snap.id, "diverged state hashes differently");
}

/// Snapshotting under machine mode (devices attached) round-trips device
/// state: UART RX queued before the snapshot is still queued after restore.
#[test]
fn snapshot_round_trips_device_state() {
    let r = Reactor::new();
    r.set_hart_enabled(true);
    r.set_machine_enabled(true);
    r.with_machine(|m| m.uart.push_rx(b"hello"));
    let snap = r.snapshot(None, 0x1000);
    let fork = Reactor::new();
    // Device topology is host-established: the fork gets a machine before
    // the snapshot's device state is restored into it.
    fork.set_hart_enabled(true);
    fork.set_machine_enabled(true);
    fork.restore(&snap).expect("restore");
    // The forked machine has the queued RX bytes and the same device
    // topology (no virtio devices, no mismatch). Read RBR through the MMIO
    // path: the first queued byte must come back.
    let rbr = fork
        .with_machine(|m| m.load(vane::vane_riscv::machine::UART_BASE, 1, false))
        .expect("machine attached")
        .expect("RBR read");
    assert_eq!(rbr, b'h' as u64, "UART RX state survived the snapshot round-trip");
    assert!(fork.with_hart(|h| h.enabled));
}

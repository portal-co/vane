//! Phase 4 of the aarch64-jit-parity/shared-memory plan: proves the
//! concrete cross-consumer-sharing scenario `vane_arch::Mem::shared`
//! exists for — an "AOT" handle and a separate "JIT" handle, each their own
//! `Mem` instance, backed by clones of the same
//! `os_page::backends::SharedLinearHost`, observe each other's writes
//! immediately. Before this, every `Mem` was a private, per-instance
//! `BTreeMap`-paged store with no sharing primitive at all.

use os_page::backends::{LinearHost, SharedLinearHost};
use vane_arch::Mem;

fn mem_with_shared(shared: SharedLinearHost) -> Mem {
    Mem {
        shared: Some(shared),
        ..Default::default()
    }
}

#[test]
fn aot_and_jit_handles_share_one_memory_instance() {
    let shared = SharedLinearHost::new(LinearHost::new(4096));
    let mut aot = mem_with_shared(shared.clone());
    let mut jit = mem_with_shared(shared.clone());

    // AOT writes a 4-byte word; JIT reads it back byte-by-byte through its
    // own, entirely separate `Mem` instance.
    for (i, b) in 0xDEAD_BEEFu32.to_le_bytes().into_iter().enumerate() {
        aot.write_byte(0x100 + i as u64, b);
    }
    let mut readback = [0u8; 4];
    for (i, slot) in readback.iter_mut().enumerate() {
        *slot = jit.read_byte(0x100 + i as u64);
    }
    assert_eq!(u32::from_le_bytes(readback), 0xDEAD_BEEF);

    // And the reverse direction: JIT writes, AOT reads.
    jit.write_byte(0x200, 0x42);
    assert_eq!(aot.read_byte(0x200), 0x42);
}

#[test]
fn unshared_mem_instances_stay_independent() {
    // Regression guard for the default (`shared: None`) path: two `Mem`s
    // with no shared backing must NOT observe each other's writes — the
    // pre-existing, still-default behavior every other `Mem` consumer
    // relies on.
    let mut a = Mem::default();
    let mut b = Mem::default();
    a.write_byte(0x10, 0xAA);
    assert_eq!(a.read_byte(0x10), 0xAA);
    assert_eq!(b.read_byte(0x10), 0, "independent Mem instances must not share state");
}

#[test]
fn shared_mem_respects_text_hole_across_handles() {
    let shared = SharedLinearHost::new(LinearHost::new(4096).with_text_hole(0x300, 0x400));
    let mut aot = mem_with_shared(shared.clone());
    let mut jit = mem_with_shared(shared.clone());

    // A write into the text hole from either handle is silently dropped
    // (write_byte is infallible — see its doc comment), not observed by
    // the other handle.
    aot.write_byte(0x350, 0x99);
    assert_eq!(jit.read_byte(0x350), 0, "text-hole write must not land through any handle");

    // Outside the hole, sharing still works normally.
    aot.write_byte(0x50, 0x77);
    assert_eq!(jit.read_byte(0x50), 0x77);
}

#[test]
fn jit_ctx_bytes_iterator_observes_shared_writes() {
    use vane_arch::JitCtx;

    let shared = SharedLinearHost::new(LinearHost::new(4096));
    let mut aot = mem_with_shared(shared.clone());
    let jit = mem_with_shared(shared.clone());

    aot.write_byte(0x10, 0x11);
    aot.write_byte(0x11, 0x22);
    aot.write_byte(0x12, 0x33);

    let collected: Vec<u8> = jit.bytes(0x10).take(3).collect();
    assert_eq!(collected, vec![0x11, 0x22, 0x33], "JitCtx::bytes must observe writes made through a different Mem sharing the same backing");
}

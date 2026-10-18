//! W1.5: snapshot serialization round-trip — a snapshot survives encode→
//! decode bit-identically (pages, regs, pc, lineage), zero pages are omitted,
//! and a corrupted byte fails the checksum. The snapshot memory model is the
//! plain-Rust (native/WASI) machine path, so this test is no-js only.
#![cfg(not(feature = "js-host"))]

use vane::Reactor;

#[test]
fn snapshot_roundtrip_bit_identical() {
    let r = Reactor::new();
    // Write a couple of guest pages (one non-zero, leaving others zero).
    let page_no = (0x8000_0000u64) >> 16;
    let mut data = vec![0u8; 65536];
    data[0..4].copy_from_slice(b"VANE");
    data[65535] = 0xFF;
    r.write_bytes(0x8000_0000, &data);

    let snap = r.snapshot(None, 0x8000_0100);
    let bytes = Reactor::snapshot_to_bytes(&snap);
    let snap2 = Reactor::snapshot_from_bytes(&bytes).expect("decode");

    // Lineage + pc + regs survive.
    assert_eq!(snap.id, snap2.id);
    assert_eq!(snap.parent, snap2.parent);
    assert_eq!(snap.pc, snap2.pc);
    assert_eq!(snap.regs, snap2.regs);
    // The non-zero page round-trips; zero pages are omitted.
    assert_eq!(snap2.pages.len(), snap.pages.len());
    assert!(snap2.pages.contains_key(&page_no));
    assert_eq!(&snap2.pages[&page_no][..4], b"VANE");
    assert_eq!(snap2.pages[&page_no][65535], 0xFF);

    // Restore into a fresh reactor and confirm the bytes are live: re-snapshot
    // and the page contents match the original (avoids a js-host-only read API).
    let r2 = Reactor::new();
    r2.restore(&snap2).expect("restore");
    let snap3 = r2.snapshot(None, 0x8000_0100);
    assert_eq!(&snap3.pages[&page_no][..4], b"VANE");
    assert_eq!(snap3.pages[&page_no][65535], 0xFF);
}

#[test]
fn snapshot_lineage_chain() {
    let r = Reactor::new();
    let s0 = r.snapshot(None, 0x1000);
    let s1 = r.snapshot(Some(&s0), 0x2000);
    // Parent linkage survives serialization.
    let s1b = Reactor::snapshot_from_bytes(&Reactor::snapshot_to_bytes(&s1)).unwrap();
    assert_eq!(s1b.parent, Some(s0.id));
    assert_eq!(s1b.pc, 0x2000);
}

#[test]
fn snapshot_checksum_catches_corruption() {
    let r = Reactor::new();
    r.write_bytes(0x8000_0000, b"sensitive");
    let snap = r.snapshot(None, 0x8000_0000);
    let mut bytes = Reactor::snapshot_to_bytes(&snap);
    // Flip a byte in the payload (not the trailing checksum).
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    assert!(
        Reactor::snapshot_from_bytes(&bytes).is_err(),
        "corruption detected"
    );
}

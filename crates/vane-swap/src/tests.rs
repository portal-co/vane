use super::*;
use alloc::boxed::Box;
use alloc::vec;

/// Deterministic nonce source for tests (a counter wrapped into 24 bytes).
/// Real hosts must use a CSPRNG; tests just need uniqueness.
fn counter_nonce() -> Box<dyn FnMut() -> [u8; NONCE_BYTES] + Send> {
    let mut ctr = 0u64;
    Box::new(move || {
        ctr += 1;
        let mut n = [0u8; NONCE_BYTES];
        n[..8].copy_from_slice(&ctr.to_le_bytes());
        n
    })
}

fn cache(cap: usize, slots: u64) -> PageCache {
    PageCache::new(
        cap,
        Box::new(MemSwap::new(slots)),
        [0x42u8; 32],
        counter_nonce(),
    )
}

fn page_with(byte: u8) -> [u8; PAGE_BYTES] {
    [byte; PAGE_BYTES]
}

#[test]
fn crypto_roundtrip() {
    let c = Crypter::new([7u8; 32]);
    let page = page_with(0xAB);
    let mut rec = [0u8; RECORD_BYTES];
    c.seal(3, &[9u8; NONCE_BYTES], &page, &mut rec);
    assert_ne!(&rec[NONCE_BYTES..NONCE_BYTES + PAGE_BYTES], &page[..]);
    let mut out = [0u8; PAGE_BYTES];
    c.open(3, &rec, &mut out).unwrap();
    assert_eq!(out, page);
}

#[test]
fn crypto_tamper_detected() {
    let c = Crypter::new([7u8; 32]);
    let page = page_with(0xAB);
    let mut rec = [0u8; RECORD_BYTES];
    c.seal(3, &[9u8; NONCE_BYTES], &page, &mut rec);
    // Flip a ciphertext byte.
    rec[NONCE_BYTES + 100] ^= 1;
    let mut out = [0u8; PAGE_BYTES];
    assert_eq!(c.open(3, &rec, &mut out), Err(SwapError::Integrity));
}

#[test]
fn crypto_slot_aad_binding() {
    // A record sealed for slot 3 must not verify for slot 4 (relocation
    // attack): the slot index is AAD.
    let c = Crypter::new([7u8; 32]);
    let page = page_with(0xAB);
    let mut rec = [0u8; RECORD_BYTES];
    c.seal(3, &[9u8; NONCE_BYTES], &page, &mut rec);
    let mut out = [0u8; PAGE_BYTES];
    assert_eq!(c.open(4, &rec, &mut out), Err(SwapError::Integrity));
}

#[test]
fn zero_page_detection() {
    assert!(is_zero_page(&[0u8; PAGE_BYTES]));
    let mut p = [0u8; PAGE_BYTES];
    p[12345] = 1;
    assert!(!is_zero_page(&p));
}

#[test]
fn resident_hit_no_fault() {
    let mut c = cache(4, 16);
    c.write_byte(0, 0, 0x55).unwrap();
    let before = c.stats();
    assert_eq!(c.read_byte(0, 0).unwrap(), 0x55);
    // No additional fault on a resident hit.
    assert_eq!(c.stats().faults, before.faults);
}

#[test]
fn eviction_preserves_data() {
    let mut c = cache(2, 16);
    // Write distinct bytes to pages 0,1,2 (cap 2 → evictions).
    c.write_byte(0, 0, 0x11).unwrap();
    c.write_byte(1, 0, 0x22).unwrap();
    c.write_byte(2, 0, 0x33).unwrap();
    // All three must read back correctly through the swap.
    assert_eq!(c.read_byte(0, 0).unwrap(), 0x11);
    assert_eq!(c.read_byte(1, 0).unwrap(), 0x22);
    assert_eq!(c.read_byte(2, 0).unwrap(), 0x33);
    assert!(c.stats().evictions > 0);
}

#[test]
fn dirty_page_written_back_and_decrypted() {
    let mut c = cache(1, 16);
    c.write_byte(0, 500, 0x99).unwrap();
    // Force eviction by touching another page.
    c.write_byte(1, 0, 0x01).unwrap();
    let st = c.stats();
    assert_eq!(st.writebacks, 1, "dirty page must be written to swap");
    assert!(st.swap_bytes_written > 0);
    // Fault page 0 back in; data must survive the encrypt/decrypt round trip.
    assert_eq!(c.read_byte(0, 500).unwrap(), 0x99);
}

#[test]
fn zero_page_not_written_to_swap() {
    let mut c = cache(1, 16);
    // Write then fully clear page 0.
    c.write_byte(0, 0, 0xAA).unwrap();
    c.write_page(0).unwrap().fill(0);
    // Evict by touching page 1.
    c.write_byte(1, 0, 0x01).unwrap();
    let st = c.stats();
    assert_eq!(st.zero_pages, 1, "a zeroed page must not consume swap I/O");
    assert_eq!(st.swap_bytes_written, 0);
    // Fault back: still zero.
    assert_eq!(c.read_byte(0, 0).unwrap(), 0);
}

#[test]
fn fresh_page_is_zero() {
    let mut c = cache(2, 16);
    assert_eq!(c.read_byte(7, 1234).unwrap(), 0);
}

#[test]
fn pinned_page_never_evicted() {
    let mut c = cache(2, 16);
    c.pin(0);
    c.write_byte(0, 0, 0x77).unwrap();
    // Touch many other pages to force evictions; page 0 stays resident.
    for p in 1..6u64 {
        c.write_byte(p, 0, p as u8).unwrap();
    }
    // Page 0 reads back without a new fault (still resident).
    let before = c.stats().faults;
    assert_eq!(c.read_byte(0, 0).unwrap(), 0x77);
    assert_eq!(c.stats().faults, before, "pinned page must not fault");
}

#[test]
fn whole_page_integrity_after_cycles() {
    // Drive more pages than the cap through the cache and verify content.
    let mut c = cache(4, 64);
    let mut expected = BTreeMap::new();
    for p in 0..20u64 {
        let val = (p.wrapping_mul(31).wrapping_add(7)) as u8;
        c.write_page(p).unwrap().fill(val);
        expected.insert(p, val);
    }
    for p in 0..20u64 {
        let frame = c.read_page(p).unwrap();
        let want = expected[&p];
        assert!(
            frame.iter().all(|&b| b == want),
            "page {p} content mismatch after swap cycling"
        );
    }
    let st = c.stats();
    assert!(st.swap_bytes_written > 0 && st.swap_bytes_read > 0);
}

/// S5: tampering with the backing store is detected as an AEAD failure and
/// surfaced as a hard error, never silent corruption.
#[test]
fn s5_backing_store_tamper_detected() {
    let store = Box::new(MemSwap::new(16));
    let mut c = PageCache::new(1, store, [0xABu8; 32], counter_nonce());
    // Write a dirty page and evict it (into swap).
    c.write_page(0).unwrap().fill(0x5A);
    c.write_byte(1, 0, 1).unwrap(); // evicts page 0
    assert_eq!(c.stats().writebacks, 1);

    // Tamper: corrupt the single record that holds page 0 (slot 0).
    // Reach into the store via a fresh read/write through a raw handle.
    let mut rec = [0u8; RECORD_BYTES];
    // MemSwap slot 0 holds page 0's record.
    {
        let s: &mut dyn SwapStore = &mut *c.store;
        s.read_record(0, &mut rec).unwrap();
        rec[NONCE_BYTES] ^= 0xFF; // flip a ciphertext byte
        s.write_record(0, &rec).unwrap();
    }
    // Faulting page 0 back in must detect the tamper.
    let err = c.read_byte(0, 0);
    assert_eq!(err, Err(SwapError::Integrity));
    assert_eq!(c.stats().integrity_failures, 1);
}

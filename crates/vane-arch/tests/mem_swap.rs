//! Mem ↔ vane-swap integration (M5): machine-mode RAM routed through the
//! encrypted PageCache via read_byte/write_byte.
use vane_arch::Mem;

const RAM_BASE: u64 = 0x8000_0000;
const PAGE: u64 = 0x1_0000;

fn counter_nonce() -> Box<dyn FnMut() -> [u8; vane_swap::NONCE_BYTES] + Send> {
    let mut ctr = 0u64;
    Box::new(move || {
        ctr += 1;
        let mut n = [0u8; vane_swap::NONCE_BYTES];
        n[..8].copy_from_slice(&ctr.to_le_bytes());
        n
    })
}

fn mem_with_swap(resident_frames: usize, ram_pages: u64) -> Mem {
    let mut m = Mem::default();
    let cache = vane_swap::PageCache::new(
        resident_frames,
        Box::new(vane_swap::MemSwap::new(ram_pages)),
        [0x11u8; 32],
        counter_nonce(),
    );
    m.swap = Some(spin::Mutex::new(cache));
    m.swap_base = RAM_BASE;
    m.swap_len = ram_pages * PAGE;
    m
}

#[test]
fn swap_region_read_write_roundtrip() {
    // 4 resident frames, 64 RAM pages → forces eviction through the
    // encrypted store.
    let mut m = mem_with_swap(4, 64);
    // Write a distinct marker per page.
    for p in 0..32u64 {
        let addr = RAM_BASE + p * PAGE;
        m.write_byte(addr, (p as u8).wrapping_mul(7).wrapping_add(3));
    }
    assert!(m.take_swap_error().is_none());
    // Read them all back through the swap.
    for p in 0..32u64 {
        let addr = RAM_BASE + p * PAGE;
        let got = m.read_byte(addr);
        let want = (p as u8).wrapping_mul(7).wrapping_add(3);
        assert_eq!(got, want, "page {p} byte mismatch after swap cycling");
    }
    assert!(m.take_swap_error().is_none());
    let st = m.swap.as_ref().unwrap().lock().stats();
    assert!(st.faults > 0 && st.evictions > 0, "expected swap activity: {st:?}");
    assert!(st.swap_bytes_written > 0, "expected writebacks: {st:?}");
}

#[test]
fn non_swap_region_uses_plain_pages() {
    let mut m = mem_with_swap(4, 64);
    // An address below the swap base (e.g. a non-RAM region) must use the
    // plain `pages` store, not the swap layer.
    m.write_byte(0x1000, 0xAB);
    assert_eq!(m.read_byte(0x1000), 0xAB);
    let st = m.swap.as_ref().unwrap().lock().stats();
    assert_eq!(st.faults, 0, "non-RAM access must not touch the swap layer");
}

#[test]
fn swap_active_flag() {
    let m = mem_with_swap(4, 64);
    assert!(m.swap_active());
    let plain = Mem::default();
    assert!(!plain.swap_active());
}

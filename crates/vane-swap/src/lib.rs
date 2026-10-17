//! Encrypted paging of guest RAM to a backing store — PLAN_LINUX_WASM.md §6.
//!
//! Guest physical RAM is split into fixed 64 KiB pages. A bounded resident
//! set ([`PageCache`]) holds the hot pages; cold pages are evicted to a
//! [`SwapStore`] (a host-provided file or buffer), each page record
//! encrypted and authenticated with XChaCha20-Poly1305.
//!
//! Security model (§6.2 item 3):
//! - Cipher: XChaCha20-Poly1305, pure-Rust and `no_std`, identical on every
//!   host (no WebCrypto fast path — one code path everywhere).
//! - Nonce: a fresh 192-bit nonce is drawn from the host CSPRNG on *every*
//!   page write. The nonce travels with the page record, so there is no
//!   generation counter to persist or get wrong.
//! - AAD: the swap slot index is bound as additional authenticated data, so
//!   a record cannot be relocated to a different slot without detection.
//! - Integrity failure on read is a hard error ([`SwapError::Integrity`]) —
//!   never silent zeros (S5).
//! - Zero pages are never written to swap; they are re-materialized on
//!   fault. This is the big win for fresh large guests.
//!
//! The store is *synchronous*: native/WASI files and OPFS sync access
//! handles are all synchronous. The async generalization needed only for the
//! IndexedDB browser fallback is an M6 concern (see PLAN §6.2 item 1).

#![no_std]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{Key, XNonce};

#[cfg(feature = "std")]
mod file;
#[cfg(feature = "std")]
pub use file::FileSwap;

/// Bytes per guest page (matches `vane_arch::Mem`'s 64 KiB pages).
pub const PAGE_BYTES: usize = 65536;
/// XChaCha20 nonce size (192 bits).
pub const NONCE_BYTES: usize = 24;
/// Poly1305 tag size.
pub const TAG_BYTES: usize = 16;
/// On-disk bytes per swapped page: `[ nonce | ciphertext | tag ]`.
pub const RECORD_BYTES: usize = NONCE_BYTES + PAGE_BYTES + TAG_BYTES;

/// Errors surfaced by the swap layer. `Integrity` is the S5 tamper-detection
/// error; it must propagate to the guest driver as a hard failure, never be
/// swallowed into a zero page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapError {
    /// The backing store failed an I/O operation.
    Store,
    /// AEAD tag verification failed — the backing store was tampered with
    /// (or a nonce/slot mismatch). Hard error (S5).
    Integrity,
    /// No swap slots left (backing store capacity exceeded).
    OutOfSlots,
    /// Every resident frame is pinned; nothing can be evicted to make room.
    AllPinned,
}

/// A backing store for encrypted page records, addressed by slot index.
/// Synchronous (see module docs). Implementations must be byte-exact:
/// `write_record` then `read_record` of the same slot returns the same bytes.
pub trait SwapStore {
    fn read_record(&mut self, slot: u64, buf: &mut [u8; RECORD_BYTES]) -> Result<(), SwapError>;
    fn write_record(&mut self, slot: u64, buf: &[u8; RECORD_BYTES]) -> Result<(), SwapError>;
    /// Total number of page slots the store can hold.
    fn slot_count(&self) -> u64;
}

/// In-memory swap store, for tests and as a null backend.
pub struct MemSwap {
    slots: Vec<Option<Box<[u8; RECORD_BYTES]>>>,
    cap: u64,
}

impl MemSwap {
    pub fn new(cap: u64) -> Self {
        MemSwap {
            slots: Vec::new(),
            cap,
        }
    }
}

impl SwapStore for MemSwap {
    fn read_record(&mut self, slot: u64, buf: &mut [u8; RECORD_BYTES]) -> Result<(), SwapError> {
        match self.slots.get(slot as usize).and_then(|s| s.as_ref()) {
            Some(rec) => {
                buf.copy_from_slice(&rec[..]);
                Ok(())
            }
            None => Err(SwapError::Store),
        }
    }
    fn write_record(&mut self, slot: u64, buf: &[u8; RECORD_BYTES]) -> Result<(), SwapError> {
        if slot >= self.cap {
            return Err(SwapError::OutOfSlots);
        }
        let idx = slot as usize;
        if idx >= self.slots.len() {
            self.slots.resize_with(idx + 1, || None);
        }
        self.slots[idx] = Some(Box::new(*buf));
        Ok(())
    }
    fn slot_count(&self) -> u64 {
        self.cap
    }
}

/// XChaCha20-Poly1305 page crypter. Cloning is cheap (the cipher is a small
/// key schedule); there is no per-instance mutable state.
pub struct Crypter {
    cipher: chacha20poly1305::XChaCha20Poly1305,
}

impl Crypter {
    pub fn new(key: [u8; 32]) -> Self {
        Crypter {
            cipher: chacha20poly1305::XChaCha20Poly1305::new(Key::from_slice(&key)),
        }
    }

    /// Encrypt `page` into `out` as `[ nonce | ciphertext | tag ]`, binding
    /// `slot` as AAD. `nonce` must be fresh-random per write (the caller
    /// draws it from the host CSPRNG).
    pub fn seal(
        &self,
        slot: u64,
        nonce: &[u8; NONCE_BYTES],
        page: &[u8; PAGE_BYTES],
        out: &mut [u8; RECORD_BYTES],
    ) {
        out[..NONCE_BYTES].copy_from_slice(nonce);
        out[NONCE_BYTES..NONCE_BYTES + PAGE_BYTES].copy_from_slice(page);
        let aad = slot.to_le_bytes();
        let tag = self
            .cipher
            .encrypt_in_place_detached(
                XNonce::from_slice(nonce),
                &aad,
                &mut out[NONCE_BYTES..NONCE_BYTES + PAGE_BYTES],
            )
            .expect("XChaCha20-Poly1305 seal is infallible for in-place buffers");
        out[NONCE_BYTES + PAGE_BYTES..].copy_from_slice(&tag);
    }

    /// Verify the tag (with `slot` as AAD) and decrypt into `out`. Returns
    /// `SwapError::Integrity` on any tampering.
    pub fn open(
        &self,
        slot: u64,
        record: &[u8; RECORD_BYTES],
        out: &mut [u8; PAGE_BYTES],
    ) -> Result<(), SwapError> {
        let nonce = &record[..NONCE_BYTES];
        let tag = &record[NONCE_BYTES + PAGE_BYTES..];
        out.copy_from_slice(&record[NONCE_BYTES..NONCE_BYTES + PAGE_BYTES]);
        let aad = slot.to_le_bytes();
        self.cipher
            .decrypt_in_place_detached(XNonce::from_slice(nonce), &aad, out, tag.into())
            .map_err(|_| SwapError::Integrity)
    }
}

/// True if the page is entirely zero (never written / fully cleared). Such
/// pages are re-materialized on fault instead of consuming a swap slot.
pub fn is_zero_page(page: &[u8; PAGE_BYTES]) -> bool {
    // Word-at-a-time for speed on the hot evict path.
    page.chunks_exact(8)
        .all(|c| c == [0u8; 8] || u64::from_le_bytes(c.try_into().unwrap()) == 0)
}

/// Where a non-resident page lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Location {
    /// All-zero page; re-materialize on fault, no swap slot consumed.
    Zero,
    /// Encrypted record at this swap slot.
    Swapped(u64),
}

/// Accounting for the swap layer, surfaced to the host (§6.2 item 5, S4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemStats {
    /// Frames currently resident.
    pub resident: u64,
    /// Resident cap (frames).
    pub cap: u64,
    /// Page faults serviced (page was not resident).
    pub faults: u64,
    /// Pages evicted from the resident set.
    pub evictions: u64,
    /// Dirty pages written back to swap on eviction.
    pub writebacks: u64,
    /// Bytes read from the swap store (decrypted records).
    pub swap_bytes_read: u64,
    /// Bytes written to the swap store (encrypted records).
    pub swap_bytes_written: u64,
    /// Evictions that were pure zero pages (no swap I/O).
    pub zero_pages: u64,
    /// AEAD integrity failures detected on read (S5).
    pub integrity_failures: u64,
}

/// The resident page set with clock (second-chance) eviction, dirty tracking,
/// a pinned set, and zero-page elision, over an encrypted [`SwapStore`].
///
/// All methods are synchronous (the store is synchronous — see module docs).
/// This is a pure data structure: it does no guest-address translation and
/// knows nothing about the interpreter; the caller maps guest physical
/// addresses to page numbers.
pub struct PageCache {
    frames: Vec<Box<[u8; PAGE_BYTES]>>,
    /// page_no -> frame index for resident pages.
    resident: BTreeMap<u64, usize>,
    /// frame index -> page_no.
    frame_page: Vec<u64>,
    /// second-chance bit per frame.
    referenced: Vec<bool>,
    /// dirty bit per frame.
    dirty: Vec<bool>,
    /// clock hand for eviction.
    hand: usize,
    /// resident cap in frames.
    cap: usize,
    /// page numbers that must never be evicted (kernel image, DTB, page
    /// tables, swap metadata — §6.2 item 2 / §7.4).
    pinned: BTreeSet<u64>,
    /// page_no -> location when not resident.
    location: BTreeMap<u64, Location>,
    /// reusable swap slots freed by zero-page transitions.
    free_slots: Vec<u64>,
    next_slot: u64,
    store: Box<dyn SwapStore + Send>,
    crypter: Crypter,
    nonce_src: Box<dyn FnMut() -> [u8; NONCE_BYTES] + Send>,
    stats: MemStats,
    /// Last swap error (I/O or integrity), for the host to surface (S5).
    error: Option<SwapError>,
}

impl PageCache {
    /// `cap` is the resident-set cap in frames. `nonce_src` is the host
    /// CSPRNG hook (e.g. `getrandom`). The key must come from the host
    /// CSPRNG and never touch disk unencrypted (§6.2 item 3).
    pub fn new(
        cap: usize,
        store: Box<dyn SwapStore + Send>,
        key: [u8; 32],
        nonce_src: Box<dyn FnMut() -> [u8; NONCE_BYTES] + Send>,
    ) -> Self {
        assert!(cap > 0, "resident cap must be at least one frame");
        PageCache {
            frames: Vec::new(),
            resident: BTreeMap::new(),
            frame_page: Vec::new(),
            referenced: Vec::new(),
            dirty: Vec::new(),
            hand: 0,
            cap,
            pinned: BTreeSet::new(),
            location: BTreeMap::new(),
            free_slots: Vec::new(),
            next_slot: 0,
            store,
            crypter: Crypter::new(key),
            nonce_src,
            stats: MemStats {
                cap: cap as u64,
                ..Default::default()
            },
            error: None,
        }
    }

    pub fn stats(&self) -> MemStats {
        let mut s = self.stats;
        s.resident = self.frames.len() as u64;
        s
    }

    /// Record a swap error (called by the memory layer when an access fails).
    pub fn record_error(&mut self, e: SwapError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    /// Take the recorded swap error, if any. The host surfaces this as a hard
    /// VM error (S5 — never silent corruption).
    pub fn take_error(&mut self) -> Option<SwapError> {
        self.error.take()
    }

    /// Pin a page so it is never evicted (kernel image, DTB, page tables).
    pub fn pin(&mut self, page_no: u64) {
        self.pinned.insert(page_no);
    }
    pub fn unpin(&mut self, page_no: u64) {
        self.pinned.remove(&page_no);
    }
    pub fn is_pinned(&self, page_no: u64) -> bool {
        self.pinned.contains(&page_no)
    }

    /// Read the page, faulting it in (and evicting a victim if the resident
    /// set is full) as needed. Sets the frame's second-chance bit.
    pub fn read_page(&mut self, page_no: u64) -> Result<&[u8; PAGE_BYTES], SwapError> {
        let f = self.ensure_resident(page_no)?;
        Ok(&self.frames[f])
    }

    /// Like [`read_page`](Self::read_page) but also marks the frame dirty.
    pub fn write_page(&mut self, page_no: u64) -> Result<&mut [u8; PAGE_BYTES], SwapError> {
        let f = self.ensure_resident(page_no)?;
        self.dirty[f] = true;
        Ok(&mut self.frames[f])
    }

    /// Read one byte (convenience over `read_page`).
    pub fn read_byte(&mut self, page_no: u64, off: usize) -> Result<u8, SwapError> {
        Ok(self.read_page(page_no)?[off])
    }
    /// Write one byte (convenience over `write_page`); marks the frame dirty.
    pub fn write_byte(&mut self, page_no: u64, off: usize, v: u8) -> Result<(), SwapError> {
        self.write_page(page_no)?[off] = v;
        Ok(())
    }

    /// Ensure `page_no` is resident, returning its frame index.
    fn ensure_resident(&mut self, page_no: u64) -> Result<usize, SwapError> {
        if let Some(&f) = self.resident.get(&page_no) {
            self.referenced[f] = true;
            return Ok(f);
        }
        self.stats.faults += 1;
        let f = self.free_frame()?;
        self.fault_in(page_no, f)?;
        self.resident.insert(page_no, f);
        self.frame_page[f] = page_no;
        self.referenced[f] = true;
        self.dirty[f] = false;
        Ok(f)
    }

    /// Return a free frame index, growing the pool or evicting a victim.
    fn free_frame(&mut self) -> Result<usize, SwapError> {
        if self.frames.len() < self.cap {
            self.frames.push(Box::new([0u8; PAGE_BYTES]));
            self.frame_page.push(u64::MAX);
            self.referenced.push(false);
            self.dirty.push(false);
            return Ok(self.frames.len() - 1);
        }
        // Clock / second-chance eviction.
        let n = self.frames.len();
        let mut scanned = 0;
        loop {
            let f = self.hand;
            self.hand = (self.hand + 1) % n;
            scanned += 1;
            let p = self.frame_page[f];
            if self.pinned.contains(&p) {
                // Pinned: skip without touching its referenced bit.
            } else if self.referenced[f] {
                self.referenced[f] = false; // second chance
            } else {
                // Victim found.
                if scanned > n.saturating_mul(2) && self.all_others_pinned(f) {
                    return Err(SwapError::AllPinned);
                }
                self.evict(f)?;
                return Ok(f);
            }
            if scanned > n.saturating_mul(2) && self.all_pinned() {
                return Err(SwapError::AllPinned);
            }
        }
    }

    fn all_pinned(&self) -> bool {
        self.frame_page.iter().all(|&p| self.pinned.contains(&p))
    }
    fn all_others_pinned(&self, except: usize) -> bool {
        self.frame_page
            .iter()
            .enumerate()
            .all(|(i, &p)| i == except || self.pinned.contains(&p))
    }

    /// Evict frame `f`'s page to swap (if dirty) or drop it (if clean /
    /// zero). Marks the frame free.
    fn evict(&mut self, f: usize) -> Result<(), SwapError> {
        let p = self.frame_page[f];
        self.resident.remove(&p);
        self.stats.evictions += 1;
        let dirty = self.dirty[f];
        let prev = self.location.get(&p).copied();
        if !dirty {
            // Unchanged since fault-in: keep its existing location (a
            // previously-swapped page's slot still holds valid content).
            if prev.is_none() {
                self.location.insert(p, Location::Zero);
            }
            self.dirty[f] = false;
            self.referenced[f] = false;
            return Ok(());
        }
        // Dirty: write back unless it's a zero page.
        if is_zero_page(&self.frames[f]) {
            if let Some(Location::Swapped(slot)) = prev {
                self.free_slots.push(slot);
            }
            self.location.insert(p, Location::Zero);
            self.stats.zero_pages += 1;
        } else {
            let slot = match prev {
                Some(Location::Swapped(s)) => s, // reuse the slot
                _ => self.alloc_slot()?,
            };
            let nonce = (self.nonce_src)();
            let mut rec = Box::new([0u8; RECORD_BYTES]);
            self.crypter.seal(slot, &nonce, &self.frames[f], &mut rec);
            self.store.write_record(slot, &rec)?;
            self.location.insert(p, Location::Swapped(slot));
            self.stats.writebacks += 1;
            self.stats.swap_bytes_written += RECORD_BYTES as u64;
        }
        self.dirty[f] = false;
        self.referenced[f] = false;
        Ok(())
    }

    /// Load page `page_no` into frame `f` (decrypt from swap, or zero it).
    fn fault_in(&mut self, page_no: u64, f: usize) -> Result<(), SwapError> {
        match self.location.get(&page_no).copied() {
            Some(Location::Swapped(slot)) => {
                let mut rec = Box::new([0u8; RECORD_BYTES]);
                self.store.read_record(slot, &mut rec)?;
                match self.crypter.open(slot, &rec, &mut self.frames[f]) {
                    Ok(()) => {}
                    Err(e) => {
                        if e == SwapError::Integrity {
                            self.stats.integrity_failures += 1;
                        }
                        return Err(e);
                    }
                }
                self.stats.swap_bytes_read += RECORD_BYTES as u64;
            }
            Some(Location::Zero) | None => {
                self.frames[f].fill(0);
            }
        }
        Ok(())
    }

    fn alloc_slot(&mut self) -> Result<u64, SwapError> {
        if let Some(s) = self.free_slots.pop() {
            return Ok(s);
        }
        let s = self.next_slot;
        if s >= self.store.slot_count() {
            return Err(SwapError::OutOfSlots);
        }
        self.next_slot += 1;
        Ok(s)
    }
}

#[cfg(test)]
mod tests;

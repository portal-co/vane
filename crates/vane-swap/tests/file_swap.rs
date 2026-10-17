//! S5 with the real file backend: encrypted pages on disk are ciphertext;
//! tampering with any byte of the backing file is detected as an AEAD tag
//! failure and surfaced as a hard error (never silent corruption).
#![cfg(feature = "std")]

use std::io::{Read, Seek, SeekFrom, Write};

use vane_swap::{FileSwap, PageCache, SwapError, NONCE_BYTES, PAGE_BYTES, RECORD_BYTES};

fn counter_nonce() -> Box<dyn FnMut() -> [u8; NONCE_BYTES] + Send> {
    let mut ctr = 0u64;
    Box::new(move || {
        ctr += 1;
        let mut n = [0u8; NONCE_BYTES];
        n[..8].copy_from_slice(&ctr.to_le_bytes());
        n
    })
}

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "vane-swap-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn file_swap_records_are_ciphertext() {
    let dir = tmpdir();
    let path = dir.join("swap.bin");
    let slots = 16u64;

    let store = FileSwap::create(&path, slots).unwrap();
    let mut cache = PageCache::new(1, Box::new(store), [0x5Au8; 32], counter_nonce());
    // Write a uniform (highly compressible / obviously-plaintext) page and
    // evict it to the file.
    cache.write_page(0).unwrap().fill(0xA1);
    cache.write_page(1).unwrap().fill(0); // evicts page 0
    assert!(cache.stats().writebacks >= 1);

    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(on_disk.len(), (slots as usize) * RECORD_BYTES);
    // Examine only the WRITTEN record (slot 0 holds page 0). A plaintext
    // 0xA1-filled page would be a 65536-byte run of 0xA1; the encrypted
    // record must have no long single-byte run. (Unused slots are zeros —
    // sparse space, not ciphertext — and are excluded.)
    let record = &on_disk[..RECORD_BYTES];
    let mut max_run = 0usize;
    let mut run = 1usize;
    for w in record.windows(2) {
        if w[0] == w[1] {
            run += 1;
            max_run = max_run.max(run);
        } else {
            run = 1;
        }
    }
    assert!(
        max_run < 64,
        "swap record has a {max_run}-byte plaintext run — not encrypted"
    );
    // And the record's ciphertext body must not be all-zero (it isn't the
    // plaintext 0xA1, and it isn't untouched sparse space).
    let body = &record[NONCE_BYTES..NONCE_BYTES + PAGE_BYTES];
    assert!(
        body.iter().any(|&b| b != 0),
        "record body is unexpectedly zero"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_swap_tamper_detected() {
    let dir = tmpdir();
    let path = dir.join("swap.bin");
    let slots = 16u64;

    let store = FileSwap::create(&path, slots).unwrap();
    let mut cache = PageCache::new(1, Box::new(store), [0x5Au8; 32], counter_nonce());
    // Page 0 dirty; page 1 write evicts page 0 into slot 0.
    cache.write_page(0).unwrap().fill(0x77);
    cache.write_page(1).unwrap().fill(0x11);
    assert!(cache.stats().writebacks >= 1);

    // Corrupt slot 0's ciphertext via a SEPARATE handle to the same file.
    {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut rec = [0u8; RECORD_BYTES];
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut rec).unwrap();
        rec[NONCE_BYTES + 10] ^= 0xFF; // flip a ciphertext byte
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&rec).unwrap();
        f.sync_all().unwrap();
    }

    // Fault page 0 back in (evicting page 1): decrypting the tampered record
    // must fail with an integrity error, not return corrupted data.
    let r = cache.read_page(0);
    assert_eq!(r.map(|_| ()).unwrap_err(), SwapError::Integrity);
    assert_eq!(cache.stats().integrity_failures, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

//! virtio-blk block device (PLAN_VIRTIO_WEVAL workstream B / milestone V2).
//!
//! A single-virtqueue block device backed by a pluggable [`BlockBackend`]:
//! a host file natively (and under WASI via a preopened file), an OPFS
//! sync-access-handle in the browser, or a copy-on-write layer over a
//! read-only base so derived machines/snapshots don't mutate the base image.

use crate::virtio::{DEV_BLK, DescriptorChain, QueueMem, VirtioDevice};

/// Sector size (virtio-blk is always 512-byte sectors on the wire).
pub const SECTOR: u64 = 512;

// Request types (virtio spec, block device).
const T_IN: u32 = 0; // read
const T_OUT: u32 = 1; // write
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

// Status bytes.
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// A pluggable block-store backend for virtio-blk. Sector-granular; the
/// device handles the descriptor/queue machinery.
pub trait BlockBackend {
    /// Total capacity in 512-byte sectors.
    fn capacity_sectors(&self) -> u64;
    /// Read `buf.len()` bytes starting at byte offset `lba * SECTOR`.
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]);
    /// Write `buf` at byte offset `lba * SECTOR`.
    fn write_sectors(&mut self, lba: u64, buf: &[u8]);
    /// Flush buffered writes. Default: nothing (write-through).
    fn flush(&mut self) {}
}

/// A virtio-blk device over a [`BlockBackend`].
pub struct VirtioBlk<B: BlockBackend> {
    backend: B,
}

impl<B: BlockBackend> VirtioBlk<B> {
    pub fn new(backend: B) -> Self {
        VirtioBlk { backend }
    }
}

impl<B: BlockBackend> VirtioDevice for VirtioBlk<B> {
    fn device_id(&self) -> u32 {
        DEV_BLK
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn features(&self) -> u32 {
        0 // no flush/barrier/geometry feature bits advertised for now
    }

    fn read_config(&self, offset: u64, buf: &mut [u8]) {
        // virtio_blk_config: capacity (u64) @ 0, blk_size (u32) @ 20.
        let capacity = self.backend.capacity_sectors();
        for (i, b) in buf.iter_mut().enumerate() {
            let o = offset + i as u64;
            *b = if o < 8 {
                (capacity >> (8 * o)) as u8
            } else if (20..24).contains(&o) {
                (SECTOR >> (8 * (o - 20))) as u8
            } else {
                0
            };
        }
    }

    fn process(&mut self, _queue: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32 {
        // The request header is the first device-readable buffer:
        // { type u32, ioprio u32, sector u64 }.
        let Some(&(haddr, hlen)) = chain.readable.first() else {
            return 0;
        };
        if hlen < 16 {
            return 0;
        }
        let req_type = mem.read_u32(haddr);
        let sector = mem.read_u64(haddr + 8);

        // The status byte is the last device-writable buffer (1 byte).
        let Some(&(saddr, _)) = chain.writable.last() else {
            return 0;
        };
        // Data buffers are the writable buffers before the status (for a
        // read) or the readable buffers after the header (for a write).
        let data_writable = &chain.writable[..chain.writable.len() - 1];

        match req_type {
            T_IN => {
                // Read `sector`.. into the writable data buffers.
                let mut off = sector * SECTOR;
                let mut written = 0u32;
                for &(addr, len) in data_writable {
                    let mut buf = alloc::vec![0u8; len as usize];
                    self.backend.read_sectors(off / SECTOR, &mut buf);
                    mem.write(addr, &buf);
                    off += len as u64;
                    written += len;
                }
                mem.write(saddr, &[S_OK]);
                written + 1
            }
            T_OUT => {
                // Write the readable data buffers (after the header) to disk.
                let mut off = sector * SECTOR;
                for &(addr, len) in &chain.readable[1..] {
                    let mut buf = alloc::vec![0u8; len as usize];
                    mem.read(addr, &mut buf);
                    self.backend.write_sectors(off / SECTOR, &buf);
                    off += len as u64;
                }
                mem.write(saddr, &[S_OK]);
                1
            }
            T_FLUSH => {
                self.backend.flush();
                mem.write(saddr, &[S_OK]);
                1
            }
            T_GET_ID => {
                let id = b"vane-virtio-blk";
                let n = chain.write_response(mem, id);
                // write_response writes into writable[0..]; but status is the
                // last writable. Overwrite the status byte.
                mem.write(saddr, &[S_OK]);
                n + 1
            }
            _ => {
                mem.write(saddr, &[S_UNSUPP]);
                1
            }
        }
    }
}

/// A copy-on-write layer over a read-only base: reads fall through to the
/// base; writes go to an in-memory overlay so the base is never mutated.
/// This is what lets derived snapshots share a base disk image (PLAN §B).
pub struct CowBlock<B: BlockBackend> {
    base: B,
    overlay: alloc::collections::BTreeMap<u64, alloc::vec::Vec<u8>>,
}

impl<B: BlockBackend> CowBlock<B> {
    pub fn new(base: B) -> Self {
        CowBlock {
            base,
            overlay: alloc::collections::BTreeMap::new(),
        }
    }
}

impl<B: BlockBackend> BlockBackend for CowBlock<B> {
    fn capacity_sectors(&self) -> u64 {
        self.base.capacity_sectors()
    }
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) {
        // Read from base, then apply overlay sectors.
        self.base.read_sectors(lba, buf);
        let start = lba * SECTOR;
        for (i, b) in buf.iter_mut().enumerate() {
            let sec = (start + i as u64) / SECTOR;
            if let Some(o) = self.overlay.get(&sec) {
                *b = o[((start + i as u64) % SECTOR) as usize];
            }
        }
    }
    fn write_sectors(&mut self, lba: u64, buf: &[u8]) {
        // Sector-granular overlay: buffer partial-sector writes.
        let mut off = lba * SECTOR;
        let mut i = 0usize;
        while i < buf.len() {
            let sec = off / SECTOR;
            let sec_off = (off % SECTOR) as usize;
            let n = core::cmp::min(SECTOR as usize - sec_off, buf.len() - i);
            let entry = self.overlay.entry(sec).or_insert_with(|| {
                let mut s = alloc::vec![0u8; SECTOR as usize];
                self.base.read_sectors(sec, &mut s);
                s
            });
            entry[sec_off..sec_off + n].copy_from_slice(&buf[i..i + n]);
            off += n as u64;
            i += n;
        }
    }
}

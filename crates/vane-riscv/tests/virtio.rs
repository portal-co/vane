//! virtio-mmio framework (V1): register block reads, the legacy queue-setup
//! handshake, descriptor-chain walking, used-ring completion, and interrupt.
use std::collections::BTreeMap;
use vane_riscv::virtio::{
    DEV_BLK, DescriptorChain, MAGIC, QueueMem, VERSION_LEGACY, VirtioDevice, VirtioMmio,
};

/// Sparse byte-addressable guest memory for the test.
struct TestMem(BTreeMap<u64, u8>);
impl QueueMem for TestMem {
    fn read(&mut self, addr: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.0.get(&(addr + i as u64)).copied().unwrap_or(0);
        }
    }
    fn write(&mut self, addr: u64, buf: &[u8]) {
        for (i, b) in buf.iter().enumerate() {
            self.0.insert(addr + i as u64, *b);
        }
    }
}
impl TestMem {
    fn put(&mut self, addr: u64, data: &[u8]) {
        self.write(addr, data);
    }
    fn get(&mut self, addr: u64, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        self.read(addr, &mut b);
        b
    }
}

/// A device that reverses the request bytes into the response.
struct Reverse;
impl VirtioDevice for Reverse {
    fn device_id(&self) -> u32 {
        DEV_BLK
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn read_config(&self, _o: u64, _b: &mut [u8]) {}
    fn process(&mut self, _q: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32 {
        let mut req = chain.read_request(mem);
        req.reverse();
        chain.write_response(mem, &req)
    }
}

const PAGE: u64 = 4096;

#[test]
fn register_block() {
    let mut d = VirtioMmio::new(Box::new(Reverse), 1);
    assert_eq!(d.read(0x000), MAGIC);
    assert_eq!(d.read(0x004), VERSION_LEGACY);
    assert_eq!(d.read(0x008), DEV_BLK);
    assert_eq!(d.read(0x00c), 0x76616E65); // "vane"
    assert_eq!(d.read(0x34), 128); // QueueNumMax
    assert!(!d.interrupt_pending());
}

#[test]
fn queue_roundtrip() {
    let mut d = VirtioMmio::new(Box::new(Reverse), 1);
    let mut mem = TestMem(BTreeMap::new());

    // Legacy queue-setup handshake.
    d.write(0x28, PAGE as u32); // GuestPageSize
    d.write(0x30, 0); // QueueSel = 0
    assert_eq!(d.read(0x34), 128); // QueueNumMax
    d.write(0x38, 4); // QueueNum = 4
    d.write(0x3c, PAGE as u32); // QueueAlign
    d.write(0x40, 1); // QueuePFN = 1 -> base 4096

    // Queue layout: desc @ 4096, avail @ 4096+64=4160, used @ 8192.
    let desc = 4096u64;
    let avail = 4160u64;
    let used = 8192u64;
    // Descriptor 0: readable request buffer @ 0x10000, 4 bytes, next=1.
    mem.put(desc, &0x10000u64.to_le_bytes());
    mem.put(desc + 8, &4u32.to_le_bytes());
    mem.put(desc + 12, &1u16.to_le_bytes()); // NEXT
    mem.put(desc + 14, &1u16.to_le_bytes());
    // Descriptor 1: writable response buffer @ 0x10100, 4 bytes.
    mem.put(desc + 16, &0x10100u64.to_le_bytes());
    mem.put(desc + 24, &4u32.to_le_bytes());
    mem.put(desc + 28, &2u16.to_le_bytes()); // WRITE
    // Available ring: idx=1, ring[0]=head 0.
    mem.put(avail + 2, &1u16.to_le_bytes());
    mem.put(avail + 4, &0u16.to_le_bytes());
    // Request data.
    mem.put(0x10000, b"ABCD");

    // Notify and process.
    d.write(0x050, 0); // QueueNotify queue 0
    assert!(d.needs_processing());
    let raised = d.process(&mut mem);
    assert!(raised);
    assert!(d.interrupt_pending());
    assert_eq!(d.read(0x060) & 1, 1); // InterruptStatus: used buffer

    // Response is the reversed request.
    assert_eq!(mem.get(0x10100, 4), b"DCBA");
    // Used ring: ring[0] = {id 0, len 4}, used idx = 1.
    assert_eq!(mem.get(used + 4, 4), 0u32.to_le_bytes());
    assert_eq!(mem.get(used + 8, 4), 4u32.to_le_bytes());
    assert_eq!(mem.get(used + 2, 2), 1u16.to_le_bytes());

    // ACK clears the interrupt line.
    d.write(0x064, 1);
    assert!(!d.interrupt_pending());
}

// ── virtio-blk (V2) ──────────────────────────────────────────────────────────
use vane_riscv::virtio_blk::{BlockBackend, SECTOR, VirtioBlk};

/// In-memory block backend for the blk test.
struct MemBlk {
    sectors: BTreeMap<u64, Vec<u8>>,
    capacity: u64,
}
impl BlockBackend for MemBlk {
    fn capacity_sectors(&self) -> u64 {
        self.capacity
    }
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            let sec = (lba * SECTOR + i as u64) / SECTOR;
            *b = self
                .sectors
                .get(&sec)
                .map(|s| s[((lba * SECTOR + i as u64) % SECTOR) as usize])
                .unwrap_or(0);
        }
    }
    fn write_sectors(&mut self, lba: u64, buf: &[u8]) {
        let mut off = lba * SECTOR;
        let mut i = 0;
        while i < buf.len() {
            let sec = off / SECTOR;
            let so = (off % SECTOR) as usize;
            let n = (SECTOR as usize - so).min(buf.len() - i);
            let e = self
                .sectors
                .entry(sec)
                .or_insert_with(|| vec![0u8; SECTOR as usize]);
            e[so..so + n].copy_from_slice(&buf[i..i + n]);
            off += n as u64;
            i += n;
        }
    }
}

/// Build a blk request chain in guest memory: header @ hdr, data @ data,
/// status @ stat. Sets up the queue and avail ring; returns after process.
fn blk_request(
    d: &mut VirtioMmio,
    mem: &mut TestMem,
    req_type: u32,
    sector: u64,
    data: &[u8],
) -> (u8, Vec<u8>) {
    // Reset, then queue setup (reset clears the queue's last_avail/used_idx
    // so each request starts fresh; the device backend keeps its data).
    d.write(0x070, 0); // Status = 0 (reset)
    d.write(0x28, PAGE as u32);
    d.write(0x30, 0);
    d.write(0x38, 4);
    d.write(0x3c, PAGE as u32);
    d.write(0x40, 1);
    let desc = 4096u64;
    let avail = 4160u64;
    let used = 8192u64;
    let hdr = 0x20000u64;
    let dat = 0x20100u64;
    let stat = 0x20400u64;
    // Header.
    mem.put(hdr, &req_type.to_le_bytes());
    mem.put(hdr + 4, &0u32.to_le_bytes()); // ioprio
    mem.put(hdr + 8, &sector.to_le_bytes());
    // Descriptors: 0=header(readable), 1=data, 2=status(writable).
    let data_is_read = req_type == 0; // read: data is device-writable
    mem.put(desc, &hdr.to_le_bytes());
    mem.put(desc + 8, &16u32.to_le_bytes());
    mem.put(desc + 12, &1u16.to_le_bytes()); // NEXT
    mem.put(desc + 14, &1u16.to_le_bytes());
    mem.put(desc + 16, &dat.to_le_bytes());
    mem.put(
        desc + 24,
        &(data.len() as u32).max(SECTOR as u32).to_le_bytes(),
    );
    mem.put(
        desc + 28,
        &if data_is_read { 3u16 } else { 1u16 }.to_le_bytes(),
    ); // NEXT|WRITE or NEXT
    mem.put(desc + 30, &2u16.to_le_bytes());
    mem.put(desc + 32, &stat.to_le_bytes());
    mem.put(desc + 40, &1u32.to_le_bytes());
    mem.put(desc + 44, &2u16.to_le_bytes()); // WRITE
    // For a write, stage the data.
    if !data_is_read {
        mem.put(dat, data);
    }
    // Avail ring.
    mem.put(avail + 2, &1u16.to_le_bytes());
    mem.put(avail + 4, &0u16.to_le_bytes());
    d.write(0x050, 0);
    d.process(mem);
    let status = mem.get(stat, 1)[0];
    let out = if data_is_read {
        mem.get(dat, SECTOR as usize)
    } else {
        vec![]
    };
    let _ = used;
    (status, out)
}

#[test]
fn blk_write_then_read() {
    let blk = MemBlk {
        sectors: BTreeMap::new(),
        capacity: 16,
    };
    let mut d = VirtioMmio::new(Box::new(VirtioBlk::new(blk)), 1);
    let mut mem = TestMem(BTreeMap::new());
    // Config: capacity @ 0.
    assert_eq!(d.read(0x100), 16u32); // low 32 of capacity
    // Write a sector of 0xAA at sector 2.
    let (ws, _) = blk_request(&mut d, &mut mem, 1, 2, &vec![0xAAu8; SECTOR as usize]);
    assert_eq!(ws, 0, "write status OK");
    // Read it back.
    let (rs, out) = blk_request(&mut d, &mut mem, 0, 2, &[]);
    assert_eq!(rs, 0, "read status OK");
    assert!(out.iter().all(|&b| b == 0xAA), "read back 0xAA sector");
}

// ── virtio-9p (V3) ───────────────────────────────────────────────────────────
mod ninep {
    use super::*;
    use std::collections::BTreeMap;
    use vane_riscv::virtio::VirtioMmio;
    use vane_riscv::virtio_9p::{Host9p, Meta9p, Virtio9p};

    /// In-memory host fs: path -> (is_dir, data).
    struct MemFs {
        files: BTreeMap<String, Vec<u8>>,
    }
    impl MemFs {
        fn new() -> Self {
            let mut files = BTreeMap::new();
            files.insert("hello.txt".to_string(), b"hello, 9p".to_vec());
            MemFs { files }
        }
    }
    impl Host9p for MemFs {
        fn stat(&mut self, path: &str) -> Result<Meta9p, i32> {
            if path.is_empty() {
                return Ok(Meta9p { mode: 0o755, size: 0, is_dir: true });
            }
            match self.files.get(path) {
                Some(d) => Ok(Meta9p { mode: 0o644, size: d.len() as u64, is_dir: false }),
                None => Err(2), // ENOENT
            }
        }
        fn open(&mut self, path: &str, _flags: u32) -> Result<u64, i32> {
            if self.files.contains_key(path) {
                Ok(1) // single handle for the test
            } else {
                Err(2)
            }
        }
        fn read(&mut self, _h: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let data = self.files.get("hello.txt").ok_or(2)?;
            let start = (offset as usize).min(data.len());
            let n = (data.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&data[start..start + n]);
            Ok(n)
        }
        fn write(&mut self, _h: u64, _o: u64, _d: &[u8]) -> Result<usize, i32> {
            Ok(0)
        }
        fn mkdir(&mut self, _p: &str, _m: u32) -> Result<(), i32> {
            Ok(())
        }
        fn readdir(&mut self, _p: &str) -> Result<Vec<(String, bool)>, i32> {
            Ok(vec![("hello.txt".to_string(), false)])
        }
        fn close(&mut self, _h: u64) {}
    }

    // 9P message builders (body after the size word).
    fn msg(ty: u8, tag: u16, body: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&((4 + 3 + body.len()) as u32).to_le_bytes());
        m.push(ty);
        m.extend_from_slice(&tag.to_le_bytes());
        m.extend_from_slice(body);
        m
    }
    fn s(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_le_bytes().to_vec();
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// Send a 9P message through the device; return the response body.
    fn rpc(d: &mut VirtioMmio, mem: &mut TestMem, req: &[u8]) -> Vec<u8> {
        // Reset + set up the queue.
        d.write(0x070, 0);
        d.write(0x28, PAGE as u32);
        d.write(0x30, 0);
        d.write(0x38, 4);
        d.write(0x3c, PAGE as u32);
        d.write(0x40, 1);
        let desc = 4096u64;
        let avail = 4160u64;
        let reqb = 0x30000u64;
        let respb = 0x32000u64;
        mem.put(reqb, req);
        // desc 0: request (readable); desc 1: response (writable).
        mem.put(desc, &reqb.to_le_bytes());
        mem.put(desc + 8, &(req.len() as u32).to_le_bytes());
        mem.put(desc + 12, &1u16.to_le_bytes()); // NEXT
        mem.put(desc + 14, &1u16.to_le_bytes());
        mem.put(desc + 16, &respb.to_le_bytes());
        mem.put(desc + 24, &4096u32.to_le_bytes());
        mem.put(desc + 28, &2u16.to_le_bytes()); // WRITE
        mem.put(avail + 2, &1u16.to_le_bytes());
        mem.put(avail + 4, &0u16.to_le_bytes());
        d.write(0x050, 0);
        d.process(mem);
        let out = mem.get(respb, 4096);
        let size = u32::from_le_bytes([out[0], out[1], out[2], out[3]]) as usize;
        out[4..size].to_vec()
    }

    #[test]
    fn ninep_version_attach_walk_read() {
        let mut d = VirtioMmio::new(Box::new(Virtio9p::new(MemFs::new(), "hostshare")), 1);
        let mut mem = TestMem(BTreeMap::new());

        // Tversion.
        let mut body = (8192u32).to_le_bytes().to_vec();
        body.extend_from_slice(&s("9P2000.L"));
        let r = rpc(&mut d, &mut mem, &msg(100, 1, &body));
        assert_eq!(r[0], 101, "Rversion");
        assert!(r.windows(8).any(|w| w == b"9P2000.L"), "version string echoed");

        // Tattach fid=1.
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // afid
        body.extend_from_slice(&s("root"));
        body.extend_from_slice(&s(""));
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // n_uname
        let r = rpc(&mut d, &mut mem, &msg(104, 2, &body));
        assert_eq!(r[0], 105, "Rattach");

        // Twalk fid=1 -> newfid=2 to "hello.txt".
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes()); // nwname
        body.extend_from_slice(&s("hello.txt"));
        let r = rpc(&mut d, &mut mem, &msg(110, 3, &body));
        assert_eq!(r[0], 111, "Rwalk");
        let nwqid = u16::from_le_bytes([r[3], r[4]]);
        assert_eq!(nwqid, 1, "walked one element");

        // Tlopen fid=2.
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes()); // O_RDONLY
        let r = rpc(&mut d, &mut mem, &msg(12, 4, &body));
        assert_eq!(r[0], 13, "Rlopen");

        // Tread fid=2 offset=0 count=64.
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&64u32.to_le_bytes());
        let r = rpc(&mut d, &mut mem, &msg(116, 5, &body));
        assert_eq!(r[0], 117, "Rread");
        let count = u32::from_le_bytes([r[3], r[4], r[5], r[6]]) as usize;
        let data = &r[7..7 + count];
        assert_eq!(data, b"hello, 9p");
    }
}

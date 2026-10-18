//! virtio-mmio framework (V1): register block reads, the legacy queue-setup
//! handshake, descriptor-chain walking, used-ring completion, and interrupt.
use std::collections::BTreeMap;
use vane_riscv::virtio::{
    DescriptorChain, QueueMem, VirtioDevice, VirtioMmio, DEV_BLK, MAGIC, VERSION_LEGACY,
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
    assert_eq!(d.read(0x02c), 128); // QueueNumMax
    assert!(!d.interrupt_pending());
}

#[test]
fn queue_roundtrip() {
    let mut d = VirtioMmio::new(Box::new(Reverse), 1);
    let mut mem = TestMem(BTreeMap::new());

    // Legacy queue-setup handshake.
    d.write(0x020, PAGE as u32); // GuestPageSize
    d.write(0x028, 0); // QueueSel = 0
    assert_eq!(d.read(0x02c), 128); // QueueNumMax
    d.write(0x030, 4); // QueueNum = 4
    d.write(0x034, PAGE as u32); // QueueAlign
    d.write(0x038, 1); // QueuePFN = 1 -> base 4096

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

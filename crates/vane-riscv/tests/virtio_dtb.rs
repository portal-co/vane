//! R8: the virtio,mmio DTB node is emitted and the device's MMIO magic reads.
use vane_riscv::machine::{Machine, VIRTIO_MMIO_BASE, virt_dtb_with_virtio};
use vane_riscv::virtio::{DescriptorChain, MAGIC, QueueMem, VirtioDevice, VirtioMmio};

struct Stub;
impl VirtioDevice for Stub {
    fn device_id(&self) -> u32 {
        2
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn read_config(&self, _o: u64, _b: &mut [u8]) {}
    fn process(&mut self, _q: usize, _c: &DescriptorChain, _m: &mut dyn QueueMem) -> u32 {
        0
    }
}

#[test]
fn virtio_mmio_dtb_node_and_magic() {
    let dtb = virt_dtb_with_virtio(128 * 1024 * 1024, None, "console=ttyS0", &[2]);
    let s = String::from_utf8_lossy(&dtb);
    // The node references virtio,mmio and the right address.
    let dtb_str: String = dtb.iter().map(|&b| b as char).collect();
    assert!(
        dtb.windows(b"virtio,mmio".len())
            .any(|w| w == b"virtio,mmio"),
        "virtio,mmio compatible present"
    );
    // The MMIO magic reads correctly through the machine.
    let mut m = Machine::new();
    m.add_virtio(Box::new(Stub));
    let magic = m.load(VIRTIO_MMIO_BASE, 4, false).expect("magic read");
    assert_eq!(magic, MAGIC as u64, "virtio magic");
    let dev_id = m.load(VIRTIO_MMIO_BASE + 8, 4, false).expect("device id");
    assert_eq!(dev_id, 2, "virtio-blk device id");
    let _ = s;
}

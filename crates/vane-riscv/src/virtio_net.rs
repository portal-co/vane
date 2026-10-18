//! virtio-net device (PLAN_VIRTIO_WEVAL workstream D / milestone V4;
//! spday full-virtualization plan item D).
//!
//! A legacy-interface virtio-net NIC: queue 0 = RX (guest buffers filled
//! with received frames), queue 1 = TX (guest frames handed to the
//! backend). Frame IO goes through the pluggable [`NetBackend`] — the
//! device itself is host-agnostic: native builds bridge to the
//! os-emulation TCP emulator, WASI/browser builds to their own backends.
//!
//! RX uses the transport's chain deferral (`process_or_defer`): a posted
//! RX buffer with no frame pending is consumed from the available ring and
//! completed in the used ring only when a frame arrives — matching real
//! hardware, so the guest driver's buffer accounting stays exact.
//!
//! No feature bits are offered (no merge-rx, no checksum/TSO offload): the
//! driver prepends the 10-byte legacy `virtio_net_hdr` to every frame and
//! sends one frame per chain.

use crate::virtio::{DEV_NET, DescriptorChain, QueueMem, VirtioDevice};

/// Legacy virtio-net header length (no `VIRTIO_NET_F_MRG_RXBUF`).
pub const VIRTIO_NET_HDR_LEN: usize = 10;

/// Config-space offsets (legacy layout).
const CFG_MAC: u64 = 0; // 6 bytes
const CFG_STATUS: u64 = 6; // u16, bit 0 = link up

/// The host side of the NIC. Implementations decide where frames go: a
/// TUN/TAP bridge, the os-emulation TCP emulator, a WebSocket relay, a
/// pcap sink...
pub trait NetBackend {
    /// Guest → host: one full Ethernet frame.
    fn send_frame(&mut self, frame: &[u8]);
    /// Host → guest: the next frame for the guest, if any.
    fn poll_recv(&mut self) -> Option<alloc::vec::Vec<u8>>;
    /// Link status reported in the config space.
    fn link_up(&self) -> bool {
        true
    }
}

/// A virtio-net device on a [`NetBackend`].
pub struct VirtioNet {
    mac: [u8; 6],
    backend: alloc::boxed::Box<dyn NetBackend>,
    /// A frame received from the host not yet placed into an RX chain.
    pending_rx: Option<alloc::vec::Vec<u8>>,
}

impl VirtioNet {
    pub fn new(mac: [u8; 6], backend: alloc::boxed::Box<dyn NetBackend>) -> Self {
        VirtioNet {
            mac,
            backend,
            pending_rx: None,
        }
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        DEV_NET
    }

    fn num_queues(&self) -> usize {
        2 // RX, TX
    }

    fn queue_size(&self) -> u32 {
        256
    }

    fn read_config(&self, offset: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            let o = offset + i as u64;
            *b = match o {
                o if (CFG_MAC..CFG_MAC + 6).contains(&o) => self.mac[(o - CFG_MAC) as usize],
                o if o == CFG_STATUS => (self.backend.link_up() as u8) & 1,
                _ => 0,
            };
        }
    }

    fn process(&mut self, queue: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32 {
        // Used only for TX (queue 1): immediate completion. RX goes through
        // process_or_defer; the default calls this, so a direct RX call is
        // treated as "poll once, complete empty" — unreachable in practice
        // (process_or_defer below overrides the RX path entirely).
        match queue {
            1 => {
                let req = chain.read_request(mem);
                if req.len() >= VIRTIO_NET_HDR_LEN {
                    self.backend.send_frame(&req[VIRTIO_NET_HDR_LEN..]);
                }
                0 // TX chains have no writable payload
            }
            _ => 0,
        }
    }

    fn process_or_defer(
        &mut self,
        queue: usize,
        chain: &DescriptorChain,
        mem: &mut dyn QueueMem,
    ) -> Option<u32> {
        if queue == 0 {
            // RX: fill the chain when a frame is pending, else defer.
            if self.pending_rx.is_none() {
                self.pending_rx = self.backend.poll_recv();
            }
            let frame = self.pending_rx.take()?;
            let mut packet = alloc::vec![0u8; VIRTIO_NET_HDR_LEN];
            packet.extend_from_slice(&frame);
            Some(chain.write_response(mem, &packet))
        } else {
            Some(self.process(queue, chain, mem))
        }
    }

    fn tick(&mut self, _mem: &mut dyn QueueMem) {
        // Pull host frames eagerly so a deferred RX chain completes on the
        // next transport pass even without a queue notification.
        if self.pending_rx.is_none() {
            self.pending_rx = self.backend.poll_recv();
        }
    }
}

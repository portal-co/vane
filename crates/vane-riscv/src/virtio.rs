//! virtio-mmio framework (PLAN_VIRTIO_WEVAL.md workstream A / milestone V1).
//!
//! A shared split-virtqueue + MMIO-register-block implementation that the
//! concrete devices (blk, 9p, net) plug into via [`VirtioDevice`]. Devices are
//! registered on the [`Machine`](crate::machine::Machine) at
//! `0x1000_1000 + 0x1000*slot` with PLIC source `1 + slot`.
//!
//! The transport speaks the **legacy (v1) virtio-mmio** interface — the
//! container2wasm/TinyEMU shape mainline Linux still supports — because it is
//! the simplest correct thing (single `QueuePFN` register, no `QueueReady`
//! handshake or config generation). Modern v2 can be layered on later by
//! reporting `Version = 2` and serving the `QueueDesc/Driver/Device` 64-bit
//! registers.
//!
//! Guest-memory access (descriptor tables, avail/used rings, and the buffers
//! they point at) goes through the [`QueueMem`] trait, implemented for
//! [`vane_arch::Mem`], so the device path is swap-aware and — for a non-local
//! host — can park via the coroutine seam.

/// virtio-mmio magic register value ("virt").
pub const MAGIC: u32 = 0x7472_6976;
/// Legacy interface version.
pub const VERSION_LEGACY: u32 = 1;

/// Device IDs (virtio spec).
pub const DEV_NET: u32 = 1;
pub const DEV_BLK: u32 = 2;
pub const DEV_9P: u32 = 9;
pub const DEV_RNG: u32 = 4;

// Legacy (v1) register offsets within the 0x1000-byte MMIO region.
const R_MAGIC: u64 = 0x000;
const R_VERSION: u64 = 0x004;
const R_DEVICE_ID: u64 = 0x008;
const R_VENDOR_ID: u64 = 0x00c;
const R_HOST_FEATURES: u64 = 0x010;
const R_HOST_FEATURES_SEL: u64 = 0x014;
const R_DRIVER_FEATURES: u64 = 0x020; // driver writes accepted features here
const R_DRIVER_FEATURES_SEL: u64 = 0x024;
const R_GUEST_PAGE_SIZE_LEGACY: u64 = 0x028; // legacy (v1) only
const R_QUEUE_SEL: u64 = 0x030;
const R_QUEUE_NUM_MAX: u64 = 0x034;
const R_QUEUE_NUM: u64 = 0x038;
const R_QUEUE_ALIGN: u64 = 0x03c; // legacy
const R_QUEUE_PFN: u64 = 0x040; // legacy
const R_QUEUE_NOTIFY: u64 = 0x050;
const R_INTERRUPT_STATUS: u64 = 0x060;
const R_INTERRUPT_ACK: u64 = 0x064;
const R_STATUS: u64 = 0x070;
const R_CONFIG: u64 = 0x100; // device-specific config space starts here

// Status bits.
pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_FAILED: u32 = 128;

// Interrupt status bits.
const INT_USED_BUFFER: u32 = 1;
const INT_CONFIG_CHANGE: u32 = 2;

// Descriptor flags.
const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// Guest-memory access for virtqueue walking and buffer I/O. Implemented for
/// [`vane_arch::Mem`]; the default helpers are little-endian word accessors.
pub trait QueueMem {
    fn read(&mut self, addr: u64, buf: &mut [u8]);
    fn write(&mut self, addr: u64, buf: &[u8]);

    fn read_u16(&mut self, addr: u64) -> u16 {
        let mut b = [0u8; 2];
        self.read(addr, &mut b);
        u16::from_le_bytes(b)
    }
    fn read_u32(&mut self, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        self.read(addr, &mut b);
        u32::from_le_bytes(b)
    }
    fn read_u64(&mut self, addr: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(addr, &mut b);
        u64::from_le_bytes(b)
    }
    fn write_u16(&mut self, addr: u64, v: u16) {
        self.write(addr, &v.to_le_bytes());
    }
    fn write_u32(&mut self, addr: u64, v: u32) {
        self.write(addr, &v.to_le_bytes());
    }
}

impl QueueMem for vane_arch::Mem {
    fn read(&mut self, addr: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.read_byte(addr.wrapping_add(i as u64));
        }
    }
    fn write(&mut self, addr: u64, buf: &[u8]) {
        for (i, b) in buf.iter().enumerate() {
            self.write_byte(addr.wrapping_add(i as u64), *b);
        }
    }
}

/// A parsed descriptor chain from the available ring, handed to the device's
/// `process`. `readable` buffers are written by the driver (the request);
/// `writable` buffers are filled by the device (the response).
#[derive(Clone)]
pub struct DescriptorChain {
    /// Index of the chain's head descriptor (echoed in the used ring).
    pub head: u16,
    /// Device-readable buffers as (guest physical addr, len).
    pub readable: alloc::vec::Vec<(u64, u32)>,
    /// Device-writable buffers as (guest physical addr, len).
    pub writable: alloc::vec::Vec<(u64, u32)>,
}

impl DescriptorChain {
    /// Read the concatenation of the readable (request) buffers.
    pub fn read_request(&self, mem: &mut dyn QueueMem) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::new();
        for &(addr, len) in &self.readable {
            let start = out.len();
            out.resize(start + len as usize, 0);
            mem.read(addr, &mut out[start..]);
        }
        out
    }

    /// Write `data` across the writable (response) buffers; returns the number
    /// of bytes actually written (capped at the buffers' total length).
    pub fn write_response(&self, mem: &mut dyn QueueMem, data: &[u8]) -> u32 {
        let mut written = 0usize;
        for &(addr, len) in &self.writable {
            if written >= data.len() {
                break;
            }
            let n = core::cmp::min(len as usize, data.len() - written);
            mem.write(addr, &data[written..written + n]);
            written += n;
        }
        written as u32
    }
}

/// A virtio device plugged into the transport. The transport handles the
/// register block, virtqueue walking, and interrupt; the device answers
/// feature/config queries and processes parsed descriptor chains.
pub trait VirtioDevice {
    /// virtio device ID (see the DEV_* constants).
    fn device_id(&self) -> u32;
    /// Number of virtqueues.
    fn num_queues(&self) -> usize;
    /// Maximum size of each queue (power of two).
    fn queue_size(&self) -> u32 {
        128
    }
    /// Device-specific feature bits (low 32; the transport adds none of its
    /// own for the legacy interface).
    fn features(&self) -> u32 {
        0
    }
    /// Read the device config space.
    fn read_config(&self, offset: u64, buf: &mut [u8]);
    /// Write the device config space.
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    /// Process one descriptor chain popped from `queue`. The device reads the
    /// request via `chain.read_request`, writes its response via
    /// `chain.write_response` (or directly via `mem` for scatter/gather), and
    /// returns the total bytes written to the writable buffers (reported in
    /// the used ring).
    fn process(&mut self, queue: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32;
    /// Process a chain with the option to DEFER completion (`None`): the
    /// transport stashes the chain and re-offers it via this method on every
    /// `process` pass until the device completes it (`Some(written)`). This
    /// is how real devices consume the available ring promptly but complete
    /// in the used ring only when they actually have data — e.g. a network
    /// RX chain waiting for a frame. The default completes immediately via
    /// [`VirtioDevice::process`], so existing devices are unchanged.
    fn process_or_defer(
        &mut self,
        queue: usize,
        chain: &DescriptorChain,
        mem: &mut dyn QueueMem,
    ) -> Option<u32> {
        Some(self.process(queue, chain, mem))
    }
    /// Periodic tick (a device can make asynchronous progress and complete
    /// queued requests here). Default: nothing.
    fn tick(&mut self, _mem: &mut dyn QueueMem) {}
    /// Opaque device state for machine snapshots (PLAN_VIRTIO_WEVAL
    /// workstream E). Host-side resources (open files, sockets, RNG seeds)
    /// must NOT be serialized — the host re-binds them after restore.
    /// Default: stateless device.
    fn save_state(&self) -> alloc::vec::Vec<u8> {
        alloc::vec::Vec::new()
    }
    /// Restore state previously produced by [`VirtioDevice::save_state`].
    /// Default: stateless device (ignores).
    fn load_state(&mut self, _state: &[u8]) {}
}

/// One split virtqueue (legacy PFN-addressed).
#[derive(Clone)]
struct Virtqueue {
    /// Driver-chosen size (<= num_max); 0 until the driver sets QueueNum.
    num: u32,
    /// Guest page size for PFN translation (legacy GuestPageSize register).
    page_size: u32,
    /// Alignment (legacy QueueAlign register).
    align: u32,
    /// QueuePFN: guest physical frame number of the queue.
    pfn: u32,
    /// Available-ring index we've processed up to.
    last_avail: u16,
    /// Used-ring index (entries we've completed).
    used_idx: u16,
}

impl Virtqueue {
    fn new() -> Self {
        Virtqueue {
            num: 0,
            page_size: 4096,
            align: 4096,
            pfn: 0,
            last_avail: 0,
            used_idx: 0,
        }
    }

    fn enabled(&self) -> bool {
        self.pfn != 0 && self.num != 0
    }

    /// Guest physical base of the queue region.
    fn base(&self) -> u64 {
        self.pfn as u64 * self.page_size as u64
    }
    fn desc_addr(&self) -> u64 {
        self.base()
    }
    fn avail_addr(&self) -> u64 {
        self.base() + 16 * self.num as u64
    }
    fn used_addr(&self) -> u64 {
        let avail_end = self.avail_addr() + 4 + 2 * self.num as u64;
        // Legacy: the used ring starts at the next `align` boundary.
        (avail_end + self.align as u64 - 1) & !(self.align as u64 - 1)
    }
}

/// Why a snapshot restore failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// The snapshot's virtio device at a slot does not match the live
    /// device (different device id): the restore would mix states.
    VirtioDeviceMismatch { expected: u32, got: u32 },
    /// The snapshot has a different number of virtio devices than the
    /// live machine: device topology is fixed at machine construction.
    VirtioCountMismatch { expected: usize, got: usize },
}

/// Snapshot of one virtio-mmio transport + the device backend's opaque
/// state (see [`VirtioDevice::save_state`]).
#[derive(Clone)]
pub struct VirtioMmioSnapshot {
    /// Device identity, checked on restore.
    pub device_id: u32,
    /// PLIC source (checked on restore).
    pub irq: u32,
    pub status: u32,
    pub host_features_sel: u32,
    pub queue_sel: u32,
    pub interrupt_status: u32,
    queues: alloc::vec::Vec<Virtqueue>,
    pub notified: bool,
    /// Chains deferred by the device (see `VirtioDevice::process_or_defer`).
    pub deferred: alloc::vec::Vec<(usize, DescriptorChain)>,
    /// Opaque device-backend state (host resources excluded by contract).
    pub device_state: alloc::vec::Vec<u8>,
}

/// A virtio-mmio register block wrapping a device: the MMIO face the guest
/// sees plus the queue-processing engine.
pub struct VirtioMmio {
    device: alloc::boxed::Box<dyn VirtioDevice>,
    /// PLIC source this device raises.
    irq: u32,
    status: u32,
    host_features_sel: u32,
    queue_sel: u32,
    interrupt_status: u32,
    queues: alloc::vec::Vec<Virtqueue>,
    /// Set by QueueNotify; cleared once the queue is drained.
    notified: bool,
    /// Chains the device deferred (see `VirtioDevice::process_or_defer`):
    /// consumed from the available ring, not yet completed in the used ring.
    deferred: alloc::vec::Vec<(usize, DescriptorChain)>,
}

impl VirtioMmio {
    pub fn new(device: alloc::boxed::Box<dyn VirtioDevice>, irq: u32) -> Self {
        let n = device.num_queues();
        VirtioMmio {
            device,
            irq,
            status: 0,
            host_features_sel: 0,
            queue_sel: 0,
            interrupt_status: 0,
            queues: (0..n).map(|_| Virtqueue::new()).collect(),
            notified: false,
            deferred: alloc::vec::Vec::new(),
        }
    }

    pub fn irq(&self) -> u32 {
        self.irq
    }

    /// Level-triggered interrupt line: asserted while there's an unacked
    /// interrupt (used-buffer or config change).
    pub fn interrupt_pending(&self) -> bool {
        self.interrupt_status != 0
    }

    pub fn device_id(&self) -> u32 {
        self.device.device_id()
    }

    fn sel_queue(&mut self) -> Option<&mut Virtqueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Read a 32-bit MMIO register at `offset`.
    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            R_MAGIC => MAGIC,
            R_VERSION => VERSION_LEGACY,
            R_DEVICE_ID => self.device.device_id(),
            R_VENDOR_ID => 0x76616E65, // "vane"
            R_HOST_FEATURES => {
                if self.host_features_sel == 0 {
                    self.device.features()
                } else {
                    0 // no high feature bits
                }
            }
            R_QUEUE_NUM_MAX => self.device.queue_size(),
            R_QUEUE_PFN => self
                .queues
                .get(self.queue_sel as usize)
                .map(|q| q.pfn)
                .unwrap_or(0),
            R_INTERRUPT_STATUS => self.interrupt_status,
            R_STATUS => self.status,
            o if (R_CONFIG..R_CONFIG + 0x100).contains(&o) => {
                let mut b = [0u8; 4];
                self.device.read_config(o - R_CONFIG, &mut b);
                u32::from_le_bytes(b)
            }
            _ => 0,
        }
    }

    /// Write a 32-bit MMIO register at `offset`.
    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            R_HOST_FEATURES_SEL => self.host_features_sel = value,
            // The driver writes its accepted features (DRIVER_FEATURES{,_SEL}).
            // We offer no device features, so there's nothing to record, but
            // the registers must exist at the correct offsets.
            R_DRIVER_FEATURES | R_DRIVER_FEATURES_SEL => {}
            R_GUEST_PAGE_SIZE_LEGACY => {
                if let Some(q) = self.sel_queue() {
                    q.page_size = value.max(1);
                }
            }
            R_QUEUE_SEL => self.queue_sel = value,
            R_QUEUE_NUM => {
                let max = self.device.queue_size();
                if let Some(q) = self.sel_queue() {
                    q.num = value.min(max);
                }
            }
            R_QUEUE_ALIGN => {
                if let Some(q) = self.sel_queue() {
                    q.align = value.max(1);
                }
            }
            R_QUEUE_PFN => {
                if let Some(q) = self.sel_queue() {
                    q.pfn = value;
                }
            }
            R_QUEUE_NOTIFY => self.notified = true,
            R_INTERRUPT_ACK => self.interrupt_status &= !value,
            R_STATUS => {
                if value == 0 {
                    // Device reset.
                    self.status = 0;
                    self.interrupt_status = 0;
                    for q in &mut self.queues {
                        *q = Virtqueue::new();
                    }
                } else {
                    self.status = value;
                }
            }
            o if (R_CONFIG..R_CONFIG + 0x100).contains(&o) => {
                self.device.write_config(o - R_CONFIG, &value.to_le_bytes());
            }
            _ => {}
        }
    }

    /// True when a queue has been notified and not yet drained, or the device
    /// wants a tick to make progress.
    pub fn needs_processing(&self) -> bool {
        self.notified
    }

    /// Drain notified queues: pop available-ring entries, hand each chain to
    /// the device, push completions onto the used ring, and raise the IRQ.
    /// Deferred chains are re-offered every pass. Returns true if an
    /// interrupt was raised.
    pub fn process(&mut self, mem: &mut dyn QueueMem) -> bool {
        let mut raised = false;
        // Re-offer deferred chains first: the device may now be able to
        // complete them (e.g. a frame arrived for a stashed RX buffer).
        let mut i = 0;
        while i < self.deferred.len() {
            let (qi, chain) = self.deferred[i].clone();
            if let Some(written) = self.device.process_or_defer(qi, &chain, mem) {
                self.deferred.remove(i);
                self.complete_chain(qi, chain.head, written, mem);
                raised = true;
            } else {
                i += 1;
            }
        }
        if self.notified {
            self.notified = false;
            for qi in 0..self.queues.len() {
                if self.process_queue(qi, mem) {
                    raised = true;
                }
            }
        }
        // Let the device make asynchronous progress (e.g. a net device that
        // received a frame); a tick that completes work also raises.
        self.device.tick(mem);
        raised || self.interrupt_status != 0
    }

    fn process_queue(&mut self, qi: usize, mem: &mut dyn QueueMem) -> bool {
        if !self.queues[qi].enabled() {
            return false;
        }
        let mut completed = false;
        loop {
            let (head, avail) = {
                let q = &self.queues[qi];
                let avail_idx = mem.read_u16(q.avail_addr() + 2);
                if q.last_avail == avail_idx {
                    break;
                }
                let slot = q.last_avail % q.num as u16;
                let head = mem.read_u16(q.avail_addr() + 4 + 2 * slot as u64);
                (head, q.last_avail)
            };
            let chain = self.read_chain(qi, head, mem);
            match self.device.process_or_defer(qi, &chain, mem) {
                Some(written) => {
                    self.complete_chain(qi, chain.head, written, mem);
                    let q = &mut self.queues[qi];
                    q.last_avail = avail.wrapping_add(1);
                    completed = true;
                }
                None => {
                    // Deferred: the available ring advances, the used ring
                    // completes later (see process_or_defer).
                    let q = &mut self.queues[qi];
                    q.last_avail = avail.wrapping_add(1);
                    self.deferred.push((qi, chain));
                }
            }
        }
        if completed {
            self.interrupt_status |= INT_USED_BUFFER;
        }
        completed
    }

    /// Complete a chain in the used ring: push the (head, written) entry and
    /// advance the used index.
    fn complete_chain(&mut self, qi: usize, head: u16, written: u32, mem: &mut dyn QueueMem) {
        let q = &mut self.queues[qi];
        let used = q.used_addr();
        let uslot = q.used_idx % q.num as u16;
        mem.write_u32(used + 4 + 8 * uslot as u64, head as u32);
        mem.write_u32(used + 4 + 8 * uslot as u64 + 4, written);
        q.used_idx = q.used_idx.wrapping_add(1);
        let used_idx_addr = used + 2;
        let used_idx = q.used_idx;
        mem.write_u16(used_idx_addr, used_idx);
        self.interrupt_status |= INT_USED_BUFFER;
    }

    /// Snapshot the transport state (register block + queues) and the
    /// device-backend's opaque state (PLAN_VIRTIO_WEVAL workstream E).
    pub fn save(&self) -> VirtioMmioSnapshot {
        VirtioMmioSnapshot {
            device_id: self.device.device_id(),
            irq: self.irq,
            status: self.status,
            host_features_sel: self.host_features_sel,
            queue_sel: self.queue_sel,
            interrupt_status: self.interrupt_status,
            queues: self.queues.clone(),
            notified: self.notified,
            deferred: self.deferred.clone(),
            device_state: self.device.save_state(),
        }
    }

    /// Restore from a snapshot. The device backend itself is NOT restored
    /// (host resources are re-bound by the host); only its opaque state is.
    /// Mismatched device identity is a visible error, not a silent mixup.
    pub fn restore(&mut self, snap: &VirtioMmioSnapshot) -> Result<(), SnapshotError> {
        if snap.device_id != self.device.device_id() || snap.irq != self.irq {
            return Err(SnapshotError::VirtioDeviceMismatch {
                expected: snap.device_id,
                got: self.device.device_id(),
            });
        }
        self.status = snap.status;
        self.host_features_sel = snap.host_features_sel;
        self.queue_sel = snap.queue_sel;
        self.interrupt_status = snap.interrupt_status;
        self.queues = snap.queues.clone();
        self.notified = snap.notified;
        self.deferred = snap.deferred.clone();
        self.device.load_state(&snap.device_state);
        Ok(())
    }

    /// Walk a descriptor chain starting at `head` on queue `qi`, collecting
    /// readable and writable buffers.
    fn read_chain(&self, qi: usize, head: u16, mem: &mut dyn QueueMem) -> DescriptorChain {
        let mut chain = DescriptorChain {
            head,
            readable: alloc::vec::Vec::new(),
            writable: alloc::vec::Vec::new(),
        };
        let desc_base = self.queues.get(qi).map(|q| q.desc_addr()).unwrap_or(0);
        let mut idx = head;
        let mut seen = 0u32;
        loop {
            let d = desc_base + 16 * idx as u64;
            let addr = mem.read_u64(d);
            let len = mem.read_u32(d + 8);
            let flags = mem.read_u16(d + 12);
            let next = mem.read_u16(d + 14);
            if flags & DESC_F_WRITE != 0 {
                chain.writable.push((addr, len));
            } else {
                chain.readable.push((addr, len));
            }
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
            seen += 1;
            if seen > 4096 {
                break; // malformed chain; bail
            }
        }
        chain
    }
}

// ── Snapshot serialization (W1.5) ────────────────────────────────────────────
use crate::snapshot_ser::{Reader, SerError, Writer};

impl Virtqueue {
    fn encode(&self, w: &mut Writer) {
        w.u32(self.num);
        w.u32(self.page_size);
        w.u32(self.align);
        w.u32(self.pfn);
        w.u16(self.last_avail);
        w.u16(self.used_idx);
    }
    fn decode(r: &mut Reader) -> Result<Virtqueue, SerError> {
        Ok(Virtqueue {
            num: r.u32()?,
            page_size: r.u32()?,
            align: r.u32()?,
            pfn: r.u32()?,
            last_avail: r.u16()?,
            used_idx: r.u16()?,
        })
    }
}

impl DescriptorChain {
    fn encode(&self, w: &mut Writer) {
        w.u16(self.head);
        w.u32(self.readable.len() as u32);
        for &(a, l) in &self.readable {
            w.u64(a);
            w.u32(l);
        }
        w.u32(self.writable.len() as u32);
        for &(a, l) in &self.writable {
            w.u64(a);
            w.u32(l);
        }
    }
    fn decode(r: &mut Reader) -> Result<DescriptorChain, SerError> {
        let head = r.u16()?;
        let mut readable = alloc::vec::Vec::new();
        for _ in 0..r.u32()? {
            readable.push((r.u64()?, r.u32()?));
        }
        let mut writable = alloc::vec::Vec::new();
        for _ in 0..r.u32()? {
            writable.push((r.u64()?, r.u32()?));
        }
        Ok(DescriptorChain {
            head,
            readable,
            writable,
        })
    }
}

impl VirtioMmioSnapshot {
    pub fn encode(&self, w: &mut Writer) {
        w.u32(self.device_id);
        w.u32(self.irq);
        w.u32(self.status);
        w.u32(self.host_features_sel);
        w.u32(self.queue_sel);
        w.u32(self.interrupt_status);
        w.u32(self.queues.len() as u32);
        for q in &self.queues {
            q.encode(w);
        }
        w.bool(self.notified);
        w.u32(self.deferred.len() as u32);
        for (qi, chain) in &self.deferred {
            w.u32(*qi as u32);
            chain.encode(w);
        }
        w.blob(&self.device_state);
    }
    pub fn decode(r: &mut Reader) -> Result<VirtioMmioSnapshot, SerError> {
        let device_id = r.u32()?;
        let irq = r.u32()?;
        let status = r.u32()?;
        let host_features_sel = r.u32()?;
        let queue_sel = r.u32()?;
        let interrupt_status = r.u32()?;
        let mut queues = alloc::vec::Vec::new();
        for _ in 0..r.u32()? {
            queues.push(Virtqueue::decode(r)?);
        }
        let notified = r.bool()?;
        let mut deferred = alloc::vec::Vec::new();
        for _ in 0..r.u32()? {
            let qi = r.u32()? as usize;
            deferred.push((qi, DescriptorChain::decode(r)?));
        }
        let device_state = alloc::vec::Vec::from(r.blob()?);
        Ok(VirtioMmioSnapshot {
            device_id,
            irq,
            status,
            host_features_sel,
            queue_sel,
            interrupt_status,
            queues,
            notified,
            deferred,
            device_state,
        })
    }
}

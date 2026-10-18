//! QEMU `virt`-compatible machine model (M4, PLAN_LINUX_WASM.md §7.4):
//! ns16550a UART, ACLINT (mswi + mtimer), PLIC, and the SiFive test
//! finisher, plus the memory-mapped dispatch that routes sub-RAM physical
//! addresses to devices.
//!
//! Everything here is host-independent and `no_std`: the host drives time
//! and console IO through [`Machine::tick`] / [`Machine::drain_tx`] /
//! [`Machine::push_rx`]. virtio-mmio devices land later in M4.
//!
//! Address map (QEMU virt):
//! - `0x0010_0000` — SiFive test finisher (poweroff/reboot)
//! - `0x0200_0000` — ACLINT mtimer (mtimecmp @ +0x4000, mtime @ +0xBFF8)
//! - `0x02F0_0000` — ACLINT mswi
//! - `0x0C00_0000` — PLIC
//! - `0x1000_0000` — ns16550a UART (PLIC source 10)
//! - `0x8000_0000` — RAM base

use super::hart::{CAUSE_LOAD_PAGE_FAULT, CAUSE_STORE_PAGE_FAULT, Trap};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// RAM base of the virt machine. Physical addresses below this are MMIO.
pub const RAM_BASE: u64 = 0x8000_0000;

pub const TEST_FINISHER_BASE: u64 = 0x0010_0000;
pub const ACLINT_MTIMER_BASE: u64 = 0x0200_0000;
pub const ACLINT_MSWI_BASE: u64 = 0x02F0_0000;
pub const PLIC_BASE: u64 = 0x0C00_0000;
pub const UART_BASE: u64 = 0x1000_0000;
/// First virtio-mmio slot; each device occupies 0x1000 bytes
/// (`VIRTIO_MMIO_BASE + slot*0x1000`), PLIC source `1 + slot`.
pub const VIRTIO_MMIO_BASE: u64 = 0x1000_1000;

pub const UART_IRQ: u32 = 10;

/// ns16550a UART. TX accumulates in `tx`; RX bytes arrive via `push_rx`.
/// Interrupt (PLIC source [`UART_IRQ`]) is raised when RX data is pending
/// and enabled in IER.
#[derive(Clone)]
pub struct Uart16550 {
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    rx: VecDeque<u8>,
    tx: Vec<u8>,
}

impl Uart16550 {
    pub fn new() -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: 0,
            fcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            rx: VecDeque::new(),
            tx: Vec::new(),
        }
    }

    pub fn push_rx(&mut self, bytes: &[u8]) {
        self.rx.extend(bytes.iter().copied());
    }

    pub fn drain_tx(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.tx)
    }

    /// Interrupt pending (RX data available and enabled, or THR-empty and
    /// enabled). Conservative: THR-empty interrupt is level-triggered
    /// whenever enabled, which is how real 16550s behave.
    pub fn irq_pending(&self) -> bool {
        let rda = self.ier & 1 != 0 && !self.rx.is_empty();
        let thre = self.ier & 2 != 0;
        rda || thre
    }

    fn read(&mut self, offset: u64) -> u8 {
        let dlab = self.lcr & 0x80 != 0;
        match offset {
            0 => {
                if dlab {
                    self.dll
                } else {
                    self.rx.pop_front().unwrap_or(0)
                }
            }
            1 => {
                if dlab {
                    self.dlm
                } else {
                    self.ier
                }
            }
            // IIR: bit0 = 0 means interrupt pending. Encode RDA (0b010)
            // ahead of THRE (0b001) per 16550 priorities.
            2 => {
                if self.ier & 1 != 0 && !self.rx.is_empty() {
                    0b0100
                } else if self.ier & 2 != 0 {
                    0b0010
                } else {
                    0b0001
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            // LSR: DR + THRE + TEMT (transmitter is always idle — writes go
            // to the queue instantly).
            5 => 0x60 | if self.rx.is_empty() { 0 } else { 1 },
            6 => 0, // MSR: no modem
            7 => self.scr,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, value: u8) {
        let dlab = self.lcr & 0x80 != 0;
        match offset {
            0 => {
                if dlab {
                    self.dll = value;
                } else {
                    self.tx.push(value);
                }
            }
            1 => {
                if dlab {
                    self.dlm = value;
                } else {
                    self.ier = value & 0x0F;
                }
            }
            2 => self.fcr = value,
            3 => self.lcr = value,
            4 => self.mcr = value,
            7 => self.scr = value,
            _ => {}
        }
    }
}

impl Default for Uart16550 {
    fn default() -> Self {
        Self::new()
    }
}

/// ACLINT: mswi (MSIP) + mtimer (mtime/mtimecmp, MTIP). Time is driven by
/// the host via [`Aclint::set_time_nanos`]; the compare check runs on every
/// step through [`Aclint::mtip`].
#[derive(Clone)]
pub struct Aclint {
    pub msip: bool,
    pub mtimecmp: u64,
    /// mtime = time_nanos * freq_hz / 1e9
    pub time_nanos: u64,
    pub freq_hz: u64,
}

impl Aclint {
    pub fn new() -> Self {
        Self {
            msip: false,
            mtimecmp: u64::MAX,
            time_nanos: 0,
            freq_hz: 10_000_000, // QEMU virt default
        }
    }

    pub fn mtime(&self) -> u64 {
        (self.time_nanos as u128 * self.freq_hz as u128 / 1_000_000_000) as u64
    }

    pub fn mtip(&self) -> bool {
        self.mtime() >= self.mtimecmp
    }

    /// `offset` is relative to ACLINT_MTIMER_BASE or ACLINT_MSWI_BASE.
    /// Byte-granular (the [`Machine`] composes wider accesses).
    fn read(&self, is_mswi: bool, offset: u64) -> u64 {
        if is_mswi {
            // msip: 32-bit register at offset 0 (hart 0 only)
            if offset < 4 {
                let shift = offset * 8;
                return ((self.msip as u64) >> shift) & 0xFF;
            }
            return 0;
        }
        match offset {
            // CLINT compatibility: msip at the region base (CLINT puts msip
            // at 0x200_0000; ACLINT splits it out to 0x2F0_0000 — we serve
            // both).
            0x0..=3 => {
                let shift = offset * 8;
                ((self.msip as u64) >> shift) & 0xFF
            }
            0x4000..=0x4007 => {
                let shift = (offset - 0x4000) * 8;
                (self.mtimecmp >> shift) & 0xFF
            }
            0xBFF8..=0xBFFF => {
                let shift = (offset - 0xBFF8) * 8;
                (self.mtime() >> shift) & 0xFF
            }
            _ => 0,
        }
    }

    fn write(&mut self, is_mswi: bool, offset: u64, value: u64) {
        if is_mswi {
            // msip: only byte 0 carries the bit
            if offset == 0 {
                self.msip = value & 1 != 0;
            }
            return;
        }
        match offset {
            0x0 => self.msip = value & 1 != 0, // CLINT-compat msip
            0x4000..=0x4007 => {
                let shift = (offset - 0x4000) * 8;
                let mask = 0xFFu64 << shift;
                self.mtimecmp = (self.mtimecmp & !mask) | (((value & 0xFF) << shift) & mask);
            }
            // mtime is read-only from the bus (host-driven)
            _ => {}
        }
    }
}

impl Default for Aclint {
    fn default() -> Self {
        Self::new()
    }
}

/// Minimal PLIC (QEMU virt wiring: context 0 = M-mode hart 0, context 1 =
/// S-mode hart 0). 32 sources (we use 1..=8 for virtio, 10 for UART).
#[derive(Clone)]
pub struct Plic {
    priority: [u32; 32],
    pending: u32,
    enable: [u32; 2],
    threshold: [u32; 2],
}

impl Plic {
    pub fn new() -> Self {
        Self {
            priority: [0; 32],
            pending: 0,
            enable: [0; 2],
            threshold: [0; 2],
        }
    }

    pub fn set_pending(&mut self, irq: u32, pending: bool) {
        if irq < 32 {
            if pending {
                self.pending |= 1 << irq;
            } else {
                self.pending &= !(1 << irq);
            }
        }
    }

    /// Whether context `ctx` has a claimable interrupt (drives MEIP/SEIP).
    pub fn eip(&self, ctx: usize) -> bool {
        let cand = self.pending & self.enable[ctx];
        (0..32).any(|i| cand & (1 << i) != 0 && self.priority[i] > self.threshold[ctx])
    }

    /// Highest-priority pending-and-enabled source for claim.
    fn claim(&mut self, ctx: usize) -> u32 {
        let cand = self.pending & self.enable[ctx];
        let mut best = 0u32;
        let mut best_pri = self.threshold[ctx];
        for i in 1..32 {
            if cand & (1 << i) != 0 && self.priority[i] > best_pri {
                best = i as u32;
                best_pri = self.priority[i];
            }
        }
        if best != 0 {
            self.pending &= !(1 << best); // claim clears the pending bit
        }
        best
    }

    /// PLIC register read (MMIO interface; public for hosts/tests).
    pub fn read(&mut self, offset: u64) -> u64 {
        match offset {
            // priorities: 4 bytes per source, source i at offset i*4
            0x000000..=0x00007F => {
                let i = (offset / 4) as usize;
                (self.priority[i] >> ((offset % 4) * 8)) as u64 & 0xFF
            }
            // pending bits
            0x001000..=0x001003 => ((self.pending >> ((offset - 0x1000) * 8)) & 0xFF) as u64,
            // enable bits, 0x80 per context; we implement 32 sources (4 bytes)
            0x002000..=0x0020FF => {
                let ctx = ((offset - 0x2000) / 0x80) as usize;
                let byte = (offset - 0x2000) % 0x80;
                if byte >= 4 {
                    0
                } else {
                    ((self.enable[ctx] >> (byte * 8)) & 0xFF) as u64
                }
            }
            // per-context threshold/claim
            0x200000..=0x201FFF => {
                let ctx = ((offset - 0x200000) / 0x1000) as usize;
                let sub = (offset - 0x200000) % 0x1000;
                match sub {
                    0x0..=3 => ((self.threshold[ctx] >> (sub * 8)) & 0xFF) as u64,
                    0x4..=7 => {
                        let v = self.claim(ctx);
                        ((v as u64) >> ((sub - 4) * 8)) & 0xFF
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// PLIC register write (MMIO interface; public for hosts/tests).
    pub fn write(&mut self, offset: u64, value: u64) {
        let byte = (value & 0xFF) as u32;
        match offset {
            0x000000..=0x00007F => {
                let i = (offset / 4) as usize;
                let shift = (offset % 4) * 8;
                let mask = 0xFFu32 << shift;
                self.priority[i] = (self.priority[i] & !mask) | ((byte << shift) & mask);
            }
            0x002000..=0x0020FF => {
                let ctx = ((offset - 0x2000) / 0x80) as usize;
                let byteoff = (offset - 0x2000) % 0x80;
                if byteoff < 4 {
                    let shift = byteoff * 8;
                    let mask = 0xFFu32 << shift;
                    self.enable[ctx] = (self.enable[ctx] & !mask) | ((byte << shift) & mask);
                }
            }
            0x200000..=0x201FFF => {
                let ctx = ((offset - 0x200000) / 0x1000) as usize;
                let sub = (offset - 0x200000) % 0x1000;
                match sub {
                    0x0..=3 => {
                        let shift = sub * 8;
                        let mask = 0xFFu32 << shift;
                        self.threshold[ctx] =
                            (self.threshold[ctx] & !mask) | ((byte << shift) & mask);
                    }
                    // claim/complete write: complete source `value`
                    0x4..=7 if sub == 4 => {
                        let _ = value; // complete: nothing to do in our level model
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Default for Plic {
    fn default() -> Self {
        Self::new()
    }
}

/// SiFive test finisher: write 0x5555 = poweroff (pass), 0x7777 = reboot,
/// (code<<16)|0x3333 = poweroff with failure code.
#[derive(Clone, Default)]
pub struct Syscon {
    pub power: Option<PowerAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    PowerOff { code: u32 },
    Reboot,
}

/// Snapshot of the machine's device state (PLAN_VIRTIO_WEVAL workstream E;
/// spday full-virtualization plan item C). Guest RAM and hart state travel
/// separately (the reactor's memory snapshot + [`crate::hart::HartSnapshot`]);
/// virtio device *backends* (host files, sockets) are not serialized — the
/// host re-binds them after restore.
#[derive(Clone)]
pub struct MachineSnapshot {
    pub uart: Uart16550,
    pub aclint: Aclint,
    pub plic: Plic,
    pub syscon: Syscon,
    pub virtio: alloc::vec::Vec<crate::virtio::VirtioMmioSnapshot>,
    pub irq_dirty: bool,
}

/// The machine: devices + physical-address dispatch.
pub struct Machine {
    pub uart: Uart16550,
    pub aclint: Aclint,
    pub plic: Plic,
    pub syscon: Syscon,
    /// virtio-mmio devices (PLAN_VIRTIO_WEVAL workstream A); slot `i` is at
    /// `VIRTIO_MMIO_BASE + i*0x1000` with PLIC source `1 + i`.
    pub virtio: alloc::vec::Vec<crate::virtio::VirtioMmio>,
    /// Set whenever device state that can change an IRQ line changed
    /// (device store, UART RX push). `step_irqs` skips the PLIC scan while
    /// clear — per-step cost matters to the interpreter's hot loop.
    irq_dirty: bool,
    /// Cached PLIC external-interrupt line states [ctx0, ctx1].
    cached_eip: [bool; 2],
}

impl Machine {
    pub fn new() -> Self {
        Self {
            uart: Uart16550::new(),
            aclint: Aclint::new(),
            plic: Plic::new(),
            syscon: Syscon::default(),
            virtio: alloc::vec::Vec::new(),
            irq_dirty: true,
            cached_eip: [false; 2],
        }
    }

    /// True when `pa` is an MMIO address (below RAM).
    pub fn is_mmio(pa: u64) -> bool {
        pa < RAM_BASE
    }

    /// Register a virtio-mmio device at the next free slot
    /// (`VIRTIO_MMIO_BASE + slot*0x1000`, PLIC source `1 + slot`). Returns the
    /// slot index.
    pub fn add_virtio(
        &mut self,
        device: alloc::boxed::Box<dyn crate::virtio::VirtioDevice>,
    ) -> usize {
        let slot = self.virtio.len();
        let irq = 1 + slot as u32;
        self.virtio
            .push(crate::virtio::VirtioMmio::new(device, irq));
        slot
    }

    /// Capture the device state for a machine snapshot.
    pub fn save(&self) -> MachineSnapshot {
        MachineSnapshot {
            uart: self.uart.clone(),
            aclint: self.aclint.clone(),
            plic: self.plic.clone(),
            syscon: self.syscon.clone(),
            virtio: self.virtio.iter().map(|v| v.save()).collect(),
            irq_dirty: self.irq_dirty,
        }
    }

    /// Restore device state from a snapshot. Device topology (count and ids)
    /// is fixed at machine construction; a mismatch fails visibly.
    pub fn restore(&mut self, snap: &MachineSnapshot) -> Result<(), crate::virtio::SnapshotError> {
        if snap.virtio.len() != self.virtio.len() {
            return Err(crate::virtio::SnapshotError::VirtioCountMismatch {
                expected: snap.virtio.len(),
                got: self.virtio.len(),
            });
        }
        self.uart = snap.uart.clone();
        self.aclint = snap.aclint.clone();
        self.plic = snap.plic.clone();
        self.syscon = snap.syscon.clone();
        for (dev, dsnap) in self.virtio.iter_mut().zip(snap.virtio.iter()) {
            dev.restore(dsnap)?;
        }
        self.irq_dirty = snap.irq_dirty;
        Ok(())
    }

    /// Drain notified virtio queues with guest-memory access, raising device
    /// IRQs. Called by the host each quantum (the trace loop's tick point),
    /// since queue processing reads/writes guest RAM which the MMIO path
    /// doesn't have. Returns true if any device raised its interrupt line.
    pub fn process_virtio(&mut self, mem: &mut dyn crate::virtio::QueueMem) -> bool {
        let mut any = false;
        for dev in &mut self.virtio {
            if dev.process(mem) {
                self.plic.set_pending(dev.irq(), true);
                self.irq_dirty = true;
                any = true;
            }
        }
        any
    }

    /// virtio-mmio load, width-aware (registers are 32-bit). `None` when
    /// `addr` is not in the virtio range.
    fn virtio_load(&mut self, addr: u64, width: u8, signed: bool) -> Option<u64> {
        if !(VIRTIO_MMIO_BASE..VIRTIO_MMIO_BASE + 0x8000).contains(&addr) {
            return None;
        }
        let rel = addr - VIRTIO_MMIO_BASE;
        let slot = (rel / 0x1000) as usize;
        let off = rel % 0x1000;
        let dev = self.virtio.get_mut(slot)?;
        // Extract `width` bytes starting at `off`, reading the containing
        // 32-bit register(s). `read` is side-effect-free so re-reads are safe.
        let mut v: u64 = 0;
        for i in 0..width as u64 {
            let bo = off + i;
            let reg = dev.read(bo & !3);
            v |= (((reg >> (8 * (bo & 3))) & 0xFF) as u64) << (8 * i);
        }
        Some(if signed {
            match width {
                1 => v as u8 as i8 as i64 as u64,
                2 => v as u16 as i16 as i64 as u64,
                4 => v as u32 as i32 as i64 as u64,
                _ => v,
            }
        } else {
            v
        })
    }

    /// virtio-mmio store, width-aware. `None` when `addr` is not in the range.
    fn virtio_store(&mut self, addr: u64, width: u8, value: u64) -> Option<()> {
        if !(VIRTIO_MMIO_BASE..VIRTIO_MMIO_BASE + 0x8000).contains(&addr) {
            return None;
        }
        let rel = addr - VIRTIO_MMIO_BASE;
        let slot = (rel / 0x1000) as usize;
        let off = rel % 0x1000;
        let dev = self.virtio.get_mut(slot)?;
        self.irq_dirty = true;
        if width == 4 && off & 3 == 0 {
            // The register block's natural access: one aligned 32-bit write.
            dev.write(off, value as u32);
        } else {
            // Config-space partial access: read-modify-write the containing
            // word so the device sees a single write.
            let reg_addr = off & !3;
            let mut reg = dev.read(reg_addr);
            for i in 0..width.min(4) as u64 {
                let bo = off + i;
                let shift = 8 * (bo & 3);
                reg = (reg & !(0xFFu32 << shift)) | ((((value >> (8 * i)) & 0xFF) as u32) << shift);
            }
            dev.write(reg_addr, reg);
        }
        Some(())
    }

    /// Device load. `width` is in bytes (1/2/4/8). Returns a
    /// zero/sign-extended value per `signed`.
    pub fn load(&mut self, addr: u64, width: u8, signed: bool) -> Result<u64, Trap> {
        if let Some(v) = self.virtio_load(addr, width, signed) {
            return Ok(v);
        }
        let fault = || Trap::exception(CAUSE_LOAD_PAGE_FAULT, addr);
        // Byte-granular reads at each device; compose for wider accesses.
        let read_dev = |m: &mut Machine, addr: u64| -> Result<u64, Trap> {
            Ok(match addr {
                a if (UART_BASE..UART_BASE + 0x100).contains(&a) => {
                    if a == UART_BASE {
                        m.irq_dirty = true; // RBR read pops the RX queue
                    }
                    m.uart.read(a - UART_BASE) as u64
                }
                a if (ACLINT_MTIMER_BASE..ACLINT_MTIMER_BASE + 0x10000).contains(&a) => {
                    m.aclint.read(false, a - ACLINT_MTIMER_BASE)
                }
                a if (ACLINT_MSWI_BASE..ACLINT_MSWI_BASE + 0x10000).contains(&a) => {
                    m.aclint.read(true, a - ACLINT_MSWI_BASE)
                }
                a if (PLIC_BASE..PLIC_BASE + 0x400_0000).contains(&a) => {
                    let sub = a - PLIC_BASE;
                    if (0x200000..=0x201FFF).contains(&sub) && sub % 0x1000 == 4 {
                        m.irq_dirty = true; // claim-register reads clear pending
                    }
                    m.plic.read(sub)
                }
                a if (TEST_FINISHER_BASE..TEST_FINISHER_BASE + 0x1000).contains(&a) => 0,
                _ => return Err(fault()),
            })
        };
        let mut v = 0u64;
        for i in 0..width as u64 {
            v |= read_dev(self, addr.wrapping_add(i))? << (8 * i);
        }
        if signed {
            Ok(match width {
                1 => v as u8 as i8 as i64 as u64,
                2 => v as u16 as i16 as i64 as u64,
                4 => v as u32 as i32 as i64 as u64,
                _ => v,
            })
        } else {
            Ok(v)
        }
    }

    /// Device store.
    pub fn store(&mut self, addr: u64, width: u8, value: u64) -> Result<(), Trap> {
        if self.virtio_store(addr, width, value).is_some() {
            return Ok(());
        }
        self.irq_dirty = true;
        let fault = || Trap::exception(CAUSE_STORE_PAGE_FAULT, addr);
        // Width-1 decomposition for UART/ACLINT/PLIC byte-style registers.
        for i in 0..width as u64 {
            let a = addr.wrapping_add(i);
            let byte = ((value >> (8 * i)) & 0xFF) as u8;
            match a {
                a if (UART_BASE..UART_BASE + 0x100).contains(&a) => {
                    self.uart.write(a - UART_BASE, byte)
                }
                a if (ACLINT_MTIMER_BASE..ACLINT_MTIMER_BASE + 0x10000).contains(&a) => self
                    .aclint
                    .write(false, a - ACLINT_MTIMER_BASE, byte as u64),
                a if (ACLINT_MSWI_BASE..ACLINT_MSWI_BASE + 0x10000).contains(&a) => {
                    self.aclint.write(true, a - ACLINT_MSWI_BASE, byte as u64)
                }
                a if (PLIC_BASE..PLIC_BASE + 0x400_0000).contains(&a) => {
                    self.plic.write(a - PLIC_BASE, byte as u64)
                }
                a if (TEST_FINISHER_BASE..TEST_FINISHER_BASE + 0x1000).contains(&a) => {
                    // Finisher acts on 32-bit value at offset 0; only latch
                    // when the full word is written (i == 0 && width >= 4
                    // handled by the caller loop writing all 4 bytes — act
                    // on the last byte to have the full value assembled).
                    if i == width as u64 - 1 && (a - TEST_FINISHER_BASE) % 4 == 3 {
                        self.syscon_write(value as u32);
                    }
                }
                _ => return Err(fault()),
            }
        }
        Ok(())
    }

    fn syscon_write(&mut self, value: u32) {
        self.syscon.power = Some(match value {
            0x5555 => PowerAction::PowerOff { code: 0 },
            0x7777 => PowerAction::Reboot,
            v if v & 0xFFFF == 0x3333 => PowerAction::PowerOff { code: v >> 16 },
            _ => return,
        });
    }

    /// Periodic host tick: update time, feed UART RX, and compute the mip
    /// bits the devices drive (MSIP/MTIP/MEIP/SEIP). Returns (set, clear)
    /// bit masks for the caller to fold into `hart.mip`.
    pub fn tick(&mut self, rx: &[u8]) -> (u64, u64) {
        // Time is driven per-step by the hart (virtual time); this tick
        // only feeds console input and refreshes IRQ lines.
        if !rx.is_empty() {
            self.uart.push_rx(rx);
            // An RX push can newly pend the UART interrupt, so the PLIC scan
            // must run (matching the irq_dirty contract in `new`).
            self.irq_dirty = true;
        }
        self.step_irqs()
    }

    /// Recompute device-driven interrupt lines without advancing time or
    /// feeding input. Cheap enough to run per interpreter step — this is
    /// what makes MSIP/MTIP/PLIC lines visible between host ticks.
    pub fn step_irqs(&mut self) -> (u64, u64) {
        // The cheap timer/IPI lines are recomputed every step (mtime moves
        // with virtual time). The PLIC scan runs only when device state
        // changed since the last scan.
        if self.irq_dirty {
            self.plic.set_pending(UART_IRQ, self.uart.irq_pending());
            // virtio lines are level-triggered on the device's unacked
            // interrupt status.
            for dev in &self.virtio {
                self.plic.set_pending(dev.irq(), dev.interrupt_pending());
            }
            self.cached_eip = [self.plic.eip(0), self.plic.eip(1)];
            self.irq_dirty = false;
        }

        let mut set = 0u64;
        let mut clear = 0u64;
        if self.aclint.msip {
            set |= super::hart::IRQ_MSIP;
        } else {
            clear |= super::hart::IRQ_MSIP;
        }
        if self.aclint.mtip() {
            set |= super::hart::IRQ_MTIP;
        } else {
            clear |= super::hart::IRQ_MTIP;
        }
        if self.cached_eip[0] {
            set |= super::hart::IRQ_MEIP;
        } else {
            clear |= super::hart::IRQ_MEIP;
        }
        if self.cached_eip[1] {
            set |= super::hart::IRQ_SEIP;
        } else {
            clear |= super::hart::IRQ_SEIP;
        }
        (set, clear)
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

/// The FDT for this machine (QEMU `virt` shape) at the given RAM size,
/// with an optional initrd range and kernel command line.
///
/// Interrupt controller phandles: 1 = per-CPU RISC-V intc, 2 = PLIC.
pub fn virt_dtb(ram_size: u64, initrd: Option<(u64, u64)>, bootargs: &str) -> Vec<u8> {
    virt_dtb_with_virtio(ram_size, initrd, bootargs, &[])
}

/// [`virt_dtb`] plus one `virtio,mmio` node per entry in `virtio_devs`
/// (device IDs in slot order; slot `i` sits at `VIRTIO_MMIO_BASE + i*0x1000`
/// with PLIC source `1 + i`).
pub fn virt_dtb_with_virtio(
    ram_size: u64,
    initrd: Option<(u64, u64)>,
    bootargs: &str,
    virtio_devs: &[u32],
) -> Vec<u8> {
    use crate::fdt::FdtBuilder;
    const PH_INTC: u32 = 1;
    const PH_PLIC: u32 = 2;

    let mut f = FdtBuilder::new();
    f.begin_node(""); // root
    f.prop_str("model", "vane-virt");
    f.prop_str_list("compatible", &["vane,virt"]);
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);

    f.begin_node("chosen");
    f.prop_str("bootargs", bootargs);
    if let Some((start, end)) = initrd {
        f.prop_u64("linux,initrd-start", start);
        f.prop_u64("linux,initrd-end", end);
    }
    f.end_node();

    f.begin_node("memory@80000000");
    f.prop_str("device_type", "memory");
    f.prop_reg("reg", RAM_BASE, ram_size);
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.prop_u32("timebase-frequency", 10_000_000);
    f.begin_node("cpu@0");
    f.prop_str("compatible", "riscv");
    f.prop_str("device_type", "cpu");
    // The hart ID — OpenSBI matches mhartid against this table; without it
    // the boot hart is never found and the firmware parks in wfi.
    f.prop_u32("reg", 0);
    f.prop_str("status", "okay");
    f.prop_str("riscv,isa", "rv64imafdc");
    f.prop_str("mmu-type", "riscv,sv39");
    f.begin_node("interrupt-controller");
    f.prop_u32("#interrupt-cells", 1);
    f.prop_flag("interrupt-controller");
    f.prop_str_list("compatible", &["riscv,cpu-intc"]);
    f.prop_u32("phandle", PH_INTC);
    f.end_node(); // cpu intc
    f.end_node(); // cpu@0
    f.end_node(); // cpus

    f.begin_node("soc");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str_list("compatible", &["simple-bus"]);
    f.prop_flag("ranges");

    // ns16550a UART @ 0x1000_0000, PLIC source 10
    f.begin_node("uart@10000000");
    f.prop_str_list("compatible", &["ns16550a"]);
    f.prop_reg("reg", UART_BASE, 0x100);
    f.prop_u32("clock-frequency", 1_843_200);
    f.prop_cells("interrupts", &[UART_IRQ]);
    f.prop_u32("interrupt-parent", PH_PLIC);
    f.end_node();

    // SiFive test finisher (poweroff/reboot) @ 0x10_0000 — OpenSBI's
    // generic platform drives this for SBI SRST via the syscon-poweroff /
    // syscon-reboot helper nodes (regmap → the finisher's phandle).
    const PH_TEST: u32 = 3;
    f.begin_node("test@100000");
    f.prop_str_list("compatible", &["sifive,test1", "sifive,test0", "syscon"]);
    f.prop_reg("reg", TEST_FINISHER_BASE, 0x1000);
    f.prop_u32("phandle", PH_TEST);
    f.end_node();
    f.begin_node("poweroff");
    f.prop_str_list("compatible", &["syscon-poweroff"]);
    f.prop_u32("regmap", PH_TEST);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x5555);
    f.end_node();
    f.begin_node("reboot");
    f.prop_str_list("compatible", &["syscon-reboot"]);
    f.prop_u32("regmap", PH_TEST);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x7777);
    f.end_node();

    // PLIC @ 0xC00_0000: M-mode external (irq 11) and S-mode external
    // (irq 9) both wired to the CPU intc.
    f.begin_node("plic@c000000");
    f.prop_str_list("compatible", &["riscv,plic0"]);
    f.prop_reg("reg", PLIC_BASE, 0x400_0000);
    f.prop_u32("#interrupt-cells", 1);
    f.prop_flag("interrupt-controller");
    f.prop_u32("phandle", PH_PLIC);
    f.prop_cells("interrupts-extended", &[PH_INTC, 11, PH_INTC, 9]);
    f.prop_u32("riscv,ndev", 32);
    f.end_node();

    // CLINT @ 0x200_0000 (msip @ +0x0, mtimecmp @ +0x4000, mtime @
    // +0xBFF8) — the exact shape QEMU's virt machine exposes, which this
    // kernel config is known to drive. (ACLINT is register-compatible; we
    // serve msip at both 0x200_0000 and 0x2F0_0000.)
    // virtio-mmio devices @ 0x1000_1000 + slot*0x1000, PLIC source 1+slot.
    for (slot, _dev_id) in virtio_devs.iter().enumerate() {
        let base = VIRTIO_MMIO_BASE + slot as u64 * 0x1000;
        let name = alloc::format!("virtio_mmio@{base:x}");
        f.begin_node(&name);
        f.prop_str_list("compatible", &["virtio,mmio"]);
        f.prop_reg("reg", base, 0x1000);
        f.prop_cells("interrupts", &[1 + slot as u32]);
        f.prop_u32("interrupt-parent", PH_PLIC);
        f.end_node();
    }

    f.begin_node("clint@2000000");
    f.prop_str_list("compatible", &["sifive,clint0", "riscv,clint0"]);
    f.prop_reg("reg", ACLINT_MTIMER_BASE, 0x10000);
    // interrupts-extended: msip (3) and mtip (7) on the CPU intc
    f.prop_cells("interrupts-extended", &[PH_INTC, 3, PH_INTC, 7]);
    f.end_node();

    f.end_node(); // soc
    f.end_node(); // root
    f.finish()
}

// ── Snapshot serialization (W1.5) ────────────────────────────────────────────
use crate::snapshot_ser::{Reader, SerError, Writer};

impl Uart16550 {
    fn encode(&self, w: &mut Writer) {
        w.u8(self.ier);
        w.u8(self.lcr);
        w.u8(self.mcr);
        w.u8(self.fcr);
        w.u8(self.scr);
        w.u8(self.dll);
        w.u8(self.dlm);
        w.u32(self.rx.len() as u32);
        for b in &self.rx {
            w.u8(*b);
        }
        w.u32(self.tx.len() as u32);
        for b in &self.tx {
            w.u8(*b);
        }
    }
    fn decode(r: &mut Reader) -> Result<Uart16550, SerError> {
        let (ier, lcr, mcr, fcr, scr, dll, dlm) = (
            r.u8()?,
            r.u8()?,
            r.u8()?,
            r.u8()?,
            r.u8()?,
            r.u8()?,
            r.u8()?,
        );
        let mut rx = VecDeque::new();
        for _ in 0..r.u32()? {
            rx.push_back(r.u8()?);
        }
        let mut tx = Vec::new();
        for _ in 0..r.u32()? {
            tx.push(r.u8()?);
        }
        Ok(Uart16550 {
            ier,
            lcr,
            mcr,
            fcr,
            scr,
            dll,
            dlm,
            rx,
            tx,
        })
    }
}

impl Aclint {
    fn encode(&self, w: &mut Writer) {
        w.bool(self.msip);
        w.u64(self.mtimecmp);
        w.u64(self.time_nanos);
        w.u64(self.freq_hz);
    }
    fn decode(r: &mut Reader) -> Result<Aclint, SerError> {
        Ok(Aclint {
            msip: r.bool()?,
            mtimecmp: r.u64()?,
            time_nanos: r.u64()?,
            freq_hz: r.u64()?,
        })
    }
}

impl Plic {
    fn encode(&self, w: &mut Writer) {
        for p in self.priority {
            w.u32(p);
        }
        w.u32(self.pending);
        for e in self.enable {
            w.u32(e);
        }
        for t in self.threshold {
            w.u32(t);
        }
    }
    fn decode(r: &mut Reader) -> Result<Plic, SerError> {
        let mut priority = [0u32; 32];
        for p in &mut priority {
            *p = r.u32()?;
        }
        let pending = r.u32()?;
        let mut enable = [0u32; 2];
        for e in &mut enable {
            *e = r.u32()?;
        }
        let mut threshold = [0u32; 2];
        for t in &mut threshold {
            *t = r.u32()?;
        }
        Ok(Plic {
            priority,
            pending,
            enable,
            threshold,
        })
    }
}

impl Syscon {
    fn encode(&self, w: &mut Writer) {
        match self.power {
            None => w.u8(0),
            Some(PowerAction::PowerOff { code }) => {
                w.u8(1);
                w.u32(code);
            }
            Some(PowerAction::Reboot) => w.u8(2),
        }
    }
    fn decode(r: &mut Reader) -> Result<Syscon, SerError> {
        Ok(Syscon {
            power: match r.u8()? {
                0 => None,
                1 => Some(PowerAction::PowerOff { code: r.u32()? }),
                2 => Some(PowerAction::Reboot),
                _ => return Err(SerError::Invalid("syscon power")),
            },
        })
    }
}

impl MachineSnapshot {
    pub fn encode(&self, w: &mut Writer) {
        self.uart.encode(w);
        self.aclint.encode(w);
        self.plic.encode(w);
        self.syscon.encode(w);
        w.u32(self.virtio.len() as u32);
        for v in &self.virtio {
            v.encode(w);
        }
        w.bool(self.irq_dirty);
    }
    pub fn decode(r: &mut Reader) -> Result<MachineSnapshot, SerError> {
        let uart = Uart16550::decode(r)?;
        let aclint = Aclint::decode(r)?;
        let plic = Plic::decode(r)?;
        let syscon = Syscon::decode(r)?;
        let mut virtio = Vec::new();
        for _ in 0..r.u32()? {
            virtio.push(crate::virtio::VirtioMmioSnapshot::decode(r)?);
        }
        let irq_dirty = r.bool()?;
        Ok(MachineSnapshot {
            uart,
            aclint,
            plic,
            syscon,
            virtio,
            irq_dirty,
        })
    }
}

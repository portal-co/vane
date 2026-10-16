//! RISC-V privileged architecture state and semantics (M3 of
//! `PLAN_LINUX_WASM.md` §7.2–7.3).
//!
//! [`HartState`] holds the privilege mode, the CSR file, and the Sv39 TLB,
//! and implements the trap engine (synchronous exceptions, interrupts,
//! delegation), `mret`/`sret`, `sfence.vma`, and the Sv39 page-table walk
//! with hardware A/D updates.
//!
//! The MMU walker reads and writes guest-physical memory through
//! [`vane_arch::Mem`]; page-table pages are ordinary guest pages (the
//! swap-layer pinning invariant is M5's concern and noted in the plan).

use alloc::collections::BTreeMap;
use vane_arch::Mem;

/// Privilege modes. Numeric values match the spec encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrivMode {
    U = 0,
    S = 1,
    M = 3,
}

impl PrivMode {
    pub fn from_u64(v: u64) -> Option<Self> {
        match v {
            0 => Some(Self::U),
            1 => Some(Self::S),
            3 => Some(Self::M),
            _ => None,
        }
    }
}

/// Interrupt bits in `mip`/`mie`.
pub const IRQ_SSIP: u64 = 1 << 1;
pub const IRQ_MSIP: u64 = 1 << 3;
pub const IRQ_STIP: u64 = 1 << 5;
pub const IRQ_MTIP: u64 = 1 << 7;
pub const IRQ_SEIP: u64 = 1 << 9;
pub const IRQ_MEIP: u64 = 1 << 11;

/// All interrupt bits implemented by this hart.
pub const IRQ_ALL: u64 = IRQ_SSIP | IRQ_MSIP | IRQ_STIP | IRQ_MTIP | IRQ_SEIP | IRQ_MEIP;
/// Interrupts software-writable by M-mode in `mip` (device-owned bits like
/// MTIP/MSIP/MEIP are driven by devices — M4).
pub const IRQ_M_WRITABLE: u64 = IRQ_SSIP | IRQ_STIP | IRQ_SEIP;
/// S-level interrupt bits.
pub const IRQ_S: u64 = IRQ_SSIP | IRQ_STIP | IRQ_SEIP;

/// Synchronous exception causes.
pub const CAUSE_MISALIGNED_FETCH: u64 = 0;
pub const CAUSE_FETCH_ACCESS: u64 = 1;
pub const CAUSE_ILLEGAL: u64 = 2;
pub const CAUSE_BREAKPOINT: u64 = 3;
pub const CAUSE_MISALIGNED_LOAD: u64 = 4;
pub const CAUSE_LOAD_ACCESS: u64 = 5;
pub const CAUSE_MISALIGNED_STORE: u64 = 6;
pub const CAUSE_STORE_ACCESS: u64 = 7;
pub const CAUSE_ECALL_U: u64 = 8;
pub const CAUSE_ECALL_S: u64 = 9;
pub const CAUSE_ECALL_M: u64 = 11;
pub const CAUSE_FETCH_PAGE_FAULT: u64 = 12;
pub const CAUSE_LOAD_PAGE_FAULT: u64 = 13;
pub const CAUSE_STORE_PAGE_FAULT: u64 = 15;

/// A trap to be raised through [`HartState::raise`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trap {
    pub cause: u64,
    pub tval: u64,
    pub interrupt: bool,
}

impl Trap {
    pub const fn exception(cause: u64, tval: u64) -> Self {
        Self {
            cause,
            tval,
            interrupt: false,
        }
    }
}

/// Memory access kind for translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Load,
    Store,
    Exec,
}

impl Access {
    fn page_fault_cause(self) -> u64 {
        match self {
            Access::Load => CAUSE_LOAD_PAGE_FAULT,
            Access::Store => CAUSE_STORE_PAGE_FAULT,
            Access::Exec => CAUSE_FETCH_PAGE_FAULT,
        }
    }
}

/// CSR operation kind (Zicsr).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsrOp {
    Write,
    Set,
    Clear,
}

// mstatus field bits.
pub const MSTATUS_SIE: u64 = 1 << 1;
pub const MSTATUS_MIE: u64 = 1 << 3;
pub const MSTATUS_SPIE: u64 = 1 << 5;
pub const MSTATUS_UBE: u64 = 1 << 6;
pub const MSTATUS_MPIE: u64 = 1 << 7;
pub const MSTATUS_SPP: u64 = 1 << 8;
pub const MSTATUS_FS: u64 = 0b11 << 13;
pub const MSTATUS_XS: u64 = 0b11 << 15;
pub const MSTATUS_MPRV: u64 = 1 << 17;
pub const MSTATUS_SUM: u64 = 1 << 18;
pub const MSTATUS_MXR: u64 = 1 << 19;
pub const MSTATUS_TVM: u64 = 1 << 20;
pub const MSTATUS_TW: u64 = 1 << 21;
pub const MSTATUS_TSR: u64 = 1 << 22;
pub const MSTATUS_UXL: u64 = 0b11 << 32;
pub const MSTATUS_SD: u64 = 1 << 63;

/// Bits of mstatus implemented (writable) by this hart.
const MSTATUS_WMASK: u64 = MSTATUS_SIE
    | MSTATUS_MIE
    | MSTATUS_SPIE
    | MSTATUS_MPIE
    | MSTATUS_UBE
    | MSTATUS_SPP
    | (0b11 << 11) // MPP
    | MSTATUS_FS
    | MSTATUS_XS
    | MSTATUS_MPRV
    | MSTATUS_SUM
    | MSTATUS_MXR
    | MSTATUS_TVM
    | MSTATUS_TW
    | MSTATUS_TSR
    | MSTATUS_UXL;

/// Bits of mstatus visible/writable through sstatus.
const SSTATUS_MASK: u64 = MSTATUS_SIE
    | MSTATUS_SPIE
    | MSTATUS_UBE
    | MSTATUS_SPP
    | MSTATUS_FS
    | MSTATUS_XS
    | MSTATUS_SUM
    | MSTATUS_MXR
    | MSTATUS_UXL;

/// misa: RV64 with A, C, D, F, I, M, S, U.
pub const MISA_VALUE: u64 = (2 << 62)
    | (1 << 0) // A
    | (1 << 2) // C
    | (1 << 3) // D
    | (1 << 5) // F
    | (1 << 8) // I
    | (1 << 12) // M
    | (1 << 18) // S
    | (1 << 20); // U

/// satp mode values.
pub const SATP_MODE_BARE: u64 = 0;
pub const SATP_MODE_SV39: u64 = 8;

struct TlbEntry {
    /// Physical page number (already shifted: physical address of the page).
    pa_base: u64,
    flags: u8, // R W X U G as in the PTE low bits
    /// log2 of the page size (12 for 4 KiB, 21 for 2 MiB, 30 for 1 GiB).
    shift: u8,
}

/// Privileged architectural state of one hart.
///
/// When `enabled` is false the emulator runs in the legacy
/// user-mode-ABI-only mode: no CSR/trap semantics, `ecall` forwards to the
/// host syscall hook. When true, the full privileged architecture is live
/// and `ecall` raises the appropriate environment-call exception.
pub struct HartState {
    pub enabled: bool,
    pub priv_mode: PrivMode,

    // M-mode CSRs.
    pub mstatus: u64,
    pub mie: u64,
    pub mip: u64,
    pub mtvec: u64,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mcounteren: u64,
    pub menvcfg: u64,
    pub mcountinhibit: u64,
    pub mseccfg: u64,
    pub mtval2: u64,
    pub mtinst: u64,

    // S-mode CSRs (sstatus/sie/sip are views of the M-mode registers).
    pub stvec: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub satp: u64,
    pub scounteren: u64,

    // PMP (WARL passthrough; enforcement is R4 scope).
    pub pmpcfg: [u64; 8],
    pub pmpaddr: [u64; 64],

    // Counters.
    pub mcycle: u64,
    pub minstret: u64,

    tlb: BTreeMap<(u64, u64), TlbEntry>,

    /// LR/SC reservation: the physical address of the outstanding
    /// load-reserved (single-hart model). Cleared by trap entry and SC.
    pub reservation: Option<u64>,

    /// Floating-point register file (raw bits; F32 values NaN-boxed) and
    /// fcsr (fflags in bits 0-4, frm in bits 5-7).
    pub fregs: [u64; 32],
    pub fcsr: u64,

    /// Internal always-advancing instruction-time counter (not a CSR, not
    /// affected by mcountinhibit) — drives the machine's virtual mtime.
    /// Driving mtime from `minstret` breaks when firmware masks the counter
    /// (OpenSBI does), freezing time and starving the guest's timer.
    pub itime: u64,

    /// Decode memoization: a direct-mapped cache from instruction word to
    /// decoded instruction. The mapping is a pure function of the word, so
    /// entries never need invalidation (self-modifying code writes a
    /// *different* word). Saves the decode match on hot loops.
    pub decode_cache: [Option<(u32, rv_asm::Inst, rv_asm::IsCompressed)>; 1024],

    /// A memory access inside a trace trapped (page/access fault). The trace
    /// bails at the next instruction boundary and the dispatch loop re-runs
    /// the faulting instruction through the stepper, which re-raises the
    /// trap with correct architectural state.
    pub pending_trace_trap: bool,

    /// A device (MMIO) access ran inside a trace — an interrupt may newly be
    /// pending. The trace exits at the next instruction boundary so the
    /// dispatch loop can deliver it (interrupts are taken between
    /// instructions, not mid-trace).
    pub pending_irq_check: bool,

    /// The machine (devices) attached to this hart, or None for pure-RAM
    /// operation (legacy userspace emulation). When present, physical
    /// accesses below [`crate::machine::RAM_BASE`] route to devices and the
    /// trace engine steps every instruction instead of tracing (device
    /// semantics cannot be captured in precompiled traces).
    pub machine: Option<crate::machine::Machine>,
}

impl Default for HartState {
    fn default() -> Self {
        Self {
            enabled: false,
            priv_mode: PrivMode::M,
            mstatus: 0,
            mie: 0,
            mip: 0,
            mtvec: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            medeleg: 0,
            mideleg: 0,
            mcounteren: 0,
            menvcfg: 0,
            mcountinhibit: 0,
            mseccfg: 0,
            mtval2: 0,
            mtinst: 0,
            stvec: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            satp: 0,
            scounteren: 0,
            pmpcfg: [0; 8],
            pmpaddr: [0; 64],
            mcycle: 0,
            minstret: 0,
            tlb: BTreeMap::new(),
            reservation: None,
            fregs: [0; 32],
            fcsr: 0,
            itime: 0,
            decode_cache: [const { None }; 1024],
            pending_trace_trap: false,
            pending_irq_check: false,
            machine: None,
        }
    }
}

impl HartState {
    /// Read mstatus with the computed SD bit folded in.
    pub fn mstatus_read(&self) -> u64 {
        let fs_dirty = self.mstatus & MSTATUS_FS == MSTATUS_FS;
        let xs_dirty = self.mstatus & MSTATUS_XS == MSTATUS_XS;
        let sd = if fs_dirty || xs_dirty { MSTATUS_SD } else { 0 };
        (self.mstatus & !MSTATUS_SD) | sd
    }

    /// sstatus view of mstatus.
    pub fn sstatus_read(&self) -> u64 {
        self.mstatus_read() & SSTATUS_MASK
    }

    /// True when supervisor virtual memory translation applies to the
    /// current privilege mode.
    pub fn translation_active(&self) -> bool {
        self.enabled && self.priv_mode != PrivMode::M && (self.satp >> 60) != SATP_MODE_BARE
    }

    /// Check a pending, enabled, and appropriately-delegated interrupt.
    /// Returns the interrupt cause number if one should be taken now.
    pub fn pending_interrupt(&self) -> Option<u64> {
        let pending = self.mip & self.mie;
        if pending == 0 {
            return None;
        }
        // Priority order per the privileged spec: MEI, MSI, MTI, SEI, SSI, STI.
        const ORDER: [(u64, u64); 6] = [
            (IRQ_MEIP, 11),
            (IRQ_MSIP, 3),
            (IRQ_MTIP, 7),
            (IRQ_SEIP, 9),
            (IRQ_SSIP, 1),
            (IRQ_STIP, 5),
        ];
        // M-mode: interrupts not delegated to S are taken in M when the
        // current mode is below M or M's global enable is set.
        let m_pending = pending & !self.mideleg;
        if m_pending != 0 && (self.priv_mode < PrivMode::M || self.mstatus & MSTATUS_MIE != 0) {
            for (bit, cause) in ORDER {
                if m_pending & bit != 0 {
                    return Some(cause);
                }
            }
        }
        // S-mode: delegated interrupts are taken in S only when the current
        // mode is S or below (never in M — they pend until the next mret),
        // and then only when below S or S's global enable is set.
        let s_pending = pending & self.mideleg;
        if self.priv_mode <= PrivMode::S
            && s_pending != 0
            && (self.priv_mode < PrivMode::S || self.mstatus & MSTATUS_SIE != 0)
        {
            for (bit, cause) in ORDER {
                if s_pending & bit != 0 {
                    return Some(cause);
                }
            }
        }
        None
    }

    /// Raise a trap, applying delegation. Returns the new pc (the trap
    /// vector address).
    pub fn raise(&mut self, trap: Trap, pc: u64) -> u64 {
        // Trap entry clears the LR/SC reservation (spec requirement).
        self.reservation = None;
        let cause_field = trap.cause | ((trap.interrupt as u64) << 63);
        let deleg = if trap.interrupt {
            self.mideleg
        } else {
            self.medeleg
        };
        let to_s = self.priv_mode <= PrivMode::S && (deleg >> trap.cause) & 1 == 1;
        if to_s {
            // Enter S-mode.
            let sie = self.mstatus & MSTATUS_SIE != 0;
            self.mstatus &= !(MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP);
            if sie {
                self.mstatus |= MSTATUS_SPIE;
            }
            if self.priv_mode == PrivMode::S {
                self.mstatus |= MSTATUS_SPP;
            }
            self.priv_mode = PrivMode::S;
            self.scause = cause_field;
            self.sepc = pc;
            self.stval = trap.tval;
            let base = self.stvec & !0b11;
            if self.stvec & 0b11 == 1 && trap.interrupt {
                base + 4 * trap.cause
            } else {
                base
            }
        } else {
            // Enter M-mode.
            let mie = self.mstatus & MSTATUS_MIE != 0;
            self.mstatus &= !(MSTATUS_MIE | MSTATUS_MPIE | (0b11 << 11));
            if mie {
                self.mstatus |= MSTATUS_MPIE;
            }
            let mpp = self.priv_mode as u64;
            self.mstatus |= mpp << 11;
            self.priv_mode = PrivMode::M;
            self.mcause = cause_field;
            self.mepc = pc;
            self.mtval = trap.tval;
            let base = self.mtvec & !0b11;
            if self.mtvec & 0b11 == 1 && trap.interrupt {
                base + 4 * trap.cause
            } else {
                base
            }
        }
    }

    /// `mret`: restore privilege and interrupt state, return the new pc.
    pub fn mret(&mut self) -> u64 {
        let mpp = (self.mstatus >> 11) & 0b11;
        let mpie = self.mstatus & MSTATUS_MPIE != 0;
        self.mstatus &= !(MSTATUS_MIE | MSTATUS_MPIE | (0b11 << 11) | MSTATUS_MPRV);
        if mpie {
            self.mstatus |= MSTATUS_MIE;
        }
        self.mstatus |= MSTATUS_MPIE; // MPIE := 1
        // MPP := U (the least privilege we support)
        let new_priv = PrivMode::from_u64(mpp).unwrap_or(PrivMode::U);
        self.priv_mode = new_priv;
        // MPRV cleared when leaving M (already cleared above).
        self.mepc
    }

    /// `sret`: restore privilege and interrupt state, return the new pc.
    pub fn sret(&mut self) -> u64 {
        let spp = self.mstatus & MSTATUS_SPP != 0;
        let spie = self.mstatus & MSTATUS_SPIE != 0;
        self.mstatus &= !(MSTATUS_SIE | MSTATUS_SPIE | MSTATUS_SPP);
        if spie {
            self.mstatus |= MSTATUS_SIE;
        }
        self.mstatus |= MSTATUS_SPIE; // SPIE := 1
        // SPP := 0 (U)
        self.priv_mode = if spp { PrivMode::S } else { PrivMode::U };
        self.sepc
    }

    /// `sfence.vma`: flush TLB entries matching vaddr/asid (x0 = all).
    pub fn sfence_vma(&mut self, vaddr: u64, asid: u64) {
        let all_addr = vaddr == 0;
        let all_asid = asid == 0;
        if all_addr && all_asid {
            self.tlb.clear();
            return;
        }
        let vpn = vaddr >> 12;
        // Flush entries whose (vpn range, asid) both match. G(lobal) entries
        // are flushed too — over-eager but always safe.
        self.tlb.retain(|&(v, a), e| {
            let span_pages = 1u64 << (e.shift.saturating_sub(12));
            let addr_match = all_addr || (vpn >= v && vpn < v + span_pages);
            let asid_match = all_asid || a == asid;
            !(addr_match && asid_match)
        });
    }

    /// Flush the whole TLB (satp writes).
    pub fn tlb_flush(&mut self) {
        self.tlb.clear();
    }

    // ── CSR access ────────────────────────────────────────────────────────

    fn csr_privilege_ok(&self, csr: u16) -> bool {
        let required = (csr >> 8) & 0b11;
        (self.priv_mode as u16) >= required
    }

    /// Execute a Zicsr operation. `value` is the rs1 (or zimm) operand;
    /// `write` is false for CSRRS/CSRRC with rs1 = x0 (no side effects).
    /// Returns the old CSR value, or an illegal-instruction trap.
    pub fn csr_op(&mut self, op: CsrOp, csr: u16, value: u64, write: bool) -> Result<u64, Trap> {
        let illegal = || Trap::exception(CAUSE_ILLEGAL, 0);
        if !self.csr_privilege_ok(csr) {
            return Err(illegal());
        }
        let read_only = (csr >> 10) & 0b11 == 0b11;
        if write && read_only {
            return Err(illegal());
        }
        // satp access from S with TVM=1 traps.
        if csr == 0x180 && self.priv_mode == PrivMode::S && self.mstatus & MSTATUS_TVM != 0 {
            return Err(illegal());
        }

        let old = self.csr_read(csr).ok_or_else(illegal)?;
        if !write {
            return Ok(old);
        }
        let new = match op {
            CsrOp::Write => value,
            CsrOp::Set => old | value,
            CsrOp::Clear => old & !value,
        };
        self.csr_write(csr, new).ok_or_else(illegal)?;
        Ok(old)
    }

    fn csr_read(&self, csr: u16) -> Option<u64> {
        // FP CSRs are only accessible when mstatus.FS != 0.
        if matches!(csr, 0x001..=0x003) && self.mstatus & MSTATUS_FS == 0 {
            return None;
        }
        // Counter CSRs (cycle/time/instret) are gated by mcounteren when
        // accessed below M, and additionally by scounteren from U.
        if matches!(csr, 0xC00..=0xC02) {
            let bit = 1u64 << (csr - 0xC00);
            if self.priv_mode < PrivMode::M && self.mcounteren & bit == 0 {
                return None;
            }
            if self.priv_mode == PrivMode::U && self.scounteren & bit == 0 {
                return None;
            }
        }
        Some(match csr {
            0x300 => self.mstatus_read(),
            0x301 => MISA_VALUE,
            0x302 => self.medeleg,
            0x303 => self.mideleg,
            0x304 => self.mie,
            0x305 => self.mtvec,
            0x306 => self.mcounteren,
            0x30A => self.menvcfg,
            0x320 => self.mcountinhibit,
            0x340 => self.mscratch,
            0x341 => self.mepc,
            0x342 => self.mcause,
            0x343 => self.mtval,
            0x344 => self.mip,
            0x34A => self.mtinst,
            0x34B => self.mtval2,
            0x747 => self.mseccfg,
            0xF11 => 0, // mvendorid
            0xF12 => 0, // marchid
            0xF13 => 0, // mimpid
            0xF14 => 0, // mhartid (single hart)
            0x100 => self.sstatus_read(),
            0x104 => self.mie & self.mideleg, // sie
            0x105 => self.stvec,
            0x106 => self.scounteren,
            0x10A => 0, // senvcfg
            0x140 => self.sscratch,
            0x141 => self.sepc,
            0x142 => self.scause,
            0x143 => self.stval,
            0x144 => self.mip & self.mideleg, // sip
            0x180 => self.satp,
            // cycle: with a machine attached, track mtime — the kernel
            // mixes rdcycle and mtime in its clockevent math, so they must
            // share a rate. Without a machine, mcycle counts instructions.
            0xC00 => self
                .machine
                .as_ref()
                .map(|m| m.aclint.mtime())
                .unwrap_or(self.mcycle),
            // time: the machine's ACLINT mtime (host-driven); 0 without a
            // machine. Linux's rdtime-based calibration loops need this to
            // advance.
            0xC01 => self
                .machine
                .as_ref()
                .map(|m| m.aclint.mtime())
                .unwrap_or(0),
            0xC02 => self.minstret, // instret
            0x001 => self.fcsr & 0x1F,     // fflags
            0x002 => (self.fcsr >> 5) & 7, // frm
            0x003 => self.fcsr & 0xFF,     // fcsr
            0x3A0..=0x3AF if csr.is_multiple_of(2) => self.pmpcfg[((csr - 0x3A0) / 2) as usize],
            0x3B0..=0x3EF => self.pmpaddr[(csr - 0x3B0) as usize],
            _ => return None,
        })
    }

    fn csr_write(&mut self, csr: u16, value: u64) -> Option<()> {
        // FP CSRs are only accessible when mstatus.FS != 0.
        if matches!(csr, 0x001..=0x003) && self.mstatus & MSTATUS_FS == 0 {
            return None;
        }
        match csr {
            0x300 => self.mstatus = value & MSTATUS_WMASK,
            0x301 => {}                                 // misa: WARL, ignore writes
            0x302 => self.medeleg = value & !(1 << 11), // ecall-from-M not delegatable
            0x303 => self.mideleg = value & IRQ_S,
            0x304 => self.mie = value & IRQ_ALL,
            0x305 => self.mtvec = value & !0b01, // allow direct/vectored, 4-aligned base
            0x306 => self.mcounteren = value & 0b111,
            0x30A => self.menvcfg = value & 1, // FIOM only; STCE etc. read 0
            0x320 => self.mcountinhibit = value & 0b101,
            0x340 => self.mscratch = value,
            0x341 => self.mepc = value & !1,
            0x342 => self.mcause = value,
            0x343 => self.mtval = value,
            0x344 => self.mip = (self.mip & !IRQ_M_WRITABLE) | (value & IRQ_M_WRITABLE),
            0x34A => self.mtinst = value,
            0x34B => self.mtval2 = value,
            0x747 => {} // mseccfg: we implement no PMP locks/ePMP; WARL zero
            0x100 => {
                self.mstatus = (self.mstatus & !SSTATUS_MASK) | (value & SSTATUS_MASK);
            }
            0x104 => {
                // sie: S-writable bits of mie that are delegated to S
                let w = self.mideleg & IRQ_S;
                self.mie = (self.mie & !w) | (value & w);
            }
            0x105 => self.stvec = value & !0b01,
            0x106 => self.scounteren = value & 0b111,
            0x140 => self.sscratch = value,
            0x141 => self.sepc = value & !1,
            0x142 => self.scause = value,
            0x143 => self.stval = value,
            0x144 => {
                // sip: only SSIP is S-writable
                if self.mideleg & IRQ_SSIP != 0 {
                    self.mip = (self.mip & !IRQ_SSIP) | (value & IRQ_SSIP);
                }
            }
            0x180 => {
                // satp: accept Bare or Sv39 (WARL; other modes ignored)
                let mode = value >> 60;
                if mode == SATP_MODE_BARE || mode == SATP_MODE_SV39 {
                    if self.satp != value {
                        self.tlb.clear();
                    }
                    self.satp = value;
                }
            }
            0x001 => self.fcsr = (self.fcsr & !0x1F) | (value & 0x1F),
            0x002 => self.fcsr = (self.fcsr & !0xE0) | ((value & 7) << 5),
            0x003 => self.fcsr = value & 0xFF,
            0xC00 => self.mcycle = value,
            0xC02 => self.minstret = value,
            0x3A0..=0x3AF if csr.is_multiple_of(2) => {
                self.pmpcfg[((csr - 0x3A0) / 2) as usize] = value;
            }
            0x3B0..=0x3EF => {
                self.pmpaddr[(csr - 0x3B0) as usize] = value;
            }
            _ => return None,
        }
        Some(())
    }

    /// Whether the machine's syscon fired (poweroff/reboot). The trace
    /// dispatch loop checks this per dispatch to stop a powered-off guest.
    pub fn power_pending(&self) -> bool {
        self.machine
            .as_ref()
            .map(|m| m.syscon.power.is_some())
            .unwrap_or(false)
    }

    /// Per-step refresh of device-driven interrupt lines (no host IO).
    /// Runs in `step_one` before the pending-interrupt check.
    pub fn machine_step_irqs(&mut self) {
        if let Some(m) = self.machine.as_mut() {
            // Virtual time: mtime tracks retired instructions (100ns per
            // instruction — a nominal 10 MHz timebase at 10 Minstr/s).
            // Real-time clocks race ahead of a slow interpreter and turn
            // the kernel's timer into an interrupt storm; virtual time
            // keeps the guest self-consistent regardless of host speed.
            m.aclint.time_nanos = self.itime.saturating_mul(100);
            let (set, clear) = m.step_irqs();
            self.mip = (self.mip | set) & !clear;
        }
    }

    /// Host tick for the machine (devices): drive time, feed UART RX,
    /// fold device-driven interrupt lines into `mip`. Returns a power
    /// action if the syscon fired.
    pub fn machine_tick(
        &mut self,
        rx: &[u8],
    ) -> Option<crate::machine::PowerAction> {
        let m = self.machine.as_mut()?;
        let (set, clear) = m.tick(rx);
        self.mip = (self.mip | set) & !clear;
        m.syscon.power
    }

    // ── Sv39 translation ──────────────────────────────────────────────────

    /// Translate a virtual address to a guest-physical address. Identity in
    /// M-mode and when satp is Bare. Raises page-fault traps on failure.
    pub fn translate(&mut self, mem: &mut Mem, vaddr: u64, access: Access) -> Result<u64, Trap> {
        if !self.translation_active() {
            return Ok(vaddr);
        }
        // Canonical check: bits 63:39 must all equal bit 38.
        let sign_extended = ((vaddr << 25) as i64 >> 25) as u64;
        if sign_extended != vaddr {
            return Err(Trap::exception(access.page_fault_cause(), vaddr));
        }
        let asid = (self.satp >> 44) & 0xFFFF;
        let vpn = vaddr >> 12;

        if let Some(entry) = self.tlb.get(&(vpn, asid)) {
            if self.tlb_permits(entry.flags, access) {
                let offset = vaddr & ((1 << entry.shift) - 1);
                return Ok(entry.pa_base + offset);
            }
            return Err(Trap::exception(access.page_fault_cause(), vaddr));
        }

        self.sv39_walk(mem, vaddr, asid, access)
    }

    /// Translate a data-load address (identity when translation is off).
    pub fn translate_load(&mut self, mem: &mut Mem, vaddr: u64) -> Result<u64, Trap> {
        self.translate(mem, vaddr, Access::Load)
    }

    /// Translate a data-store address.
    pub fn translate_store(&mut self, mem: &mut Mem, vaddr: u64) -> Result<u64, Trap> {
        self.translate(mem, vaddr, Access::Store)
    }

    /// Translate an instruction-fetch address.
    pub fn translate_exec(&mut self, mem: &mut Mem, vaddr: u64) -> Result<u64, Trap> {
        self.translate(mem, vaddr, Access::Exec)
    }

    /// Device read at a physical address: `None` when no machine is attached
    /// or the address is RAM, `Some(Ok(v))` for a device value,
    /// `Some(Err(_))` for an access fault.
    pub fn machine_load(
        &mut self,
        pa: u64,
        width: vane_arch::MemWidth,
        signed: bool,
    ) -> Option<Result<u64, Trap>> {
        let m = self.machine.as_mut()?;
        if !crate::machine::Machine::is_mmio(pa) {
            return None;
        }
        let w = match width {
            vane_arch::MemWidth::B1 => 1,
            vane_arch::MemWidth::B2 => 2,
            vane_arch::MemWidth::B4 => 4,
            vane_arch::MemWidth::B8 => 8,
        };
        self.pending_irq_check = true;
        Some(m.load(pa, w, signed))
    }

    /// Device write at a physical address; see [`HartState::machine_load`].
    pub fn machine_store(
        &mut self,
        pa: u64,
        width: vane_arch::MemWidth,
        v: u64,
    ) -> Option<Result<(), Trap>> {
        let m = self.machine.as_mut()?;
        if !crate::machine::Machine::is_mmio(pa) {
            return None;
        }
        let w = match width {
            vane_arch::MemWidth::B1 => 1,
            vane_arch::MemWidth::B2 => 2,
            vane_arch::MemWidth::B4 => 4,
            vane_arch::MemWidth::B8 => 8,
        };
        self.pending_irq_check = true;
        Some(m.store(pa, w, v))
    }

    fn tlb_permits(&self, flags: u8, access: Access) -> bool {
        let (r, w, x, u) = (
            flags & 1 != 0,
            flags & 2 != 0,
            flags & 4 != 0,
            flags & 8 != 0,
        );
        let mxr = self.mstatus & MSTATUS_MXR != 0;
        let sum = self.mstatus & MSTATUS_SUM != 0;
        match self.priv_mode {
            PrivMode::U => {
                if !u {
                    return false;
                }
                match access {
                    Access::Load => r || (x && mxr),
                    Access::Store => w,
                    Access::Exec => x,
                }
            }
            PrivMode::S => match access {
                // S may not execute U-accessible pages.
                Access::Exec => x && !u,
                // S accesses to U pages need SUM.
                _ if u && !sum => false,
                Access::Load => r || (x && mxr),
                Access::Store => w,
            },
            PrivMode::M => true, // M-mode bypasses translation entirely
        }
    }

    fn sv39_walk(
        &mut self,
        mem: &mut Mem,
        vaddr: u64,
        asid: u64,
        access: Access,
    ) -> Result<u64, Trap> {
        let fault = || Trap::exception(access.page_fault_cause(), vaddr);
        let mut a = (self.satp & 0xFFF_FFFFFFFF) << 12; // PPN * 4096
        let mut level: i32 = 2;
        let pte;
        let pte_addr;

        loop {
            let vpn_i = (vaddr >> (12 + 9 * level as u64)) & 0x1FF;
            let addr = a + vpn_i * 8;
            let val = read_mem_u64(mem, addr);
            let v = val & 1 != 0;
            let r = val & 2 != 0;
            let w = val & 4 != 0;
            if !v || (!r && w) {
                return Err(fault());
            }
            if r || val & 8 != 0 {
                pte = val;
                pte_addr = addr;
                break;
            }
            // Pointer to next level.
            level -= 1;
            if level < 0 {
                return Err(fault());
            }
            a = (val >> 10) << 12;
        }

        let r = pte & 2 != 0;
        let w = pte & 4 != 0;
        let x = pte & 8 != 0;
        let u = pte & 16 != 0;
        let g = pte & 32 != 0;
        let a_bit = pte & 64 != 0;
        let d_bit = pte & 128 != 0;

        // Superpage alignment: lower PPN bits must be zero.
        let ppn = pte >> 10;
        if level == 2 && ppn & 0x3FFFF != 0 {
            return Err(fault());
        }
        if level == 1 && ppn & 0x1FF != 0 {
            return Err(fault());
        }

        // Permission check (same rules as the TLB fast path).
        let flags = (r as u8) | ((w as u8) << 1) | ((x as u8) << 2) | ((u as u8) << 3);
        if !self.tlb_permits(flags, access) {
            return Err(fault());
        }

        // Hardware A/D update.
        let need_ad = !a_bit || (access == Access::Store && !d_bit);
        if need_ad {
            let new_pte = pte | 64 | if access == Access::Store { 128 } else { 0 };
            write_mem_u64(mem, pte_addr, new_pte);
        }

        let shift = 12 + 9 * level as u64;
        // Assemble: the PTE's PPN above the page offset + the vaddr's
        // in-page (and, for superpages, in-superpage) offset.
        let pa = (ppn << 12 & !((1 << shift) - 1)) | (vaddr & ((1 << shift) - 1));
        self.tlb.insert(
            (vpn_of(vaddr), asid),
            TlbEntry {
                pa_base: pa & !((1u64 << shift) - 1),
                flags: flags | ((g as u8) << 4),
                shift: shift as u8,
            },
        );
        Ok(pa)
    }
}

fn vpn_of(vaddr: u64) -> u64 {
    vaddr >> 12
}

fn read_mem_u64(mem: &mut Mem, addr: u64) -> u64 {
    let mut v = 0u64;
    for i in 0..8 {
        v |= (mem.read_byte(addr.wrapping_add(i)) as u64) << (8 * i);
    }
    v
}

fn write_mem_u64(mem: &mut Mem, addr: u64, v: u64) {
    for i in 0..8 {
        mem.write_byte(addr.wrapping_add(i), (v >> (8 * i)) as u8);
    }
}

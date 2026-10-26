//! Execute one privileged (or otherwise non-lowered) instruction inline in a
//! StackOp trace, via the hart — the single implementation of the privileged
//! semantics (PLAN_TRACE_CUTOVER). `rv_emit` emits `StackOp::Privileged` for
//! these instead of boundarying out to a separate step engine; the trace
//! engine's interpreter calls this through `StackHost::privileged`.
//!
//! Trap atomicity: a privileged op always *ends* its trace (the caller emits
//! `PushImm(next_pc); TailCall` after it), so a faulting memory op never
//! follows it in the same trace — there is no partial-commit hazard.

use crate::hart::{
    CAUSE_BREAKPOINT, CAUSE_ILLEGAL, CsrOp, HartState, MSTATUS_FS, MSTATUS_TSR, PrivMode, Trap,
};
use rv_asm::{Inst, Xlen};
use vane_arch::interp::PrivOutcome;
use vane_arch::{Mem, MemWidth};

/// The result of executing one privileged instruction.
pub struct PrivExec {
    /// Continue (the trace proceeds to the terminating `PushImm; TailCall`)
    /// or divert (the trace ends at this pc — trap handler or trap return).
    pub outcome: PrivOutcome,
    /// A satp write or sfence.vma changed the translation: flush the
    /// VA-keyed trace cache.
    pub clear_traces: bool,
    /// fence.i: the code cache is dirty (self-modifying code).
    pub code_dirty: bool,
}

impl PrivExec {
    fn cont() -> Self {
        PrivExec {
            outcome: PrivOutcome::Continue,
            clear_traces: false,
            code_dirty: false,
        }
    }
    fn divert(pc: u64) -> Self {
        PrivExec {
            outcome: PrivOutcome::Divert(pc),
            clear_traces: false,
            code_dirty: false,
        }
    }
}

/// Execute the privileged instruction `raw` at guest `pc` against `hart`,
/// reading/writing the register file `regs`. `mem` backs the FP load/store
/// arms (translation + MMIO-or-RAM access, one semantics with the stepper).
/// Mirrors the privileged arms of the reference stepper (one semantics).
pub fn execute_privileged(
    hart: &mut HartState,
    mem: &mut Mem,
    pc: u64,
    raw: u32,
    regs: &mut [u64; 32],
) -> PrivExec {
    let Ok((inst, _)) = Inst::decode(raw, Xlen::Rv64) else {
        let npc = hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc);
        return PrivExec::divert(npc);
    };
    if !hart.enabled {
        let npc = hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc);
        return PrivExec::divert(npc);
    }
    let set_reg = |regs: &mut [u64; 32], r: rv_asm::Reg, v: u64| {
        let i = (r.0 % 32) as usize;
        if i != 0 {
            regs[i] = v;
        }
    };
    let illegal = |hart: &mut HartState| {
        PrivExec::divert(hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc))
    };

    // FP load helper (Flw/Fld): translate, read MMIO-or-RAM, write the FP
    // register (write_f* marks mstatus.FS dirty). Mirrors step_one's Flw/Fld
    // (FS gate, xlate, loadv). Trap atomicity is free: the op ends its trace
    // and a fault diverts to the trap handler without committing anything.
    macro_rules! fp_load {
        ($offset:expr, $dest:expr, $base:expr, $w:expr, $n:expr, $is64:expr) => {{
            if hart.mstatus & MSTATUS_FS == 0 {
                return illegal(hart);
            }
            let va = regs[($base.0 % 32) as usize].wrapping_add_signed($offset.as_i64());
            let pa = match hart.translate_load(mem, va) {
                Ok(p) => p,
                Err(t) => return PrivExec::divert(hart.raise(t, pc)),
            };
            let v = if let Some(r) = hart.machine_load(pa, $w, false) {
                match r {
                    Ok(v) => v,
                    Err(t) => return PrivExec::divert(hart.raise(t, pc)),
                }
            } else {
                let mut v = 0u64;
                for i in 0..$n {
                    v |= (mem.read_byte(pa.wrapping_add(i)) as u64) << (8 * i);
                }
                v
            };
            if $is64 {
                hart.write_f64($dest, f64::from_bits(v));
            } else {
                hart.write_f32($dest, f32::from_bits(v as u32));
            }
            return PrivExec::cont();
        }};
    }
    // FP store helper (Fsw/Fsd): translate, write MMIO-or-RAM. Mirrors
    // step_one's Fsw/Fsd.
    macro_rules! fp_store {
        ($offset:expr, $src:expr, $base:expr, $w:expr, $n:expr, $mask:expr) => {{
            if hart.mstatus & MSTATUS_FS == 0 {
                return illegal(hart);
            }
            let va = regs[($base.0 % 32) as usize].wrapping_add_signed($offset.as_i64());
            let pa = match hart.translate_store(mem, va) {
                Ok(p) => p,
                Err(t) => return PrivExec::divert(hart.raise(t, pc)),
            };
            let v = hart.fregs[($src).0 as usize] & $mask;
            if let Some(r) = hart.machine_store(pa, $w, v) {
                match r {
                    Ok(()) => {}
                    Err(t) => return PrivExec::divert(hart.raise(t, pc)),
                }
            } else {
                for i in 0..$n {
                    mem.write_byte(pa.wrapping_add(i), (v >> (8 * i)) as u8);
                }
            }
            return PrivExec::cont();
        }};
    }

    // CSR op helper: run hart.csr_op, write the old value to `dest` on
    // success, divert to the trap handler on a fault. Returns clear_traces
    // for a satp write.
    macro_rules! csr {
        ($op:expr, $csr:expr, $dest:expr, $val:expr, $write:expr) => {{
            let csr_no = ($csr).0;
            let is_satp = csr_no == 0x180 && $write;
            match hart.csr_op($op, csr_no, $val, $write) {
                Ok(old) => {
                    set_reg(regs, $dest, old);
                    let mut r = PrivExec::cont();
                    r.clear_traces = is_satp;
                    return r;
                }
                Err(t) => return PrivExec::divert(hart.raise(t, pc)),
            }
        }};
    }

    match inst {
        Inst::Csrrw { csr, dest, src } => {
            csr!(CsrOp::Write, csr, dest, regs[(src.0 % 32) as usize], true)
        }
        Inst::Csrrs { csr, dest, src } => csr!(
            CsrOp::Set,
            csr,
            dest,
            regs[(src.0 % 32) as usize],
            src.0 != 0
        ),
        Inst::Csrrc { csr, dest, src } => csr!(
            CsrOp::Clear,
            csr,
            dest,
            regs[(src.0 % 32) as usize],
            src.0 != 0
        ),
        Inst::Csrrwi { csr, dest, uimm } => csr!(CsrOp::Write, csr, dest, uimm.as_u64(), true),
        Inst::Csrrsi { csr, dest, uimm } => {
            csr!(CsrOp::Set, csr, dest, uimm.as_u64(), uimm.as_u64() != 0)
        }
        Inst::Csrrci { csr, dest, uimm } => {
            csr!(CsrOp::Clear, csr, dest, uimm.as_u64(), uimm.as_u64() != 0)
        }
        Inst::Mret => {
            if hart.priv_mode != PrivMode::M {
                return illegal(hart);
            }
            PrivExec::divert(hart.mret())
        }
        Inst::Sret => {
            let pm = hart.priv_mode;
            if pm < PrivMode::S || (pm == PrivMode::S && hart.mstatus & MSTATUS_TSR != 0) {
                return illegal(hart);
            }
            PrivExec::divert(hart.sret())
        }
        Inst::Wfi => {
            // Implemented as a NOP (the spec allows this); interrupts are
            // checked at every trace dispatch anyway.
            if hart.priv_mode < PrivMode::M && hart.mstatus & crate::hart::MSTATUS_TW != 0 {
                return illegal(hart);
            }
            PrivExec::cont()
        }
        Inst::SfenceVma { addr, asid } => {
            let pm = hart.priv_mode;
            if pm < PrivMode::S
                || (pm == PrivMode::S && hart.mstatus & crate::hart::MSTATUS_TVM != 0)
            {
                return illegal(hart);
            }
            hart.sfence_vma(regs[(addr.0 % 32) as usize], regs[(asid.0 % 32) as usize]);
            // Mappings changed: the VA-keyed trace cache is stale.
            let mut r = PrivExec::cont();
            r.clear_traces = true;
            r
        }
        Inst::Ebreak => PrivExec::divert(hart.raise(Trap::exception(CAUSE_BREAKPOINT, pc), pc)),
        // FP loads/stores (rv_emit lowers them as Privileged, PLAN_TRACE_
        // CUTOVER): memory ops with an FP-register destination/source.
        Inst::Flw { offset, dest, base } => {
            fp_load!(offset, dest, base, MemWidth::B4, 4, false)
        }
        Inst::Fld { offset, dest, base } => {
            fp_load!(offset, dest, base, MemWidth::B8, 8, true)
        }
        Inst::Fsw { offset, src, base } => {
            fp_store!(offset, src, base, MemWidth::B4, 4, 0xffff_ffff)
        }
        Inst::Fsd { offset, src, base } => {
            fp_store!(offset, src, base, MemWidth::B8, 8, u64::MAX)
        }
        Inst::FenceI => {
            // Self-modifying code: the trace cache must be revalidated.
            let mut r = PrivExec::cont();
            r.code_dirty = true;
            r
        }
        // The remaining F/D compute ops (arithmetic, min/max, classify,
        // moves, conversions, fused multiply-add) go through the shared
        // `exec_fp` (one semantics with the stepper). rv_emit routes every FP
        // op here (Privileged), so this catch-all only ever sees FP compute
        // ops; with FS off they trap illegal (NOT divert(pc), which would
        // re-dispatch to this same op forever).
        other => {
            if hart.mstatus & MSTATUS_FS == 0 {
                return illegal(hart);
            }
            match crate::fp::exec_fp(hart, regs, &other) {
                crate::fp::FpOutcome::Done => PrivExec::cont(),
                crate::fp::FpOutcome::Writeback(rd, v) => {
                    set_reg(regs, rd, v);
                    PrivExec::cont()
                }
                // Unreachable: rv_emit only routes FP ops here and exec_fp
                // covers the whole F/D compute set.
                crate::fp::FpOutcome::NotFp => PrivExec::divert(pc),
            }
        }
    }
}

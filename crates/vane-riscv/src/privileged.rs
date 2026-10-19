//! Execute one privileged (or otherwise non-lowered) instruction inline in a
//! StackOp trace, via the hart — the single implementation of the privileged
//! semantics (PLAN_TRACE_CUTOVER). `rv_emit` emits `StackOp::Privileged` for
//! these instead of boundarying out to a separate step engine; the trace
//! engine's interpreter calls this through `StackHost::privileged`.
//!
//! Trap atomicity: a privileged op always *ends* its trace (the caller emits
//! `PushImm(next_pc); TailCall` after it), so a faulting memory op never
//! follows it in the same trace — there is no partial-commit hazard.

use crate::hart::{CsrOp, HartState, PrivMode, Trap, CAUSE_BREAKPOINT, CAUSE_ILLEGAL, MSTATUS_TSR};
use rv_asm::{Inst, Xlen};
use vane_arch::interp::PrivOutcome;

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
        PrivExec { outcome: PrivOutcome::Continue, clear_traces: false, code_dirty: false }
    }
    fn divert(pc: u64) -> Self {
        PrivExec { outcome: PrivOutcome::Divert(pc), clear_traces: false, code_dirty: false }
    }
}

/// Execute the privileged instruction `raw` at guest `pc` against `hart`,
/// reading/writing the register file `regs`. Mirrors the privileged arms of
/// the reference stepper (one semantics).
pub fn execute_privileged(hart: &mut HartState, pc: u64, raw: u32, regs: &mut [u64; 32]) -> PrivExec {
    let Ok((inst, _)) = Inst::decode(raw, Xlen::Rv64) else {
        let npc = hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc);
        return PrivExec::divert(npc);
    };
    if !hart.enabled {
        let npc = hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc);
        return PrivExec::divert(npc);
    }
    let mut set_reg = |regs: &mut [u64; 32], r: rv_asm::Reg, v: u64| {
        let i = (r.0 % 32) as usize;
        if i != 0 {
            regs[i] = v;
        }
    };
    let illegal = |hart: &mut HartState| PrivExec::divert(hart.raise(Trap::exception(CAUSE_ILLEGAL, raw as u64), pc));

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
        Inst::Csrrw { csr, dest, src } => csr!(CsrOp::Write, csr, dest, regs[(src.0 % 32) as usize], true),
        Inst::Csrrs { csr, dest, src } => csr!(CsrOp::Set, csr, dest, regs[(src.0 % 32) as usize], src.0 != 0),
        Inst::Csrrc { csr, dest, src } => csr!(CsrOp::Clear, csr, dest, regs[(src.0 % 32) as usize], src.0 != 0),
        Inst::Csrrwi { csr, dest, uimm } => csr!(CsrOp::Write, csr, dest, uimm.as_u64(), true),
        Inst::Csrrsi { csr, dest, uimm } => csr!(CsrOp::Set, csr, dest, uimm.as_u64(), uimm.as_u64() != 0),
        Inst::Csrrci { csr, dest, uimm } => csr!(CsrOp::Clear, csr, dest, uimm.as_u64(), uimm.as_u64() != 0),
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
            if pm < PrivMode::S || (pm == PrivMode::S && hart.mstatus & crate::hart::MSTATUS_TVM != 0) {
                return illegal(hart);
            }
            hart.sfence_vma(regs[(addr.0 % 32) as usize], regs[(asid.0 % 32) as usize]);
            // Mappings changed: the VA-keyed trace cache is stale.
            let mut r = PrivExec::cont();
            r.clear_traces = true;
            r
        }
        Inst::Ebreak => PrivExec::divert(hart.raise(Trap::exception(CAUSE_BREAKPOINT, pc), pc)),
        Inst::FenceI => {
            // Self-modifying code: the trace cache must be revalidated.
            let mut r = PrivExec::cont();
            r.code_dirty = true;
            r
        }
        // Not a privileged instruction we handle inline: divert back to `pc`
        // so the dispatcher's stepper handles it (pre-cutover behaviour).
        _ => PrivExec::divert(pc),
    }
}

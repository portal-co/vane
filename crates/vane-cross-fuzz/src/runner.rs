//! Differential execution engine for Vane's `StackOp` frontends against
//! Unicorn 2.x (CROSS_FUZZ_PLAN.md §4).
//!
//! [`CrossRunner`] owns a Unicorn instance and a `vane_arch::Mem`, keeps both
//! engines' state in lockstep, and executes **single instructions**
//! step-differentially:
//!
//! 1. Identical code words, register image, and scratch memory are installed
//!    into both engines.
//! 2. Vane runs the frontend's `StackOp` trace for the instruction via the
//!    native `interpret`.
//! 3. Unicorn runs exactly one instruction (`emu_start` with `count = 1`).
//! 4. The resulting states are diffed field-by-field.
//!
//! Traps, unsupported instructions, decode failures, and syscall boundaries
//! are classified by [`crate::skip`] and skip the comparison rather than
//! failing.

use alloc::{string::String, vec::Vec};
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::Unicorn;

use crate::compare::{FieldDiff, diff_states};
use crate::skip::Verdict;
use crate::state::{MachineState, VaneCrossHost};

/// Base address of the code page in both engines' address spaces.
pub const CODE_BASE: u64 = 0x1000;
/// Base address of the scratch data page both engines map and compare.
/// Deliberately separated from the code region by Unicorn's internal page
/// granularity: overlapping mappings return `UC_ERR_MAP`.
pub const DATA_BASE: u64 = 0x10000;
/// Size of the mapped code region; big enough for every branch/jump target
/// the fuzz generators produce.
pub const CODE_LEN: u64 = 0xf000;
/// Size of the mapped data region (Unicorn requires page-size multiples;
/// Vane's `Mem` is byte-granular so any size works on that side).
pub const PAGE: u64 = 0x10000;
/// Word placed after every generated instruction in Vane's memory image.
/// An `rv_emit` trace that falls through past the instruction under test
/// executes this word and hits `Exit::Trap("op:ebreak")` — a
/// distinguishable, deliberate marker proving the trace reached the pad PC,
/// so any *other* trap must have come from the instruction under test
/// itself. Unicorn never executes the pad: its `count = 1` single step has
/// already stopped. (The all-ones illegal encoding was rejected exactly
/// because its trap message is indistinguishable from the tested
/// instruction's own decode failure at adjacent blocks.)
pub const STOP_PAD: u32 = 0x0010_0073; // ebreak

/// Which frontend/engine pair a runner drives.
pub trait ArchProfile {
    /// Unicorn architecture + mode for this profile.
    fn unicorn() -> (Arch, Mode);
    /// Install a full 32-entry register image into Unicorn.
    fn unicorn_write_regs(uc: &mut Unicorn<()>, regs: &[u64; 32]);
    /// Read the full 32-entry register image back out of Unicorn.
    fn unicorn_read_regs(uc: &mut Unicorn<()>) -> [u64; 32];
    /// Read Unicorn's PC.
    fn unicorn_read_pc(uc: &mut Unicorn<()>) -> u64;
    /// Names for the 32 register-image slots (used in diffs/reports).
    fn reg_names() -> [&'static str; 32];
    /// Whether slot 0 is an architectural hardwired-zero register.
    fn reg_zero_is_hardwired() -> bool {
        false
    }
    /// Initial PC for the engine that will run `words` (already installed at
    /// `CODE_BASE`). Branch/JAL targets stay inside the code page, so the
    /// "until" bound for Unicorn's single step is the page end.
    fn start_pc() -> u64 {
        CODE_BASE
    }
}

/// Verdict for one fully-executed differential step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Both engines executed the instruction; fields matched exactly.
    Match,
    /// Both executed it; these fields diverged.
    Mismatch(Vec<FieldDiff>),
    /// One engine completed while the reference refused (row 7) — fail with
    /// the Unicorn error for the report.
    UnicornRefused(i32),
}

/// Result of [`CrossRunner::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Compared(StepOutcome),
    /// Skipped per [`crate::skip`] (rows 1–5); carries the classification.
    Skipped(crate::skip::Skip),
    /// Unknown trap kind — fail closed (row 6).
    UnknownTrap(String),
    /// Vane's trace signalled a syscall boundary (row 5).
    Ecall,
}

/// Runs the differential loop for one [`ArchProfile`].
pub struct CrossRunner<P: ArchProfile> {
    uc: Unicorn<'static, ()>,
    mem: vane_arch::Mem,
    /// Register image last installed via [`CrossRunner::setup`]; re-applied
    /// to every Vane host (Unicorn's copy persists across steps).
    regs: [u64; 32],
    _profile: core::marker::PhantomData<P>,
}

impl<P: ArchProfile> CrossRunner<P> {
    pub fn new() -> Self {
        let (arch, mode) = P::unicorn();
        let uc = Unicorn::new(arch, mode).expect("unicorn open");
        let mut runner = Self {
            uc,
            mem: vane_arch::Mem::default(),
            regs: [0; 32],
            _profile: core::marker::PhantomData,
        };
        // Same regions mapped into both engines so a load/store that faults
        // in only one is a genuine mismatch, not a skip (plan §5).
        runner.uc
            .mem_map(CODE_BASE, CODE_LEN, Prot::ALL)
            .expect("map code page");
        runner.uc
            .mem_map(DATA_BASE, PAGE, Prot::ALL)
            .expect("map data page");
        runner
    }

    /// Install `words` as the code image, `regs` as the register image, and
    /// `data` (up to one page) as the scratch data image, into **both**
    /// engines identically.
    /// Byte-level variant of [`CrossRunner::setup`]: the caller composes
    /// the Vane code image itself (e.g. a compressed instruction followed
    /// by a 2-byte `c.ebreak` pad that fits in the same word slot).
    /// `unicorn_code` is written to Unicorn only (usually == `vane_code`
    /// minus the pad); `vane_code` is written to the harness `Mem`.
    pub fn setup_raw(
        &mut self,
        vane_code: &[u8],
        unicorn_code: &[u8],
        regs: &[u64; 32],
        data: &[u8],
    ) {
        for (i, b) in vane_code.iter().enumerate() {
            self.mem.write_byte(CODE_BASE + i as u64, *b);
        }
        for (i, b) in data.iter().take(PAGE as usize).enumerate() {
            self.mem.write_byte(DATA_BASE + i as u64, *b);
        }
        self.uc.mem_write(CODE_BASE, unicorn_code).expect("uc code write");
        if !data.is_empty() {
            self.uc
                .mem_write(DATA_BASE, &data[..data.len().min(PAGE as usize)])
                .expect("uc data write");
        }
        P::unicorn_write_regs(&mut self.uc, regs);
        self.regs = *regs;
    }

    pub fn setup(&mut self, words: &[u32], regs: &[u64; 32], data: &[u8]) {
        let mut code = Vec::with_capacity(words.len() * 4 + 4);
        for w in words {
            code.extend_from_slice(&w.to_le_bytes());
        }
        // Vane side: write the code image through `Mem` (byte-at-a-time;
        // `CheckCode`'s integrity fetch also reads through here). Vane gets
        // `words` **plus the stop pad** — see the pad rationale on
        // [`STOP_PAD`]. Unicorn gets `words` only: its `count = 1` single
        // step never reaches past the instruction under test, and a pad
        // fetch there would be a spurious `UC_ERR_INSN_INVALID`.
        let mut vane_pad_only = Vec::with_capacity(words.len() * 4 + 4);
        for w in words {
            vane_pad_only.extend_from_slice(&w.to_le_bytes());
        }
        vane_pad_only.extend_from_slice(&STOP_PAD.to_le_bytes());
        for (i, b) in vane_pad_only.iter().enumerate() {
            self.mem.write_byte(CODE_BASE + i as u64, *b);
        }
        for (i, b) in data.iter().take(PAGE as usize).enumerate() {
            self.mem.write_byte(DATA_BASE + i as u64, *b);
        }
        // Unicorn side.
        self.uc.mem_write(CODE_BASE, &code).expect("uc code write");
        if !data.is_empty() {
            self.uc
                .mem_write(DATA_BASE, &data[..data.len().min(PAGE as usize)])
                .expect("uc data write");
        }
        P::unicorn_write_regs(&mut self.uc, regs);
        self.regs = *regs;
    }

    /// Execute one differential step for the (single) instruction at
    /// `trace_pc`: Vane's scoped trace vs Unicorn's single step.
    pub fn step<E>(&mut self, _trace_pc: u64, emit: E) -> Step
    where
        E: FnOnce(&mut vane_arch::Mem) -> Vec<vane_target_core::StackOp>,
    {
        // ── Vane side ────────────────────────────────────────────────────
        let ops = emit(&mut self.mem);
        // None = instruction completed (fallthrough-to-pad or branch/jump).
        // Some(skip) = the instruction itself trapped with a classifiable
        // trap message (rows 1–3).
        let mut insn_trap_skip: Option<crate::skip::Skip> = None;
        let mut host = VaneCrossHost::new(&mut self.mem, P::reg_zero_is_hardwired());
        host.state.regs = self.regs;
        let vane_exit = host.run_trace(&ops);
        let vane_state = host.state.clone();
        drop(host);

        // ── Classify Vane's exit ─────────────────────────────────────────
        match &vane_exit {
            crate::interp::Exit::Ecall => return Step::Ecall,
            crate::interp::Exit::Trap(msg) => {
                // The ebreak pad is the scoped trace's normal end: the
                // instruction under test completed and control fell through
                // into the pad. Any other trap is the instruction's own.
                // Two cases produce a trap here:
                // 1. the pad (`msg == "op:ebreak"` — instruction completed),
                // 2. the instruction itself trapped (any message).
                // Non-ebreak traps classify immediately (rows 1–2). An
                // `op:ebreak` is ambiguous (pad vs an ebreak *instruction*,
                // which is itself a row-3 skip): defer to the Unicorn side —
                // a guest ebreak makes Unicorn refuse, while the pad is
                // never executed by Unicorn's count=1 step.
                match crate::skip::classify_trap(msg) {
                    Verdict::Skip(skip) if msg != "op:ebreak" => {
                        #[cfg(feature = "cross-debug-traps")]
                        {
                            self.last_trap_msg = alloc::format!("msg={msg}");
                        }
                        return Step::Skipped(skip);
                    }
                    Verdict::UnknownTrap(m) => return Step::UnknownTrap(m),
                    _ => {}
                }
                insn_trap_skip = match crate::skip::classify_trap(msg) {
                    Verdict::Skip(s) => Some(s),
                    _ => None,
                };
            }
            crate::interp::Exit::TailCall(_) => {
                // Normal architectural exit (fallthrough/branch/jump). Fall
                // through to the Unicorn side and compare.
            }
        }

        // ── Unicorn side: exactly one instruction ────────────────────────
        let start = P::start_pc();
        let until = CODE_BASE + PAGE;
        match self.uc.emu_start(start, until, 0, 1) {
            Ok(()) => {}
            Err(e) => {
                // Unicorn refused. If Vane's own trap was a classifiable
                // skip (guest break, unsupported, undecodable), both engines
                // agree this isn't a normal architectural exit — skip (the
                // plan's rows 1–3 evaluate on *either* engine's refusal).
                // Otherwise row 7: Vane executed something the reference
                // refused; assume Vane is wrong.
                if let Some(skip) = insn_trap_skip {
                    return Step::Skipped(skip);
                }
                return Step::Compared(StepOutcome::UnicornRefused(e as i32));
            }
        }

        // ── Compare ──────────────────────────────────────────────────────
        let mut uc_state = MachineState::default();
        uc_state.regs = P::unicorn_read_regs(&mut self.uc);
        let _ = P::unicorn_read_pc(&mut self.uc);
        let diffs = diff_states(&vane_state, &uc_state, &P::reg_names());
        if diffs.is_empty() {
            Step::Compared(StepOutcome::Match)
        } else {
            Step::Compared(StepOutcome::Mismatch(diffs))
        }
    }

    /// Read the byte image of a range from Vane's `Mem` (post-step, for
    /// store-effect verification).
    pub fn vane_mem_range(&self, base: u64, len: usize) -> Vec<u8> {
        crate::state::read_mem_range(&self.mem, base, len)
    }

    /// Read the same range from Unicorn.
    pub fn unicorn_mem_range(&self, base: u64, len: usize) -> Vec<u8> {
        let mut buf = alloc::vec![0u8; len];
        self.uc.mem_read(base, &mut buf).expect("uc mem_read");
        buf
    }

    /// Borrow the Unicorn instance for profile-specific probing in tests.
    pub fn unicorn(&mut self) -> &mut Unicorn<'static, ()> {
        &mut self.uc
    }
}

impl<P: ArchProfile> Default for CrossRunner<P> {
    fn default() -> Self {
        Self::new()
    }
}

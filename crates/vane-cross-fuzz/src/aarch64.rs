//! AArch64 glue: building Vane's AArch64 trace for one instruction word and
//! the Unicorn ARM64 profile (CROSS_FUZZ_PLAN.md M3).
//!
//! `emit_aarch64` mirrors `Aarch64Reactor::run`: a `TemplateJit` over the
//! harness's `Mem` with `Heat::New` and default flags, feeding `a64_emit`.

use alloc::vec::Vec;
use unicorn_engine::RegisterARM64;
use vane_arch::{Heat, Mem};
use vane_arch::template::{Params, TemplateJit};
use vane_target_core::StackOp;

use crate::runner::CrossRunner;

/// Build the AArch64 `StackOp` trace for the instruction word at `pc` from
/// the runner's `Mem` (always `Heat::New` — never cache-hit).
pub fn emit_aarch64(mem: &mut Mem, pc: u64) -> Vec<StackOp> {
    let jit = TemplateJit {
        params: Params {
            react: mem,
            trial: &|_| Heat::New,
            root: 0,
            flags: vane_arch::template::Flags::default(),
            boundaries: None,
        },
        pc,
        labels: Default::default(),
        depth: 0,
    };
    vane_aarch64::fuzz::a64_emit(&jit)
}

/// Alias so tests/fuzz targets can name the runner type directly.
pub type A64Runner = CrossRunner<A64Profile>;

/// Unicorn ARM64 register for Vane slot `i` (x0..x30 → X0..X30 explicit
/// variants; slot 31 → SP). See `unicorn_write_regs` for why arithmetic on
/// `X0 as i32` is wrong.
pub fn a64_reg(i: usize) -> RegisterARM64 {
    // X0..X28 are contiguous in Unicorn's enum; X29/X30 are aliased near
    // the top (before NZCV/SP), so they need explicit variants.
    const X0: i32 = RegisterARM64::X0 as i32;
    match i {
        0..=28 => unsafe { core::mem::transmute::<i32, RegisterARM64>(X0 + i as i32) },
        29 => RegisterARM64::X29,
        30 => RegisterARM64::X30,
        _ => RegisterARM64::SP,
    }
}

/// AArch64 profile: Unicorn `Arch::ARM64` in `Mode::ARM`; Vane slots
/// x0..x30 (slot 31 = SP). AArch64 instructions are fixed 4 bytes, so the
/// ebreak stop-pad sits at pc+4 (the next word slot).
pub struct A64Profile;

impl crate::runner::ArchProfile for A64Profile {
    fn unicorn() -> (unicorn_engine::unicorn_const::Arch, unicorn_engine::unicorn_const::Mode) {
        (
            unicorn_engine::unicorn_const::Arch::ARM64,
            unicorn_engine::unicorn_const::Mode::ARM,
        )
    }

    fn unicorn_write_regs(uc: &mut unicorn_engine::Unicorn<()>, regs: &[u64; 32]) {
        for (i, v) in regs.iter().enumerate() {
            // Unicorn's ARM64 register enum is NOT contiguous X0..X30
            // (X29/X30 aliases appear near the top of the enum), so
            // `X0 + n` arithmetic silently reads the wrong register. Match
            // each index to its explicit variant instead.
            let reg = a64_reg(i);
            uc.reg_write(reg, *v).expect("unicorn reg_write x/sp");
        }
        // QEMU's initial PSTATE has the Z flag set (empty condition field);
        // clear it so a non-flag-setting instruction's "no NZCV update"
        // doesn't read as a divergence from Vane's all-zero defaults.
        uc.reg_write(RegisterARM64::NZCV, 0)
            .expect("unicorn reg_write nzcv init");
    }

    fn unicorn_read_regs(uc: &mut unicorn_engine::Unicorn<()>) -> [u64; 32] {
        core::array::from_fn(|i| {
            uc.reg_read(a64_reg(i)).expect("unicorn reg_read x/sp")
        })
    }

    fn unicorn_read_pc(uc: &mut unicorn_engine::Unicorn<()>) -> u64 {
        uc.reg_read(RegisterARM64::PC).expect("unicorn reg_read pc")
    }

    fn reg_names() -> [&'static str; 32] {
        core::array::from_fn(|i| match i {
            31 => "sp",
            _ => "x",
        })
    }

    fn pad_trap_msg() -> &'static str {
        // Vane A64 lowers `EXCEPTION::BRK_EXCEPTION` to
        // `aarch64: brk #0x…`; the pad word is BRK #0.
        "aarch64: brk #0"
    }

    fn pad_word() -> u32 {
        crate::runner::STOP_PAD_A64
    }

    fn packs_nzcv() -> bool {
        true
    }
}

/// Execute one differential step for AArch64: build Vane's trace for `pc`
/// from the runner's memory and compare against Unicorn's single step.
pub fn step_aarch64(runner: &mut CrossRunner<A64Profile>, pc: u64) -> crate::runner::Step {
    runner.step(pc, |mem| emit_aarch64(mem, pc))
}

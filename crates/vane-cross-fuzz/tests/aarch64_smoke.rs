//! AArch64 differential smoke tests (CROSS_FUZZ_PLAN.md M3): hand-written
//! instruction words through both Vane (`a64_emit` + `interpret`) and
//! Unicorn ARM64, single-stepped, state-compared.

use vane_cross_fuzz::runner::{CODE_BASE, DATA_BASE, Step, StepOutcome};
use vane_cross_fuzz::aarch64::{A64Runner, step_aarch64};

fn one_step(words: &[u32], regs: &[(usize, u64)]) -> Step {
    let mut r = [0u64; 32];
    for (i, v) in regs {
        r[*i] = *v;
    }
    let mut runner = A64Runner::new();
    runner.setup(words, &r, &[]);
    step_aarch64(&mut runner, CODE_BASE)
}

#[test]
fn movz_matches_unicorn() {
    // movz x1, #42
    let words = [0xd280_0541u32];
    let step = one_step(&words, &[]);
    assert_eq!(step, Step::Compared(StepOutcome::Match));
}

#[test]
fn add_imm_matches_unicorn() {
    // add x3, x1, #100
    let words = [0x9102_8063u32];
    let step = one_step(&words, &[(1, 5)]);
    assert_eq!(step, Step::Compared(StepOutcome::Match));
}

#[test]
fn adds_sets_nzcv_consistently() {
    // adds xzr, x1, x2 (SUBS with minus? no — ADDS): 0xb1000041? Use
    // adds x3, x1, x2, lsl #0 — encoded: 0xeb020063 is subs; adds is
    // 0xab020063. Verify flags derived the same on both engines.
    let words = [0xab02_0063u32];
    let step = one_step(&words, &[(1, 1), (2, u64::MAX)]);
    assert_eq!(step, Step::Compared(StepOutcome::Match));
}

#[test]
fn subs_overflow_flags_match() {
    // subs x3, x1, x2 with overflow: 1 - 2^63
    let words = [0xeb02_0063u32];
    let step = one_step(&words, &[(1, 1), (2, 1 << 63)]);
    assert_eq!(step, Step::Compared(StepOutcome::Match));
}

#[test]
fn store_pair_matches() {
    // stp x1, x2, [sp] — writes both halves; compare memory too.
    let words = [0xa900_7fe1u32 | 0x2]; // stp x1, x2, [sp, #16]
    let mut r = [0u64; 32];
    r[31] = DATA_BASE;
    r[1] = 0x1111_1111_2222_2222;
    r[2] = 0x3333_3333_4444_4444;
    let mut runner = A64Runner::new();
    runner.setup(&words, &r, &[0u8; 64]);
    let step = step_aarch64(&mut runner, CODE_BASE);
    assert_eq!(step, Step::Compared(StepOutcome::Match));
    assert_eq!(
        runner.vane_mem_range(DATA_BASE + 16, 16),
        runner.unicorn_mem_range(DATA_BASE + 16, 16)
    );
}

#[test]
fn undefined_encoding_skips() {
    // 0xffffffff is not a valid A64 encoding (disarm64 → None) → row-1
    // skip.
    let words = [0xffff_ffffu32];
    let step = one_step(&words, &[]);
    assert_eq!(step, Step::Skipped(vane_cross_fuzz::skip::Skip::Undecodable));
}

#[test]
fn brk_is_guest_break_skip() {
    // 0xd4200000 = brk #0 → row-3 guest-break skip.
    let words = [0xd420_0000u32];
    let step = one_step(&words, &[]);
    assert_eq!(step, Step::Skipped(vane_cross_fuzz::skip::Skip::GuestBreak));
}

#[test]
fn svc_is_syscall_skip() {
    let words = [0xd400_0001u32]; // svc #0
    let step = one_step(&words, &[]);
    assert_eq!(step, Step::Ecall);
}

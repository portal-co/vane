//! M1: proptest differential testing of the RV64 frontend against Unicorn
//! with uniformly random instruction words (CROSS_FUZZ_PLAN.md §7.1, §8.1).
//!
//! Most random words skip (undecodable / unsupported); the point of this
//! target is to prove the pipeline end-to-end — generation, execution,
//! skip classification, and comparison — with enough compared cases per run
//! that silent over-skipping would be visible in the counters.

use vane_cross_fuzz::runner::{Step, StepOutcome};

proptest::proptest! {
    #[test]
    fn rv64_word_vs_unicorn(word in proptest::prelude::any::<u64>(), seed in proptest::prelude::any::<u64>()) {
        let step = run_one(word as u32, seed);
        // Any *comparison* failure is a real bug; skips are expected.
        proptest::prop_assert!(!matches!(
            step,
            Step::Compared(StepOutcome::Mismatch(_))
                | Step::UnknownTrap(_)
        ), "word {word:#010x} seed {seed:#x}: {step:?}");
    }
}

fn run_one(word: u32, seed: u64) -> Step {
    // Deterministic register/data seeding from `seed` (proptest regenerates
    // inputs while shrinking; the derivation keeps every rerun identical).
    let mut regs = [0u64; 32];
    for (i, r) in regs.iter_mut().enumerate().skip(1) {
        *r = seed
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(i as u64)
            .rotate_left((i as u32) % 64);
    }
    let data: [u8; 64] = core::array::from_fn(|i| (seed >> (i % 8)) as u8);
    vane_cross_fuzz::probe::run_with(word, &regs, &data)
}

/// Manual-statistics wrapper (proptest's own counters cover shrunk
/// failures; this gives run/skip/compare totals for the CI log).
#[test]
fn rv64_skip_rate_sanity() {
    let mut ran = 0u32;
    let mut compared = 0u32;
    let mut skipped = 0u32;
    for n in 0..2000u64 {
        let word = (n.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as u32;
        let step = run_one(word, n);
        ran += 1;
        match step {
            Step::Compared(StepOutcome::Match)
            | Step::Compared(StepOutcome::Mismatch(_))
            | Step::Compared(StepOutcome::UnicornRefused(_)) => compared += 1,
            Step::Skipped(_) | Step::Ecall => skipped += 1,
            Step::UnknownTrap(m) => panic!("fail-closed trap hit: {m}"),
        }
    }
    println!("rv64 random-word sweep: ran={ran} compared={compared} skipped={skipped}");
    // The pipeline must actually compare something; if every word skipped,
    // the harness would prove nothing.
    assert!(compared > 0, "no random word produced a comparison");
}

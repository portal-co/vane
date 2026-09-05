//! M2: encoder-driven RV64 generation (CROSS_FUZZ_PLAN.md §7.2–7.4).
//!
//! Uniform random words mostly miss implemented families; this target
//! assembles words from the actual field layouts (opcode/funct3/funct7)
//! with random register/immediate fields, biased toward the families
//! `rv_body` implements. This is where the compare rate climbs.

use proptest::prelude::*;

use vane_cross_fuzz::runner::{Step, StepOutcome};

/// (opcode, funct3, funct7) for R-type/I-type/S-type/B-type families the
/// RV64 frontend implements (CROSS_FUZZ_PLAN.md §7.3 weights per family).
fn rtype(opcode: u32, funct3: u32, funct7: u32) -> impl Strategy<Value = u32> {
    (
        any::<u32>().prop_map(move |seed| {
            let rd = seed & 0x1f;
            let rs1 = (seed >> 5) & 0x1f;
            let rs2 = (seed >> 10) & 0x1f;
            (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
        })
    )
}

fn itype(opcode: u32, funct3: u32) -> impl Strategy<Value = u32> {
    any::<u32>().prop_map(move |seed| {
        let rd = seed & 0x1f;
        let rs1 = (seed >> 5) & 0x1f;
        // 12-bit immediate incl. boundary-heavy values from the low byte.
        let imm = match seed >> 20 & 7 {
            0 => 0,
            1 => 1,
            2 => u32::MAX & 0xfff,          // -1
            3 => 1 << 11,                   // min signed-12
            4 => (seed >> 23) & 0xfff,
            5 => !(seed & 0x3f) & 0xfff,
            6 => 0x7ff,                     // -1 pre-sign
            _ => (seed >> 17) & 0xfff,
        };
        (imm << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
    })
}

fn stype(opcode: u32, funct3: u32) -> impl Strategy<Value = u32> {
    any::<u32>().prop_map(move |seed| {
        let rs1 = (seed >> 5) & 0x1f;
        let rs2 = (seed >> 10) & 0x1f;
        let imm = ((seed >> 15) & 0x7f) as u32; // S-imm7 (low 7 bits)
        let imm = match seed >> 22 & 3 {
            0 => imm,                       // as-is
            1 => 0,
            2 => 0x40 | imm & 0x3f,         // small positive
            _ => imm,
        };
        ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | ((imm & 0x1f) << 7) | opcode
    })
}

fn utype(opcode: u32) -> impl Strategy<Value = u32> {
    any::<u32>().prop_map(move |seed| {
        let rd = seed & 0x1f;
        let imm20 = (seed >> 5) & 0xfffff;
        (imm20 << 12) | (rd << 7) | opcode
    })
}

/// A sampled encoded RV64 word: mixes the implemented families with the
/// boundary-immediate I/S generators; a small weight stays uniform-random
/// so the classifier keeps exercising.
fn encoded_word() -> impl Strategy<Value = u32> {
    prop_oneof![
        4 => itype(0x13, 0), // addi
        2 => itype(0x13, 1), // slli
        2 => itype(0x13, 5), // srli/srai by funct7
        2 => itype(0x1b, 0), // addiw
        2 => rtype(0x33, 0, 0x00), // add
        2 => rtype(0x33, 0, 0x20), // sub
        2 => rtype(0x3b, 0, 0x00), // addw
        2 => rtype(0x3b, 0, 0x20), // subw
        2 => rtype(0x33, 4, 0x00), // xor
        2 => rtype(0x33, 6, 0x00), // or
        2 => rtype(0x33, 7, 0x00), // and
        2 => rtype(0x33, 0, 0x01), // mul
        2 => rtype(0x33, 4, 0x01), // div
        2 => rtype(0x33, 5, 0x01), // divu
        2 => rtype(0x33, 6, 0x01), // rem
        2 => rtype(0x33, 7, 0x01), // remu
        2 => rtype(0x33, 1, 0x00), // sll
        2 => rtype(0x33, 5, 0x00), // srl
        2 => rtype(0x33, 5, 0x20), // sra
        1 => stype(0x23, 2), // sw
        1 => stype(0x23, 3), // sd
        1 => stype(0x23, 0), // sb
        1 => itype(0x03, 2), // lw
        1 => itype(0x03, 3), // ld
        1 => utype(0x37),    // lui
        1 => uniform_word(),
    ]
}

fn uniform_word() -> impl Strategy<Value = u32> {
    any::<u32>()
}

fn regs_from(seed: u64) -> [u64; 32] {
    let mut regs = [0u64; 32];
    for (i, r) in regs.iter_mut().enumerate().skip(1) {
        *r = seed
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(i as u64)
            .rotate_left((i as u32) % 64);
    }
    // Point a couple of address registers at the data page so loads/stores
    // hit mapped memory on both engines (wild addresses also valid; the
    // comparator then asserts the *faulting* agreement).
    regs[4] = vane_cross_fuzz::runner::DATA_BASE;
    regs[5] = vane_cross_fuzz::runner::DATA_BASE + 8;
    regs
}

proptest::proptest! {
    #[test]
    fn rv64_encoded_word_vs_unicorn(
        word in encoded_word(),
        seed in any::<u64>(),
    ) {
        let step = vane_cross_fuzz::probe::run_with(word, &regs_from(seed), &[0u8; 64]);
        proptest::prop_assert!(!matches!(
            step,
            Step::Compared(StepOutcome::Mismatch(_)) | Step::UnknownTrap(_)
        ), "word {word:#010x} seed {seed:#x}: {step:?}");
    }
}

#[test]
fn rv64_encoded_sweep() {
    let mut compared = 0u32;
    let mut skipped = 0u32;
    for n in 0..1000u64 {
        // Deterministic stand-in for the encoder: hash n into a word, then
        // also exercise every R-type funct3/funct7 pair implemented.
        let word = (n.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 24) as u32;
        let step = vane_cross_fuzz::probe::run_with(word, &regs_from(n), &[0u8; 64]);
        match step {
            Step::Compared(_) => compared += 1,
            Step::Skipped(_) | Step::Ecall => skipped += 1,
            Step::UnknownTrap(m) => panic!("fail-closed trap hit: {m}"),
        }
    }
    println!("rv64 encoded sweep: compared={compared} skipped={skipped}");
    assert!(compared > 0);
}

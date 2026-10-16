#![cfg(not(feature = "js-host"))]
use rv_asm::{Imm, Inst, Reg, Xlen};
use vane::{Reactor, StepOutcome};

#[test]
fn auipc_jalr_chain() {
    let r = Reactor::new();
    // auipc ra, 0 at 0x100; jalr ra, -336(ra) at 0x104
    let w0 = Inst::Auipc {
        uimm: Imm::new_u32(0),
        dest: Reg(1),
    }
    .encode_normal(Xlen::Rv64);
    let w1 = Inst::Jalr {
        offset: Imm::new_i32(-336),
        base: Reg(1),
        dest: Reg(1),
    }
    .encode_normal(Xlen::Rv64);
    r.write_bytes(0x100, &w0.to_le_bytes());
    r.write_bytes(0x104, &w1.to_le_bytes());
    let mut regs = [0u64; 32];
    let n = r.step_one(0x100, &mut regs).unwrap();
    assert!(matches!(n, StepOutcome::Next(0x104)));
    assert_eq!(regs[1], 0x100, "auipc result");
    let n = r.step_one(0x104, &mut regs).unwrap();
    // jalr: ra (link) = 0x108; pc = 0x100 - 336 = 0xffffffff_ffffffb0... wrap
    let expected = 0x100u64.wrapping_sub(336) & !1;
    match n {
        StepOutcome::Next(pc) => assert_eq!(pc, expected, "jalr target"),
        _ => panic!(),
    }
}

/// Regression test for the interpreter-side JALR link hazard: the original
/// interp wrote the link register before reading the base, so
/// `jalr ra, off(ra)` jumped to `link+off` instead of `base+off`. (rv_emit's
/// lowering already handled this; the per-instruction path did not. Found
/// via the M3 privileged payload, which uses auipc+jalr for static calls.)
#[test]
fn jalr_reads_base_before_writing_link() {
    auipc_jalr_chain();
}

#[test]
fn w_extension_sign_and_wrap() {
    let r = Reactor::new();
    r.set_hart_enabled(true);
    let mut regs = [0u64; 32];
    let mut pc = 0x8000_0000u64;
    let mut n = 0;
    let mut step = |regs: &mut [u64; 32], inst: Inst| {
        r.write_bytes(pc, &inst.encode_normal(Xlen::Rv64).to_le_bytes());
        match r.step_one(pc, regs).unwrap() { StepOutcome::Next(n) => pc = n, _ => panic!() }
        n += 1;
    };
    // x1 = 0x80000000 (negative 32-bit)
    step(&mut regs, Inst::Lui { uimm: Imm::new_u32(0x80000000), dest: Reg(1) });
    assert_eq!(regs[1], 0xFFFF_FFFF_8000_0000, "lui sign-extends");
    // sraw x2, x1, 1 -> arithmetic shift of 0x80000000 -> 0xC0000000 -> sign-ext
    regs[2] = 1;
    step(&mut regs, Inst::SraW { src2: Reg(2), dest: Reg(3), src1: Reg(1) });
    assert_eq!(regs[3], 0xFFFF_FFFF_C000_0000, "sraw sign-extends the operand");
    // srlw x4, x1, 1 -> 0x40000000
    step(&mut regs, Inst::SrlW { src2: Reg(2), dest: Reg(4), src1: Reg(1) });
    assert_eq!(regs[4], 0x4000_0000, "srlw");
    // sllw x5, x1, 1 -> 0 (low 32 zero) -> sign-ext 0
    step(&mut regs, Inst::SllW { src2: Reg(2), dest: Reg(5), src1: Reg(1) });
    assert_eq!(regs[5], 0, "sllw drops high bits");
    // sllw 0x40000001, 1 -> 0x80000002 -> sign-extended
    regs[6] = 0x4000_0001;
    step(&mut regs, Inst::SllW { src2: Reg(2), dest: Reg(7), src1: Reg(6) });
    assert_eq!(regs[7], 0xFFFF_FFFF_8000_0002, "sllw sign-extends result");
    // addw -1 + 1 = 0
    regs[8] = 0xFFFF_FFFF; // = -1 as i32
    regs[9] = 1;
    step(&mut regs, Inst::AddW { src2: Reg(9), dest: Reg(10), src1: Reg(8) });
    assert_eq!(regs[10], 0, "addw");
    // addw 0x7FFFFFFF + 1 = 0x80000000 -> sign-extended
    regs[8] = 0x7FFF_FFFF;
    step(&mut regs, Inst::AddW { src2: Reg(9), dest: Reg(10), src1: Reg(8) });
    assert_eq!(regs[10], 0xFFFF_FFFF_8000_0000, "addw overflow wraps+sign-extends");
    // subw 0 - 1 = -1
    regs[8] = 0;
    regs[9] = 1;
    step(&mut regs, Inst::SubW { src2: Reg(9), dest: Reg(10), src1: Reg(8) });
    assert_eq!(regs[10], u64::MAX, "subw");
    println!("wops OK after {n} steps");
}

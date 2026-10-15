#![cfg(not(feature = "js-host"))]
#[test]
fn auipc_jalr_chain() {
    use rv_asm::{Imm, Inst, Reg, Xlen};
    use vane::{Reactor, StepOutcome};
    let r = Reactor::new();
    // auipc ra, 0 at 0x100; jalr ra, -336(ra) at 0x104
    let w0 = Inst::Auipc { uimm: Imm::new_u32(0), dest: Reg(1) }.encode_normal(Xlen::Rv64);
    let w1 = Inst::Jalr { offset: Imm::new_i32(-336), base: Reg(1), dest: Reg(1) }.encode_normal(Xlen::Rv64);
    r.write_bytes(0x100, &w0.to_le_bytes());
    r.write_bytes(0x104, &w1.to_le_bytes());
    let mut regs = [0u64; 32];
    let n = r.step_one(0x100, &mut regs).unwrap();
    assert!(matches!(n, StepOutcome::Next(0x104)));
    assert_eq!(regs[1], 0x100, "auipc result");
    let n = r.step_one(0x104, &mut regs).unwrap();
    // jalr: ra (link) = 0x108; pc = 0x100 - 336 = 0xffffffff_ffffffb0... wrap
    let expected = 0x100u64.wrapping_sub(336) & !1;
    match n { StepOutcome::Next(pc) => assert_eq!(pc, expected, "jalr target"), _ => panic!() }
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

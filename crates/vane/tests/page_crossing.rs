#![cfg(not(feature = "js-host"))]
//! Regression: loads/stores/fetches crossing a 64 KiB Mem page boundary
//! must split correctly (previously read/wrote past the page allocation).
use rv_asm::{Imm, Inst, Reg, Xlen};
use vane::{Reactor, StepOutcome};

fn w(r: &Reactor, pc: u64, inst: &Inst) {
    r.write_bytes(pc, &inst.encode_normal(Xlen::Rv64).to_le_bytes());
}

#[test]
fn cross_page_load_store_fetch() {
    let r = Reactor::new();
    r.set_hart_enabled(true);
    let mut regs = [0u64; 32];

    // Write a known 64-bit pattern straddling the first page boundary.
    let pattern: u64 = 0x0123_4567_89ab_cdef;
    let straddle = 0x8000_0000 + 0xFFFC; // 4 bytes before the 64K boundary
                                         // sd x1, 0(x2): x2 = straddle
    w(
        &r,
        0x8000_0000,
        &Inst::Lui {
            uimm: Imm::new_u32(0x8000_0000),
            dest: Reg(2),
        },
    ); // wrong: sign-extends!
       // Use emit-style: just poke registers directly for setup.
    regs[2] = straddle;
    // store via Sd
    w(
        &r,
        0x8000_0000,
        &Inst::Sd {
            offset: Imm::new_i32(0),
            src: Reg(1),
            base: Reg(2),
        },
    );
    regs[1] = pattern;
    match r.step_one(0x8000_0000, &mut regs).unwrap() {
        StepOutcome::Next(_) => {}
        _ => panic!("store failed"),
    }
    // Read it back byte-wise and as a straddling u64.
    for i in 0..8 {
        let b = r.read_guest_byte(straddle + i);
        assert_eq!(
            b,
            (pattern >> (8 * i)) as u8,
            "byte {i} of straddling store"
        );
    }
    // Load it back through the interpreter.
    w(
        &r,
        0x8000_0100,
        &Inst::Ld {
            offset: Imm::new_i32(0),
            dest: Reg(3),
            base: Reg(2),
        },
    );
    match r.step_one(0x8000_0100, &mut regs).unwrap() {
        StepOutcome::Next(_) => {}
        _ => panic!("load failed"),
    }
    assert_eq!(regs[3], pattern, "straddling load");

    // Fetch across a page boundary: place a 4-byte instruction at page_end-2
    // (compressed pc ≡ 2 mod 4) followed by a nop on the next page.
    let fetch_straddle = 0x8000_0000 + 0x1_FFFE;
    let nop = Inst::Addi {
        imm: Imm::new_i32(0),
        dest: Reg(0),
        src1: Reg(0),
    };
    r.write_bytes(fetch_straddle, &nop.encode_normal(Xlen::Rv64).to_le_bytes());
    r.write_bytes(
        fetch_straddle + 4,
        &nop.encode_normal(Xlen::Rv64).to_le_bytes(),
    );
    match r.step_one(fetch_straddle, &mut regs).unwrap() {
        StepOutcome::Next(n) => assert_eq!(n, fetch_straddle + 4),
        _ => panic!("fetch failed"),
    }
}

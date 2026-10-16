#![cfg(not(feature = "js-host"))]
//! S-mode PLIC delivery: a UART RX interrupt must reach an S-mode handler
//! through PLIC context 1 (the QEMU-virt S-mode context), with M delegating
//! SEIP to S. This is the path the Linux 8250 tty driver depends on.
use rv_asm::{Csr, Imm, Inst, Reg, Xlen};
use vane::{Reactor, StepOutcome};

const CODE: u64 = 0x8000_0000;
const MTVEC: u64 = 0x9000_0000;
const STVEC: u64 = 0x9100_0000;
const UART: u64 = 0x1000_0000;
const PLIC: u64 = 0x0C00_0000;

fn w(r: &Reactor, pc: u64, inst: &Inst) {
    r.write_bytes(pc, &inst.encode_normal(Xlen::Rv64).to_le_bytes());
}

/// Emit an exact 64-bit constant load (the naive LUI-only form sign-extends
/// when bit 31 is set — RAM starts at 0x8000_0000).
fn emit_li(r: &Reactor, pc: &mut u64, dest: Reg, value: u64) {
    if (value as i32 as u64) == value && value < 2048 {
        w(r, *pc, &Inst::Addi { imm: Imm::new_i32(value as i32), dest, src1: Reg(0) });
        *pc += 4;
        return;
    }
    // lui %hi; addiw %lo; then clear the sign-extended top half.
    let lo = value as u32 as i32 as i64; // sign-extended low half
    let hi = ((value >> 12) + ((value >> 11) & 1)) as u32; // carry-adjusted
    w(r, *pc, &Inst::Lui { uimm: Imm::new_u32(hi << 12), dest });
    *pc += 4;
    w(r, *pc, &Inst::AddiW { imm: Imm::new_i32(lo as i32 & 0xfff), dest, src1: dest });
    *pc += 4;
    w(r, *pc, &Inst::Slli { imm: Imm::new_u32(32), dest, src1: dest });
    *pc += 4;
    w(r, *pc, &Inst::Srli { imm: Imm::new_u32(32), dest, src1: dest });
    *pc += 4;
}

#[test]
fn uart_irq_reaches_s_mode_via_plic_ctx1() {
    let r = Reactor::new();
    r.set_hart_enabled(true);
    r.set_machine_enabled(true);

    // M-mode program at CODE:
    //   csrw mtvec, t0 (point at MTVEC)  — via li+csrw
    //   delegate SEIP to S (mideleg bit 9) and set SIE bits
    //   write PLIC: priority[10]=7, enable ctx1 source 10, threshold 0
    //   mret into S at SENTRY
    // S-mode at SENTRY (via mtvec-free direct sret):
    //   loop { wfi }  — the SEIP interrupt goes to STVEC
    // STVEC handler:
    //   read PLIC ctx1 claim, store it at a known address, write tohost pass.
    // M-mode program: delegate SEIP to S, write PLIC (host-poked below),
    // enable the UART RX interrupt, mret into S at 0x80001000.
    let mut pc = CODE;
    macro_rules! emit {
        ($i:expr) => {{
            w(&r, pc, &$i);
            pc += 4;
        }};
    }
    // mideleg |= SEIP (bit 9)
    emit!(Inst::Addi { imm: Imm::new_i32(1), dest: Reg(5), src1: Reg(0) });
    emit!(Inst::Slli { imm: Imm::new_u32(9), dest: Reg(5), src1: Reg(5) });
    emit!(Inst::Csrrs { csr: Csr::MIDELEG, dest: Reg(0), src: Reg(5) });
    // UART IER = RDA (offset 1) = 1
    emit_li(&r, &mut pc, Reg(6), UART);
    emit!(Inst::Addi { imm: Imm::new_i32(1), dest: Reg(5), src1: Reg(0) });
    emit!(Inst::Sb { offset: Imm::new_i32(1), src: Reg(5), base: Reg(6) });
    // mepc = SENTRY; mstatus.MPP = S; mret
    emit_li(&r, &mut pc, Reg(5), 0x8000_1000);
    emit!(Inst::Csrrw { csr: Csr::MEPC, dest: Reg(0), src: Reg(5) });
    emit!(Inst::Addi { imm: Imm::new_i32(1), dest: Reg(5), src1: Reg(0) });
    emit!(Inst::Slli { imm: Imm::new_u32(11), dest: Reg(5), src1: Reg(5) });
    emit!(Inst::Csrrs { csr: Csr::MSTATUS, dest: Reg(0), src: Reg(5) });
    emit!(Inst::Mret);

    // PLIC setup via the host (same MMIO register interface).
    r.with_machine(|m| {
        // Use the byte-composing MMIO path (Machine::store) — the raw
        // Plic::write is byte-granular.
        m.store(PLIC + 10 * 4, 4, 7).unwrap(); // priority[10] = 7
        m.store(PLIC + 0x2000 + 0x80, 4, 1 << 10).unwrap(); // enable ctx1 src 10
        m.store(PLIC + 0x201000, 4, 0).unwrap(); // threshold ctx1 = 0
        m.uart.push_rx(b"Q");
    });

    // S-mode entry at 0x80001000: stvec = 0x80002000; sie.SEIE; sstatus.SIE;
    // then spin.
    let mut pc = 0x8000_1000;
    macro_rules! emit {
        ($i:expr) => {{
            w(&r, pc, &$i);
            pc += 4;
        }};
    }
    emit_li(&r, &mut pc, Reg(5), 0x8000_2000);
    emit!(Inst::Csrrw { csr: Csr::STVEC, dest: Reg(0), src: Reg(5) });
    emit!(Inst::Addi { imm: Imm::new_i32(1), dest: Reg(5), src1: Reg(0) });
    emit!(Inst::Slli { imm: Imm::new_u32(9), dest: Reg(5), src1: Reg(5) });
    emit!(Inst::Csrrs { csr: Csr::SIE, dest: Reg(0), src: Reg(5) }); // SEIE
    emit!(Inst::Addi { imm: Imm::new_i32(1), dest: Reg(5), src1: Reg(0) });
    emit!(Inst::Slli { imm: Imm::new_u32(1), dest: Reg(5), src1: Reg(5) });
    emit!(Inst::Csrrs { csr: Csr::SSTATUS, dest: Reg(0), src: Reg(5) }); // SIE
    emit!(Inst::Jal { offset: Imm::new_i32(0), dest: Reg(0) }); // spin

    // S-mode handler at 0x80002000: claim via PLIC ctx1, store the claim at
    // 0x80003000, write 'D' to the UART, spin.
    let mut pc = 0x8000_2000;
    macro_rules! emit {
        ($i:expr) => {{
            w(&r, pc, &$i);
            pc += 4;
        }};
    }
    // PLIC ctx1 claim/complete = base + 0x200000 + ctx*0x1000 + 4
    emit_li(&r, &mut pc, Reg(5), 0x0C00_0000 + 0x20_1004);
    emit!(Inst::Lw { offset: Imm::new_i32(0), dest: Reg(6), base: Reg(5) });
    emit_li(&r, &mut pc, Reg(7), 0x8000_3000);
    emit!(Inst::Sd { offset: Imm::new_i32(0), src: Reg(6), base: Reg(7) });
    emit!(Inst::Addi { imm: Imm::new_i32(0x44), dest: Reg(5), src1: Reg(0) }); // 'D'
    emit_li(&r, &mut pc, Reg(6), UART);
    emit!(Inst::Sb { offset: Imm::new_i32(0), src: Reg(5), base: Reg(6) });
    emit!(Inst::Jal { offset: Imm::new_i32(0), dest: Reg(0) }); // spin

    let mut regs = [0u64; 32];
    let mut pc = CODE;
    for i in 0..2000 {
        let before = r.with_hart(|h| (h.priv_mode, h.mcause, h.mepc));
        match r.step_one(pc, &mut regs).unwrap() {
            StepOutcome::Next(n) => pc = n,
            StepOutcome::LegacyEcall(_) => panic!("unexpected legacy ecall"),
        }
        let after = r.with_hart(|h| (h.priv_mode, h.mcause, h.mepc));
        if before != after {
            println!("[{i}] trap-ish at pc={pc:#x}: {before:?} -> {after:?}");
        }
    }

    let claim = (0..8).fold(0u64, |acc, i| acc | (r.read_guest_byte(0x8000_3000 + i) as u64) << (8 * i));
    r.with_hart(|h| {
        println!(
            "after: priv={:?} mip={:#x} mie={:#x} mideleg={:#x} mcause={:#x} mstatus={:#x}",
            h.priv_mode, h.mip, h.mie, h.mideleg, h.mcause, h.mstatus
        );
    });
    let (e0, e1, p, pend) = r.with_machine(|m| {
        (m.plic.eip(0), m.plic.eip(1), m.uart.irq_pending(), {
            // peek pending via a claim on ctx1? no — read pending register
            m.plic.read(0x1000)
        })
    }).unwrap_or_default();
    println!("plic: eip0={e0} eip1={e1} uart_irq={p} pending={pend:#x}");
    assert_eq!(claim, 10, "S-mode handler should have claimed UART IRQ 10");
    let tx = r.with_machine(|m| m.uart.drain_tx()).unwrap_or_default();
    assert!(tx.contains(&b'D'), "S-mode handler should have written 'D' to the UART");
}

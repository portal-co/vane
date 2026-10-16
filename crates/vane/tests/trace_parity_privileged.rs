#![cfg(not(feature = "js-host"))]
//! StackOp-port parity (PLAN_LINUX_WASM.md §9): with Sv39 translation
//! active, the trace engine (`trace_run_fueled`) and the per-instruction
//! stepper (`step_one`) must produce identical architectural state — through
//! translated loads/stores, an arithmetic loop, and a page fault.
use rv_asm::{Csr, Imm, Inst, Reg, Xlen};
use vane::vane_riscv::hart::{PrivMode, SATP_MODE_SV39};
use vane::{Reactor, StepOutcome};

const CODE: u64 = 0x8000_0000;
const MTVEC: u64 = 0x9000_0000;
const ROOT: u64 = 0x8002_0000;
const L2: u64 = 0x8002_1000;
const L3: u64 = 0x8002_2000;
const DATA_PA: u64 = 0x8003_0000;
const MAP_VA: u64 = 0x4000_0000;
const FAULT_VA: u64 = 0x6000_0000;
const SPIN: u64 = 0x8000_0400;

const PTE_V: u64 = 1;
const PTE_R: u64 = 2;
const PTE_W: u64 = 4;
const PTE_X: u64 = 8;
const PTE_A: u64 = 64;
const PTE_D: u64 = 128;

fn w(r: &Reactor, pc: u64, inst: &Inst) {
    r.write_bytes(pc, &inst.encode_normal(Xlen::Rv64).to_le_bytes());
}
fn wq(r: &Reactor, addr: u64, v: u64) {
    r.write_bytes(addr, &v.to_le_bytes());
}
fn rq(r: &Reactor, addr: u64) -> u64 {
    (0..8).fold(0u64, |acc, i| acc | (r.read_guest_byte(addr + i) as u64) << (8 * i))
}

/// li/sd helpers using a known-good lui+addiw+slli/srli sequence for 64-bit
/// constants (bit31-safe).
fn emit_li(r: &Reactor, pc: &mut u64, dest: Reg, value: u64) {
    if (value as i32 as u64) == value && value < 2048 {
        w(r, *pc, &Inst::Addi { imm: Imm::new_i32(value as i32), dest, src1: Reg(0) });
        *pc += 4;
        return;
    }
    let hi = ((value >> 12) + ((value >> 11) & 1)) as u32;
    let lo = (value as u32 as i32) & 0xfff;
    w(r, *pc, &Inst::Lui { uimm: Imm::new_u32(hi << 12), dest });
    *pc += 4;
    w(r, *pc, &Inst::AddiW { imm: Imm::new_i32(lo), dest, src1: dest });
    *pc += 4;
    w(r, *pc, &Inst::Slli { imm: Imm::new_u32(32), dest, src1: dest });
    *pc += 4;
    w(r, *pc, &Inst::Srli { imm: Imm::new_u32(32), dest, src1: dest });
    *pc += 4;
}

fn setup(r: &Reactor) {
    r.set_hart_enabled(true);
    // Sv39 tables: identity megapage for code (0x8000_0000..), MAP_VA →
    // DATA_PA (RW, no X, no U), nothing else.
    wq(r, ROOT + 2 * 8, (0x8000_0000 >> 12 << 10) | PTE_A | PTE_D | PTE_X | PTE_W | PTE_R | PTE_V);
    wq(r, ROOT + 8, (L2 >> 12 << 10) | PTE_V);
    wq(r, L2, (L3 >> 12 << 10) | PTE_V);
    wq(r, L3, (DATA_PA >> 12 << 10) | PTE_A | PTE_D | PTE_W | PTE_R | PTE_V);
    wq(r, DATA_PA, 0x1111_2222_3333_4444);

    r.with_hart(|h| {
        h.satp = (SATP_MODE_SV39 << 60) | (ROOT >> 12);
        h.priv_mode = PrivMode::S;
        h.mtvec = MTVEC;
        h.stvec = 0x9100_0000;
        // Delegate store page faults to S (the handler at STVEC handles them).
        h.medeleg = 1 << 15;
    });
}

/// Program (runs in S-mode with translation):
///   x10 = 0; x11 = 3;
///   loop: x10 += [MAP_VA]; [MAP_VA+8] = x10; x11--; bne loop
///   sd x0, 0(xFAULT)        // page fault → S handler at STVEC
///   (handler) load sepc's faulting pc into x12; skip via sepc+=4; sret
///   spin
fn build_program(r: &Reactor) {
    let mut pc = CODE;
    macro_rules! emit {
        ($inst:expr) => {{
            w(r, pc, &$inst);
            pc += 4;
        }};
    }
    emit!(Inst::Addi { imm: Imm::new_i32(0), dest: Reg(10), src1: Reg(0) });
    emit!(Inst::Addi { imm: Imm::new_i32(3), dest: Reg(11), src1: Reg(0) });
    emit_li(r, &mut pc, Reg(1), MAP_VA); // x1 = MAP_VA
    let loop_pc = pc;
    emit!(Inst::Ld { offset: Imm::new_i32(0), dest: Reg(2), base: Reg(1) }); // x2 = *x1
    emit!(Inst::Add { src2: Reg(2), dest: Reg(10), src1: Reg(10) }); // x10 += x2
    emit!(Inst::Sd { offset: Imm::new_i32(8), src: Reg(10), base: Reg(1) }); // *(x1+8) = x10
    emit!(Inst::Addi { imm: Imm::new_i32(-1), dest: Reg(11), src1: Reg(11) }); // x11--
    emit!(Inst::Bne { offset: Imm::new_i32(loop_pc.wrapping_sub(pc) as i64 as i32), src1: Reg(11), src2: Reg(0) });
    // Faulting store to an unmapped VA.
    emit_li(r, &mut pc, Reg(3), FAULT_VA);
    emit!(Inst::Sd { offset: Imm::new_i32(0), src: Reg(0), base: Reg(3) });
    // After the handler srets back here: jump to the spin loop.
    emit!(Inst::Jal { offset: Imm::new_i32(SPIN.wrapping_sub(pc) as i64 as i32), dest: Reg(0) });
    // The spin loop lives AT SPIN.
    w(r, SPIN, &Inst::Jal { offset: Imm::new_i32(0), dest: Reg(0) });

    // S-mode fault handler at STVEC: record sepc, sepc += 4, sret.
    let mut pc = 0x9100_0000u64;
    macro_rules! emit {
        ($inst:expr) => {{
            w(r, pc, &$inst);
            pc += 4;
        }};
    }
    emit!(Inst::Csrrs { csr: Csr::SEPC, dest: Reg(12), src: Reg(0) });
    emit!(Inst::Csrrs { csr: Csr::SEPC, dest: Reg(13), src: Reg(0) });
    emit!(Inst::Addi { imm: Imm::new_i32(4), dest: Reg(13), src1: Reg(13) });
    emit!(Inst::Csrrw { csr: Csr::SEPC, dest: Reg(0), src: Reg(13) });
    emit!(Inst::Sret);
}

#[test]
fn trace_and_step_parity_under_sv39() {
    // Baseline: step_one.
    let a = {
        let r = Reactor::new();
        setup(&r);
        build_program(&r);
        let mut regs = [0u64; 32];
        let mut pc = CODE;
        for _ in 0..4000 {
            match r.step_one(pc, &mut regs).unwrap() {
                StepOutcome::Next(n) => pc = n,
                StepOutcome::LegacyEcall(_) => panic!("unexpected"),
            }
            if pc == SPIN {
                break;
            }
        }
        assert_eq!(pc, SPIN, "baseline reached the spin loop");
        let mut mem = Vec::new();
        for i in 0..16u64 {
            mem.push(r.read_guest_byte(DATA_PA + i));
        }
        (regs, mem)
    };

    // Ported engine: trace_run_fueled.
    let b = {
        let r = Reactor::new();
        setup(&r);
        build_program(&r);
        let mut never = async |_h: &mut vane_arch::interp::LocalStackHost<Reactor>| -> bool {
            panic!("no ecall in this program")
        };
        // Drive dispatch until the spin pc is reached (fuel is generous).
        let mut last = CODE;
        for _ in 0..400 {
            match futures_block_on(r.trace_run_fueled(last, &mut never, Some(50))) {
                Ok(pc) => {
                    println!("dispatch: {last:#x} -> {pc:#x}");
                    last = pc;
                    if pc == SPIN {
                        break;
                    }
                }
                Err(e) => panic!("trace_run failed at pc={:#x}: {}", last, e),
            }
        }
        assert_eq!(last, SPIN, "trace engine reached the spin loop");
        let mut regs = [0u64; 32];
        use vane_arch::interp::LocalReactor;
        for i in 1..32u8 {
            regs[i as usize] = r.lr_load_reg(i);
        }
        let mut mem = Vec::new();
        for i in 0..16u64 {
            mem.push(r.read_guest_byte(DATA_PA + i));
        }
        (regs, mem)
    };

    if a != b {
        for i in 0..32 {
            if a.0[i] != b.0[i] {
                println!("x{i}: stepper={:#x} trace={:#x}", a.0[i], b.0[i]);
            }
        }
        for i in 0..16 {
            if a.1[i] != b.1[i] {
                println!("mem[{i}]: stepper={:#x} trace={:#x}", a.1[i], b.1[i]);
            }
        }
    }
    assert_eq!(a, b, "trace engine and stepper diverged under Sv39");
    // The loop ran 3 times: x10 = 3 * 0x1111222233334444.
    assert_eq!(a.0[10], 3u64.wrapping_mul(0x1111_2222_3333_4444));
    // The fault handler ran (sepc recorded in x12 = the faulting sd's pc).
    assert_ne!(a.0[12], 0, "the fault handler ran");
}

/// Minimal block_on (the trace engine's futures never actually pend).
fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

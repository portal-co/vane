//! M4 machine-model payload (PLAN_LINUX_WASM.md §7.4): exercises the
//! ns16550a UART, ACLINT timer, PLIC, and SiFive syscon end-to-end under
//! `vane-cli --machine`, without a kernel.
//!
//! Flow (all M-mode):
//! 1. UART TX: write "M1:" to THR; check LSR.THRE
//! 2. ACLINT: mtimecmp = 0 → MTIP timer interrupt fires, handler disarms
//! 3. UART RX: a host-fed byte arrives (vane-cli --uart-rx), IER.RDA set
//!    → PLIC source 10 pends → MEIP external interrupt → handler claims it
//! 4. syscon poweroff (vane-cli exits 0)
//!
//! Markers written to the UART let the gate script verify progress.

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};

const UART: usize = 0x1000_0000;
const PLIC: usize = 0x0C00_0000;
const MTIMER_MTIMECMP: usize = 0x200_4000;
const SYSCON: usize = 0x10_0000;
const PLIC_CLAIM_CTX0: usize = PLIC + 0x20_0004;

fn w8(addr: usize, v: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, v) }
}
fn r8(addr: usize) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}
fn w32(addr: usize, v: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}
fn w64(addr: usize, v: u64) {
    unsafe { core::ptr::write_volatile(addr as *mut u64, v) }
}

fn uart_tx(s: &str) {
    for b in s.bytes() {
        w8(UART, b);
    }
}

fn poweroff(code: u32) -> ! {
    w32(
        SYSCON,
        if code == 0 {
            0x5555
        } else {
            (code << 16) | 0x3333
        },
    );
    loop {}
}

#[unsafe(no_mangle)]
static mut TIMER_HITS: u64 = 0;
#[unsafe(no_mangle)]
static mut PLIC_HITS: u64 = 0;
#[unsafe(no_mangle)]
static mut CLAIMED: u64 = 0;
#[unsafe(no_mangle)]
static mut RX_BYTE: u64 = 0;

/// M-mode trap handler: MTIP (int 7) → count + disarm timer; MEIP (int 11)
/// → pop the UART RX byte (clears the IRQ source), count, PLIC
/// claim+complete. Anything else powers off with fail code 42.
#[unsafe(naked)]
extern "C" fn m_trap() {
    naked_asm!(
        "csrr t0, mcause",
        // Build the interrupt causes without giant literals:
        // t1 = (1<<63) | cause
        "li t1, 1",
        "slli t1, t1, 63",
        "addi t2, t1, 7", // MTIP
        "beq t0, t2, 2f",
        "addi t2, t1, 11", // MEIP
        "beq t0, t2, 3f",
        // unexpected trap
        "li t2, 0x100000",
        "li t3, 0x2a3333", // (42<<16)|0x3333
        "sw t3, 0(t2)",
        "1: j 1b",
        "2:", // timer
        "la t2, TIMER_HITS",
        "ld t3, 0(t2)",
        "addi t3, t3, 1",
        "sd t3, 0(t2)",
        // disarm: mtimecmp = -1
        "li t2, 0x2004000",
        "li t3, -1",
        "sd t3, 0(t2)",
        "mret",
        "3:", // PLIC external
        "la t2, PLIC_HITS",
        "ld t3, 0(t2)",
        "addi t3, t3, 1",
        "sd t3, 0(t2)",
        // PLIC claim (ctx0) FIRST — popping the UART byte clears the
        // interrupt source, and our PLIC's pending bit follows it
        // (level-triggered); claiming after the pop would see nothing.
        "li t2, 0xc200004",
        "lw t4, 0(t2)",
        "la t3, CLAIMED",
        "sd t4, 0(t3)",
        // pop the UART byte (clears the interrupt source)
        "li t2, 0x10000000",
        "lbu t4, 0(t2)",
        "la t2, RX_BYTE",
        "sd t4, 0(t2)",
        // PLIC complete
        "li t2, 0xc200004",
        "sw t4, 0(t2)",
        "mret",
    );
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        // A small scratch stack at the top of the first MiB of RAM.
        "li sp, 0x80800000",
        "j main_entry",
    );
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    unsafe {
        asm!("csrw mtvec, {}", in(reg) m_trap as *const () as usize);
    }

    // 1. UART TX + LSR.THRE.
    uart_tx("M1:");
    if r8(UART + 5) & 0x60 == 0 {
        poweroff(43);
    }
    uart_tx("T");

    // 2. ACLINT timer interrupt.
    w64(MTIMER_MTIMECMP, 0); // mtip immediately
    unsafe {
        asm!(
            "li t0, 0x80", // MTIE
            "csrs mie, t0",
            "csrsi mstatus, 0x8", // MIE
            out("t0") _,
        );
    }
    // The timer interrupt fires before the next instruction; mret returns here.
    unsafe { core::arch::asm!("nop") };
    if unsafe { core::ptr::read_volatile(&raw const TIMER_HITS) } != 1 {
        poweroff(44);
    }
    uart_tx("T");

    // 3. UART RX byte → PLIC source 10 → MEIP claim.
    w8(UART + 1, 1); // IER: RDA interrupt enable
    w32(PLIC + 10 * 4, 7); // priority[10] = 7
    w32(PLIC + 0x2000, 1 << 10); // enable ctx0 source 10
    w32(PLIC + 0x20_0000, 0); // threshold 0
    unsafe {
        asm!(
            "li t0, 0x800", // MEIE
            "csrs mie, t0",
            out("t0") _,
        );
    }
    // Wait for the external interrupt. It may arrive after this point when
    // the RX byte is fed at runtime (e.g. the M7 stdin pump) rather than
    // pre-staged via --uart-rx, so spin (bounded) instead of a single nop.
    let mut spins = 0u32;
    while unsafe { core::ptr::read_volatile(&raw const PLIC_HITS) } != 1 {
        spins += 1;
        if spins > 20_000_000 {
            poweroff(45);
        }
        unsafe { core::arch::asm!("nop") };
    }
    if unsafe { core::ptr::read_volatile(&raw const CLAIMED) } != 10 {
        poweroff(46);
    }
    if unsafe { core::ptr::read_volatile(&raw const RX_BYTE) } != b'V' as u64 {
        poweroff(47);
    }
    uart_tx("T");

    // 4. syscon poweroff (pass).
    unsafe {
        asm!("csrci mstatus, 0x8");
    }
    uart_tx("K\n");
    poweroff(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    poweroff(15);
}

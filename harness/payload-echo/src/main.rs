//! M7 stdin-pump validation payload: poll the UART for a byte (LSR.RDA),
//! read it, echo it back, then power off. Polling (not interrupts) makes this
//! immune to host-input delivery timing (the byte arrives between quanta).

#![no_std]
#![no_main]

use core::arch::naked_asm;

const UART: usize = 0x1000_0000;
const SYSCON: usize = 0x10_0000;

fn w8(addr: usize, v: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, v) }
}
fn r8(addr: usize) -> u8 {
    unsafe { core::ptr::read_volatile(addr as *const u8) }
}
fn uart_tx(s: &str) {
    for b in s.bytes() {
        w8(UART, b);
    }
}
fn poweroff(code: u32) -> ! {
    unsafe {
        core::ptr::write_volatile(
            SYSCON as *mut u32,
            if code == 0 {
                0x5555
            } else {
                (code << 16) | 0x3333
            },
        )
    };
    loop {}
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! {
    naked_asm!("li sp, 0x80800000", "j main_entry");
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    uart_tx("ECHO:");
    // Poll LSR (offset 5) bit 0 (RDA: receiver data ready) until a byte
    // arrives, then read RBR (offset 0) and echo it.
    let b = loop {
        if r8(UART + 5) & 1 != 0 {
            break r8(UART);
        }
    };
    w8(UART, b);
    uart_tx(":OK\n");
    poweroff(if b == b'V' { 0 } else { 60 });
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    poweroff(15);
}

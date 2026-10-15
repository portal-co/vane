//! Minimal RV64 payload for the M0 gate: write "hello" to the emulated
//! console via the vane-cli syscall shim, then exit.
//!
//! This speaks the *guest* Linux syscall ABI (a7 = nr, a0..a2 = args):
//! - write(1, msg, len) with a7 = 64
//! - exit(0) with a7 = 93
//!
//! It runs on bare emulated hardware (no OS), so it is `no_std`/`no_main`
//! and is linked at 0x8000_0000 by `link.x`.

#![no_std]
#![no_main]

use core::arch::asm;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let msg = b"hello from vane wasip1\n";
    unsafe {
        asm!(
            "ecall",
            in("a7") 64u64,
            in("a0") 1u64,
            in("a1") msg.as_ptr() as u64,
            in("a2") msg.len() as u64,
            lateout("a0") _,
        );
        asm!(
            "ecall",
            in("a7") 93u64,
            in("a0") 0u64,
            options(noreturn),
        );
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

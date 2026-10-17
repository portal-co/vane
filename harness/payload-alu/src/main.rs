//! Pure-ALU benchmark: register-heavy compute loop, no memory traffic, to
//! isolate the trace engine's per-instruction compute overhead.
#![no_std]
#![no_main]
use core::arch::naked_asm;
const UART: usize = 0x1000_0000;
const SYSCON: usize = 0x10_0000;
fn w8(a: usize, v: u8) { unsafe { core::ptr::write_volatile(a as *mut u8, v) } }
fn uart(s: &str) { for b in s.bytes() { w8(UART, b) } }
fn off(c: u32) -> ! { unsafe { core::ptr::write_volatile(SYSCON as *mut u32, if c==0 {0x5555} else {(c<<16)|0x3333}) }; loop {} }

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! { naked_asm!("li sp, 0x80800000", "j main_entry") }

#[inline(never)]
fn compute(mut a: u64, mut b: u64, mut c: u64, mut d: u64) -> u64 {
    // ~30M iterations of register-only arithmetic.
    let mut i = 0u64;
    while i < 30_000_000 {
        a = a.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(b);
        b ^= a >> 13;
        c = c.wrapping_add(a ^ b).rotate_left(7);
        d = d.wrapping_sub(c).wrapping_mul(0x9E3779B97F4A7C15);
        a ^= d;
        i += 1;
    }
    a ^ b ^ c ^ d
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    uart("ALU:");
    let r = compute(1, 2, 3, 4);
    // print low byte as hex-ish marker
    w8(UART, b'0' + ((r & 0xF) as u8).min(9));
    uart(":OK\n");
    off(0);
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { off(15) }

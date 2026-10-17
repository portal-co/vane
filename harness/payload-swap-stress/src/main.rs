//! M5 / S4 swap-stress payload (PLAN_LINUX_WASM.md §6, exit criterion S4):
//! drives more guest RAM than the resident-set cap through the encrypted
//! swap and verifies integrity with the workload's own checksum.
//!
//! The payload writes a per-page xorshift64* word pattern across a RAM region
//! larger than the resident cap, then reads it back and re-verifies every
//! word, accumulating a checksum. Under `--resident <cap>` smaller than the
//! region, pages are evicted to (and faulted back from) the encrypted
//! backing store; a correct round trip proves both the eviction/fault-in
//! mechanics and the AEAD confidentiality/integrity of the swapped pages.
//!
//! Reports "S4:<checksum-hex>:OK" then powers off via syscon; on mismatch
//! reports "S4:MISMATCH:<page>:<idx>" and powers off with a nonzero code.

#![no_std]
#![no_main]

use core::arch::naked_asm;

const UART: usize = 0x1000_0000;
const SYSCON: usize = 0x10_0000;

/// Stress region base: above the payload (which lives at 0x8000_0000+).
const STRESS_BASE: usize = 0x8100_0000;
/// Number of 64 KiB pages to cycle. With a small resident cap this forces
/// most of the region through the encrypted swap. 96 pages = 6 MiB.
const STRESS_PAGES: usize = 96;
const PAGE_BYTES: usize = 0x1_0000;
const WORDS_PER_PAGE: usize = PAGE_BYTES / 8;

fn w8(addr: usize, v: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, v) }
}
fn uart_tx(s: &str) {
    for b in s.bytes() {
        w8(UART, b);
    }
}
fn uart_hex(mut v: u64) {
    let mut buf = [0u8; 16];
    for i in 0..16 {
        let nib = (v >> 60) & 0xF;
        buf[i] = if nib < 10 {
            b'0' + nib as u8
        } else {
            b'a' + (nib as u8 - 10)
        };
        v <<= 4;
    }
    for b in buf {
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

/// xorshift64* PRNG.
struct Rng(u64);
impl Rng {
    fn for_page(page: usize) -> Self {
        // Nonzero, page-derived seed.
        Rng((page as u64)
            .wrapping_add(1)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn fill_page(page: usize) {
    let base = STRESS_BASE + page * PAGE_BYTES;
    let mut rng = Rng::for_page(page);
    for w in 0..WORDS_PER_PAGE {
        let val = rng.next();
        unsafe { core::ptr::write_volatile((base + w * 8) as *mut u64, val) };
    }
}

/// Verify a page; on success fold its words into `checksum`. Returns Err(idx)
/// on the first mismatched word.
fn verify_page(page: usize, checksum: &mut u64) -> Result<(), usize> {
    let base = STRESS_BASE + page * PAGE_BYTES;
    let mut rng = Rng::for_page(page);
    for w in 0..WORDS_PER_PAGE {
        let want = rng.next();
        let got = unsafe { core::ptr::read_volatile((base + w * 8) as *const u64) };
        if got != want {
            return Err(w);
        }
        *checksum ^= got;
    }
    Ok(())
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        // Scratch stack at the top of the first MiB.
        "li sp, 0x80800000",
        "j main_entry",
    );
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    uart_tx("S4:write\n");
    for p in 0..STRESS_PAGES {
        fill_page(p);
    }
    uart_tx("S4:verify\n");
    let mut checksum: u64 = 0;
    for p in 0..STRESS_PAGES {
        if let Err(idx) = verify_page(p, &mut checksum) {
            uart_tx("S4:MISMATCH:");
            uart_hex(p as u64);
            w8(UART, b':');
            uart_hex(idx as u64);
            w8(UART, b'\n');
            poweroff(50);
        }
    }
    uart_tx("S4:");
    uart_hex(checksum);
    uart_tx(":OK\n");
    poweroff(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    poweroff(15);
}

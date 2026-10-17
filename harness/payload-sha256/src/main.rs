//! Bare-metal SHA-256 test payload: computes SHA-256 of a fixed string and
//! writes the hex digest to the UART. Used to isolate ALU bugs from the
//! full Linux userspace stack.
#![no_std]
#![no_main]
use core::arch::naked_asm;

const UART: usize = 0x1000_0000;
fn w8(addr: usize, v: u8) {
    unsafe { core::ptr::write_volatile(addr as *mut u8, v) }
}
fn uart_tx(s: &str) {
    for b in s.bytes() {
        w8(UART, b);
    }
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u32; 8] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // Fixed-size buffer (no_std, no alloc): pad in place.
    let mut buf = [0u8; 128];
    buf[..data.len()].copy_from_slice(data);
    let bitlen = (data.len() as u64) * 8;
    buf[data.len()] = 0x80;
    let total = (data.len() + 1 + 8 + 63) & !63; // round up to a 64-block
    buf[total - 8..total].copy_from_slice(&bitlen.to_be_bytes());

    for chunk in buf[..total].as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! {
    naked_asm!("li sp, 0x80800000", "j main_entry");
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    let digest = sha256(b"hello-from-busybox\n");
    let mut hex = [0u8; 64];
    let hexd = b"0123456789abcdef";
    for (i, w) in digest.iter().enumerate() {
        for j in 0..4 {
            let byte = (w >> ((3 - j) * 8)) as u8;
            hex[i * 8 + j * 2] = hexd[(byte >> 4) as usize];
            hex[i * 8 + j * 2 + 1] = hexd[(byte & 15) as usize];
        }
    }
    uart_tx("sha256(hello-from-busybox\\n)=");
    uart_tx(core::str::from_utf8(&hex).unwrap());
    uart_tx("\n");
    // power off via syscon
    unsafe { core::ptr::write_volatile(0x100000 as *mut u32, 0x5555) };
    loop {}
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    uart_tx("PANIC\n");
    if let Some(loc) = info.location() {
        uart_tx("at ");
        uart_tx(loc.file());
        uart_tx(":");
        let mut n = [0u8; 10];
        let mut l = loc.line();
        let mut i = 10;
        while l > 0 {
            i -= 1;
            n[i] = b'0' + (l % 10) as u8;
            l /= 10;
        }
        uart_tx(core::str::from_utf8(&n[i..]).unwrap());
    }
    uart_tx("\n");
    loop {}
}

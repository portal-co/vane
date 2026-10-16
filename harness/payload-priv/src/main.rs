//! M3 E2E payload (PLAN_LINUX_WASM.md §7): exercises the privileged
//! architecture end-to-end under `vane-cli --hart`:
//!
//! 1. M-mode: CSR read/write (mtvec, mscratch, medeleg, satp)
//! 2. Sv39 setup: identity megapage for code+data, one 4 KiB mapping of a
//!    distinct physical page at VA 0x4000_0000
//! 3. mret into S-mode; ecall from S traps to M (not delegated)
//! 4. Translated load/store through the Sv39 mapping
//! 5. Store page fault delegated to the S-mode handler
//! 6. Second ecall, now delegated to S (medeleg bit set by the M handler)
//! 7. Report via `tohost` (riscv-tests convention: (code<<1)|1, 1 = pass)

#![no_std]
#![no_main]

use core::arch::{asm, naked_asm};

const MAGIC: u64 = 0x5ec0_de42_1234_5678;
const MAP_VA: u64 = 0x4000_0000;
const FAULT_VA: u64 = 0x6000_0000;

#[unsafe(link_section = ".tohost")]
#[unsafe(no_mangle)]
static mut TOHOST: u64 = 0;

#[unsafe(no_mangle)]
static mut COUNTER_M: u64 = 0;
#[unsafe(no_mangle)]
static mut FLAG_S_FAULT: u64 = 0;
#[unsafe(no_mangle)]
static mut FLAG_S_ECALL: u64 = 0;

#[repr(align(4096))]
struct Pt([u64; 512]);

#[repr(align(4096))]
struct Page([u8; 4096]);

#[unsafe(no_mangle)]
static mut ROOT: Pt = Pt([0; 512]);
#[unsafe(no_mangle)]
static mut L2T: Pt = Pt([0; 512]);
#[unsafe(no_mangle)]
static mut L3T: Pt = Pt([0; 512]);
#[unsafe(no_mangle)]
static mut DATA: Page = Page([0; 4096]);

fn pass() -> ! {
    unsafe { core::ptr::write_volatile(&raw mut TOHOST, 1) };
    loop {}
}

fn fail(code: u64) -> ! {
    unsafe { core::ptr::write_volatile(&raw mut TOHOST, (code << 1) | 1) };
    loop {}
}

/// M-mode trap handler: expects ecall-from-S (cause 9). Counts it, sets
/// medeleg bit 9 (so the *next* ecall delegates to S), skips the trapping
/// instruction, and mrets. Any other cause writes fail code 2 to tohost.
#[unsafe(naked)]
extern "C" fn m_trap() {
    naked_asm!(
        "csrr t0, mcause",
        "li t1, 9",
        "bne t0, t1, 2f",
        "la t2, COUNTER_M",
        "ld t3, 0(t2)",
        "addi t3, t3, 1",
        "sd t3, 0(t2)",
        "csrr t2, medeleg",
        "ori t2, t2, 0x200",
        "csrw medeleg, t2",
        "csrr t2, mepc",
        "addi t2, t2, 4",
        "csrw mepc, t2",
        "mret",
        "2:",
        "la t2, TOHOST",
        "li t3, 5", // (2<<1)|1
        "sd t3, 0(t2)",
        "3: j 3b",
    );
}

/// S-mode trap handler: cause 9 (delegated ecall) → set FLAG_S_ECALL;
/// cause 15 (store page fault) → check stval == FAULT_VA and set
/// FLAG_S_FAULT. Returns past the trapping instruction. Anything else
/// writes fail code 3/4 to tohost.
#[unsafe(naked)]
extern "C" fn s_trap() {
    naked_asm!(
        "csrr t0, scause",
        "li t1, 9",
        "beq t0, t1, 2f",
        "li t1, 15",
        "beq t0, t1, 3f",
        // unexpected cause: fail(3)
        "la t2, TOHOST",
        "li t3, 7", // (3<<1)|1
        "sd t3, 0(t2)",
        "1: j 1b",
        "2:", // delegated ecall
        "la t2, FLAG_S_ECALL",
        "li t3, 1",
        "sd t3, 0(t2)",
        "j 4f",
        "3:", // store page fault
        "csrr t0, stval",
        "li t1, 0x60000000",
        "beq t0, t1, 5f",
        // wrong stval: fail(4)
        "la t2, TOHOST",
        "li t3, 9", // (4<<1)|1
        "sd t3, 0(t2)",
        "1: j 1b",
        "5:",
        "la t2, FLAG_S_FAULT",
        "li t3, 1",
        "sd t3, 0(t2)",
        // Skip the faulting instruction by its true length (compressed
        // stores are 2 bytes): (inst & 3) == 3 means 4 bytes.
        "csrr t2, sepc",
        "lhu t3, 0(t2)",
        "andi t3, t3, 3",
        "li t4, 3",
        "beq t3, t4, 6f",
        "addi t2, t2, 2",
        "j 7f",
        "6: addi t2, t2, 4",
        "7: csrw sepc, t2",
        "sret",
        "4:", // delegated ecall path: ecall is always 4 bytes
        "csrr t2, sepc",
        "addi t2, t2, 4",
        "csrw sepc, t2",
        "sret",
    );
}

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text._start")]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        // A small scratch stack at the top of the identity megapage.
        "li sp, 0x80800000",
        "j main_entry",
    );
}

#[unsafe(no_mangle)]
extern "C" fn main_entry() -> ! {
    unsafe {
        // 1. mtvec + mscratch CSR roundtrip.
        asm!("csrw mtvec, {}", in(reg) m_trap as *const () as usize);
        asm!("csrw mscratch, {}", in(reg) 0x1234567usize);
        let got: usize;
        asm!("csrr {}, mscratch", out(reg) got);
        if got != 0x1234567 {
            fail(1);
        }
    }
    {
        // 2. Sv39 tables.
        unsafe {
            // DATA page content (checked later through the mapping).
            core::ptr::write_volatile(&raw mut DATA as *mut u64, MAGIC);

            let root = &raw mut ROOT as *mut u64;
            let l2 = &raw mut L2T as *mut u64;
            let l3 = &raw mut L3T as *mut u64;
            let data = &raw mut DATA as u64;

            const V: u64 = 1;
            const RWXADG: u64 = 2 | 4 | 8 | 64 | 128 | 32;
            const RWAD: u64 = 2 | 4 | 64 | 128;

            // root[2]: 1 GiB identity megapage for 0x8000_0000.. (code/data)
            root.add(2)
                .write_volatile((0x8000_0000 >> 12 << 10) | RWXADG | V);
            // root[1] -> L2 (VA 0x4000_0000 lives under index 1)
            root.add(1).write_volatile(((l2 as u64) >> 12 << 10) | V);
            // l2[0] -> L3
            l2.write_volatile(((l3 as u64) >> 12 << 10) | V);
            // l3[0]: VA 0x4000_0000 -> &DATA (RW, no X, no U)
            l3.write_volatile((data >> 12 << 10) | RWAD | V);

            // 3. satp = Sv39 | root ppn; fence.
            let satp: usize = (8 << 60) | ((root as usize) >> 12);
            asm!("csrw satp, {}", "sfence.vma", in(reg) satp);

            // Delegate load/store page faults to S.
            asm!("csrw medeleg, {}", in(reg) (1usize << 13) | (1usize << 15));

            // 4. Enter S-mode at s_entry (identity-mapped): MPP=S, mepc, mret.
            asm!(
                "li t0, 0x1800",
                "csrc mstatus, t0",
                "li t0, 0x0800",
                "csrs mstatus, t0",
                "csrw mepc, {}",
                "mret",
                in(reg) s_entry as *const () as usize,
                out("t0") _,
            );
        }
    }
    // Unreachable: mret diverges into s_entry.
    loop {}
}

#[unsafe(no_mangle)]
extern "C" fn s_entry() -> ! {
    unsafe {
        asm!("csrw stvec, {}", in(reg) s_trap as *const () as usize);

        // 5. ecall from S → M handler (not delegated yet).
        asm!("ecall");

        // 6. Translated load/store through the Sv39 mapping.
        let p = MAP_VA as *const u64;
        if core::ptr::read_volatile(p) != MAGIC {
            fail(4);
        }
        core::ptr::write_volatile((MAP_VA + 8) as *mut u64, 0x1234);
        if core::ptr::read_volatile((MAP_VA + 8) as *const u64) != 0x1234 {
            fail(5);
        }

        // 7. Store to an unmapped VA → store page fault → s_trap.
        core::ptr::write_volatile(FAULT_VA as *mut u64, 1);

        // 8. ecall again — now delegated to S by the M handler.
        asm!("ecall");

        // 9. Verify everything happened.
        if core::ptr::read_volatile(&raw const COUNTER_M) != 1 {
            fail(6);
        }
        if core::ptr::read_volatile(&raw const FLAG_S_FAULT) != 1 {
            fail(7);
        }
        if core::ptr::read_volatile(&raw const FLAG_S_ECALL) != 1 {
            fail(8);
        }
        pass()
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    fail(15);
}

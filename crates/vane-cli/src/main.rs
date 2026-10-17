//! `vane-cli` — self-driving vane host.
//!
//! Two modes:
//!
//! - **Userspace-shim mode (default)**: load a static RV64 ELF and bridge a
//!   minimal Linux syscall set (write/read/exit) to stdio. Used by the M0
//!   gate.
//! - **Machine mode (`--hart --machine`)**: boot with the privileged
//!   architecture and the QEMU-virt device model (UART/ACLINT/PLIC/syscon).
//!   Optionally boots OpenSBI + a Linux Image + initrd + generated DTB.
//!
//! Usage:
//!   vane-cli --elf <payload.elf> [--hart] [--tohost 0xADDR]
//!   vane-cli --machine --opensbi fw_jump.bin --kernel Image [--initrd INITRD]
//!            [--append "console=ttyS0 ..."] [--ram-size 128MiB]

mod elf;

use vane::Reactor;
use vane_host::HostIo;

const ENOSYS: u64 = (-38i64) as u64;

/// Stdout-backed host console.
struct StdioHost;

impl HostIo for StdioHost {
    fn uart_write(&mut self, byte: u8) {
        use std::io::Write;
        let _ = std::io::stdout().write_all(&[byte]);
    }
}

struct Args {
    elf_path: Option<String>,
    entry: Option<u64>,
    max_insns: Option<u64>,
    /// Enable the privileged architecture (M3): boot in M-mode with the
    /// trap engine and Sv39 live. In this mode ecall does NOT reach the
    /// syscall shim — the guest's own trap handlers run.
    hart: bool,
    /// riscv-tests convention: poll this guest address between fueled
    /// interpreter rounds; a nonzero value with bit 0 set ends the run with
    /// exit code `value >> 1`.
    tohost: Option<u64>,
    /// Attach the QEMU-virt machine (M4).
    machine: bool,
    /// OpenSBI firmware binary (flat, loaded at 0x8000_0000).
    opensbi: Option<String>,
    /// Kernel image (flat or ELF, loaded at 0x8020_0000).
    kernel: Option<String>,
    /// initrd (cpio archive, loaded at 0x8400_0000).
    initrd: Option<String>,
    /// Kernel command line for the DTB's /chosen/bootargs.
    append: Option<String>,
    /// RAM size in bytes (decimal or 0x-hex).
    ram_size: u64,
    /// Bytes to inject into the machine UART's RX queue at boot (test
    /// payloads read them via the emulated 16550).
    uart_rx: Option<Vec<u8>>,
    /// Execution engine: `trace` (StackOp traces, the default — the StackOp
    /// port) or `step` (per-instruction step_one reference).
    engine: String,
    /// Encrypted swap backing-store file (M5). Requires --machine.
    swap: Option<String>,
    /// Resident RAM cap in bytes (decimal or 0x-hex) when --swap is set.
    resident: Option<u64>,
    /// Optional 32-byte swap key file; otherwise a per-session random key is
    /// drawn from the host CSPRNG (a fresh key trivially invalidates old
    /// swap — by design, PLAN §6.2 item 3).
    swap_key: Option<String>,
}

const RAM_BASE: u64 = 0x8000_0000;
/// fw_dynamic_info structure address (QEMU convention: a2 points here).
const FW_DYNAMIC_INFO_ADDR: u64 = 0x87E0_0000;
const KERNEL_ADDR: u64 = 0x8020_0000;
const INITRD_ADDR: u64 = 0x8400_0000;
const DTB_ADDR: u64 = 0x87F0_0000;

fn parse_args() -> Result<Args, String> {
    let mut elf_path = None;
    let mut entry = None;
    let mut max_insns = None;
    let mut hart = false;
    let mut tohost = None;
    let mut machine = false;
    let mut opensbi = None;
    let mut kernel = None;
    let mut initrd = None;
    let mut append = None;
    let mut ram_size = 128 * 1024 * 1024; // 128 MiB bring-up default
    let mut uart_rx: Option<Vec<u8>> = None;
    let mut engine: Option<String> = None;
    let mut swap = None;
    let mut resident = None;
    let mut swap_key = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--elf" => elf_path = Some(it.next().ok_or("--elf needs a value")?),
            "--entry" => {
                let v = it.next().ok_or("--entry needs a value")?;
                let v = v.strip_prefix("0x").unwrap_or(&v);
                entry = Some(u64::from_str_radix(v, 16).map_err(|e| format!("--entry: {e}"))?);
            }
            "--max-insns" => {
                let v = it.next().ok_or("--max-insns needs a value")?;
                max_insns = Some(v.parse().map_err(|e| format!("--max-insns: {e}"))?);
            }
            "--hart" => hart = true,
            "--machine" => machine = true,
            "--tohost" => {
                let v = it.next().ok_or("--tohost needs a value")?;
                let v = v.strip_prefix("0x").unwrap_or(&v);
                tohost = Some(u64::from_str_radix(v, 16).map_err(|e| format!("--tohost: {e}"))?);
            }
            "--opensbi" => opensbi = Some(it.next().ok_or("--opensbi needs a value")?),
            "--kernel" => kernel = Some(it.next().ok_or("--kernel needs a value")?),
            "--initrd" => initrd = Some(it.next().ok_or("--initrd needs a value")?),
            "--append" => append = Some(it.next().ok_or("--append needs a value")?),
            "--uart-rx" => {
                let v = it.next().ok_or("--uart-rx needs a value")?;
                uart_rx = Some(v.into_bytes());
            }
            "--engine" => {
                engine = Some(it.next().ok_or("--engine needs a value")?);
            }
            "--swap" => swap = Some(it.next().ok_or("--swap needs a value")?),
            "--swap-key" => swap_key = Some(it.next().ok_or("--swap-key needs a value")?),
            "--resident" => {
                let v = it.next().ok_or("--resident needs a value")?;
                resident = Some(if let Some(h) = v.strip_prefix("0x") {
                    u64::from_str_radix(h, 16).map_err(|e| format!("--resident: {e}"))?
                } else {
                    v.parse().map_err(|e| format!("--resident: {e}"))?
                });
            }
            "--ram-size" => {
                let v = it.next().ok_or("--ram-size needs a value")?;
                ram_size = if let Some(h) = v.strip_prefix("0x") {
                    u64::from_str_radix(h, 16).map_err(|e| format!("--ram-size: {e}"))?
                } else {
                    v.parse().map_err(|e| format!("--ram-size: {e}"))?
                };
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        elf_path,
        entry,
        max_insns,
        hart,
        tohost,
        machine,
        opensbi,
        kernel,
        initrd,
        append,
        ram_size,
        engine: engine.unwrap_or_else(|| "trace".into()),
        uart_rx,
        swap,
        resident,
        swap_key,
    })
}

/// Load a binary image at `addr`; if it parses as ELF64, honor its segments
/// and return its entry point. Returns the entry pc.
fn load_image(reactor: &Reactor, path: &str, flat_addr: u64) -> u64 {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vane-cli: cannot read {path}: {e}");
            std::process::exit(2);
        }
    };
    match elf::load(reactor, &bytes) {
        Ok(entry) => {
            eprintln!("vane-cli: {path}: ELF, entry {entry:#x}");
            entry
        }
        Err(_) => {
            // Flat binary at the fixed address.
            reactor.write_bytes(flat_addr, &bytes);
            eprintln!(
                "vane-cli: {path}: flat, {} bytes at {flat_addr:#x}",
                bytes.len()
            );
            flat_addr
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vane-cli: {e}");
            eprintln!(
                "usage: vane-cli --elf <path> [--hart] [--tohost 0xADDR] | --machine --opensbi F --kernel K [--initrd I] [--append CMDLINE]"
            );
            std::process::exit(2);
        }
    };
    let reactor = Reactor::new();

    let entry;
    if args.machine {
        // Machine mode: devices + privileged architecture.
        reactor.set_hart_enabled(true);
        reactor.set_machine_enabled(true);
        if let Some(rx) = &args.uart_rx {
            reactor.with_machine(|m| m.uart.push_rx(rx));
        }

        // M5: encrypted swap for the RAM region. Set up BEFORE loading images
        // so they land in the swap-backed store.
        if let Some(swap_path) = &args.swap {
            let resident = args.resident.unwrap_or(args.ram_size / 2).max(0x1_0000);
            let frames = (resident / 0x1_0000).max(1) as usize;
            let slots = args.ram_size / 0x1_0000; // one slot per RAM page
            let store = vane_swap::FileSwap::create(std::path::Path::new(swap_path), slots)
                .unwrap_or_else(|e| {
                    eprintln!("vane-cli: cannot create swap file {swap_path}: {e}");
                    std::process::exit(2);
                });
            let key: [u8; 32] = match &args.swap_key {
                Some(kf) => {
                    let b = std::fs::read(kf).expect("swap key file");
                    if b.len() != 32 {
                        eprintln!("vane-cli: swap key file must be exactly 32 bytes");
                        std::process::exit(2);
                    }
                    let mut k = [0u8; 32];
                    k.copy_from_slice(&b);
                    k
                }
                None => {
                    let mut k = [0u8; 32];
                    getrandom::fill(&mut k).expect("CSPRNG for swap key");
                    k
                }
            };
            let cache = vane_swap::PageCache::new(
                frames,
                Box::new(store),
                key,
                Box::new(|| {
                    let mut n = [0u8; vane_swap::NONCE_BYTES];
                    getrandom::fill(&mut n).expect("CSPRNG for swap nonce");
                    n
                }),
            );
            reactor.enable_swap(RAM_BASE, args.ram_size, cache);
            eprintln!(
                "vane-cli: swap on {swap_path} (resident {} MiB of {} MiB RAM, {} frames)",
                resident / (1024 * 1024),
                args.ram_size / (1024 * 1024),
                frames
            );
        }

        let mut next_addr = RAM_BASE;
        if let Some(p) = &args.opensbi {
            load_image(&reactor, p, RAM_BASE);
            next_addr = KERNEL_ADDR;
        }
        if let Some(p) = &args.kernel {
            load_image(&reactor, p, next_addr.max(KERNEL_ADDR));
        }
        let initrd_range = if let Some(p) = &args.initrd {
            let bytes = std::fs::read(p).expect("initrd");
            reactor.write_bytes(INITRD_ADDR, &bytes);
            Some((INITRD_ADDR, INITRD_ADDR + bytes.len() as u64))
        } else {
            None
        };
        // fw_dynamic_info for OpenSBI's fw_dynamic firmware
        // (https://github.com/riscv-software-src/opensbi include/fw_dynamic.h):
        // magic "OSBI" (0x4942534F), version 2, next_addr/next_mode (1=S),
        // options 0, boot_hart 0.
        if args.opensbi.is_some() {
            let kernel_entry = KERNEL_ADDR;
            let info: [u64; 6] = [0x4942_534F, 2, kernel_entry, 1, 0, 0];
            let mut bytes = Vec::new();
            for w in info {
                bytes.extend_from_slice(&w.to_le_bytes());
            }
            reactor.write_bytes(FW_DYNAMIC_INFO_ADDR, &bytes);
        }
        let bootargs = args.append.as_deref().unwrap_or("console=ttyS0");
        let dtb = vane::vane_riscv::machine::virt_dtb(args.ram_size, initrd_range, bootargs);
        reactor.write_bytes(DTB_ADDR, &dtb);

        // Pin the boot artifacts so they are never evicted (§6.2 item 2):
        // OpenSBI, kernel image, initrd, fw_dynamic_info, DTB. With swap on,
        // these are the always-resident working set.
        if reactor.swap_active() {
            reactor.pin_swap_range(RAM_BASE, KERNEL_ADDR - RAM_BASE); // OpenSBI
            if let Some(p) = &args.kernel {
                let klen = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                reactor.pin_swap_range(KERNEL_ADDR, klen);
            }
            if let Some((s, e)) = initrd_range {
                reactor.pin_swap_range(s, e - s);
            }
            reactor.pin_swap_range(FW_DYNAMIC_INFO_ADDR, 0x1_0000);
            reactor.pin_swap_range(DTB_ADDR, dtb.len() as u64);
        }

        // A bare-metal payload (ELF) may take the place of firmware+kernel
        // (used by the machine-model gate).
        entry = if let Some(p) = &args.elf_path {
            let bytes = std::fs::read(p).expect("elf payload");
            match elf::load(&reactor, &bytes) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("vane-cli: {p}: {e}");
                    std::process::exit(2);
                }
            }
        } else {
            RAM_BASE
        };
    } else {
        // Payload mode: a single ELF.
        let elf_path = match &args.elf_path {
            Some(p) => p.clone(),
            None => {
                eprintln!("vane-cli: need --elf <path> (or --machine with --opensbi/--kernel)");
                std::process::exit(2);
            }
        };
        let bytes = match std::fs::read(&elf_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("vane-cli: cannot read {elf_path}: {e}");
                std::process::exit(2);
            }
        };
        entry = match elf::load(&reactor, &bytes) {
            Ok(e) => args.entry.unwrap_or(e),
            Err(e) => {
                eprintln!("vane-cli: {elf_path}: {e}");
                std::process::exit(2);
            }
        };
        if args.hart {
            reactor.set_hart_enabled(true);
        }
    }

    // Boot register state for machine mode: a0 = hartid, a1 = dtb.
    let mut regs = [0u64; 32];
    if args.machine {
        regs[10] = 0; // hartid
        regs[11] = DTB_ADDR;
        regs[12] = FW_DYNAMIC_INFO_ADDR; // a2 for OpenSBI fw_dynamic
        // The trace engine's register file lives in the Reactor (Core.regs);
        // mirror the boot registers there too.
        reactor.write_reg(10, 0);
        reactor.write_reg(11, DTB_ADDR);
        reactor.write_reg(12, FW_DYNAMIC_INFO_ADDR);
    }

    let mut host = StdioHost;
    let mut exit_code: Option<u64> = None;
    let mut insns: u64 = 0;
    let mut pc = entry;

    // M7 (`wasix` feature): pump stdin on a background thread into a channel
    // so the interpreter loop never blocks on console input (and, under
    // WASIX, uses a real thread). The loop drains the channel each quantum.
    #[cfg(feature = "wasix")]
    let stdin_rx = {
        let (tx, rx) = std::sync::mpsc::channel::<u8>();
        if args.machine {
            std::thread::spawn(move || {
                use std::io::Read;
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 256];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,        // EOF
                        Ok(n) => {
                            for &b in &buf[..n] {
                                if tx.send(b).is_err() {
                                    return; // receiver gone
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Some(rx)
    };
    #[cfg(not(feature = "wasix"))]
    let stdin_rx: Option<std::sync::mpsc::Receiver<u8>> = None;

    let fueled = args.hart || args.machine;
    if fueled {
        // Privileged/machine mode: ecall traps in-guest; completion is
        // reported via tohost (payload tests) or the syscon (poweroff).
        const FUEL: u64 = 1_000_000;
        // Trace dispatch fuel is per-trace (a trace can run thousands of
        // instructions), so use a smaller count to keep the host tick rate
        // (UART drain, syscon, tohost) responsive.
        const FUEL_TRACE: u64 = 50_000;
        let mut total: u64 = 0;
        let t0 = std::time::Instant::now();
        loop {
            let result = if args.engine == "trace" {
                // The StackOp trace engine (the port): traces run translated
                // and MMIO-aware code with trap-atomic bails; privileged
                // instructions and interrupts dispatch through the stepper.
                let mut noop = async |_: &mut vane::vane_riscv::interp::LocalStackHost<
                    vane::Reactor,
                >|
                       -> bool { false };
                let result = futures::executor::block_on(reactor.trace_run_fueled(
                    pc,
                    &mut noop,
                    Some(FUEL_TRACE),
                ));
                drop(noop);
                result.map_err(vane::InterpError::Unsupported)
            } else {
                let mut noop = async |_: &mut [u64; 32]| -> bool { false };
                let result = futures::executor::block_on(reactor.interp_core_fueled(
                    pc,
                    &mut regs,
                    &mut noop,
                    Some(FUEL),
                ));
                drop(noop);
                result
            };
            match result {
                Ok(next_pc) => pc = next_pc,
                Err(e) => {
                    eprintln!("vane-cli: interpreter error at pc={pc:#x}: {e}");
                    std::process::exit(1);
                }
            }
            total += FUEL;

            // Heartbeat for boot debugging: VANE_HEARTBEAT=1 prints
            // pc/priv/mcause each round.
            if std::env::var_os("VANE_HEARTBEAT").is_some() {
                let (privm, mcause, mip) = reactor.with_hart(|h| (h.priv_mode, h.mcause, h.mip));
                let (mtime, mtimecmp) = reactor
                    .with_machine(|m| (m.aclint.mtime(), m.aclint.mtimecmp))
                    .unwrap_or((0, 0));
                eprintln!(
                    "[{total}M] pc={pc:#x} priv={privm:?} mcause={mcause:#x} mip={mip:#x} mtime={mtime} mtimecmp={mtimecmp:#x}"
                );
            }

            // Device tick: real time, drain UART TX to stdout.
            if args.machine {
                let _ = t0;
                // Drain any console input the stdin pump thread delivered
                // (empty when the `wasix` feature is off).
                let mut rx_bytes: Vec<u8> = Vec::new();
                if let Some(rx) = &stdin_rx {
                    while let Ok(b) = rx.try_recv() {
                        if std::env::var_os("VANE_DEBUG_STDIN").is_some() {
                            eprintln!("[stdin-pump] got byte {b:#x}");
                        }
                        rx_bytes.push(b);
                    }
                }
                let power = reactor.with_hart(|h| h.machine_tick(&rx_bytes));
                if let Some(tx) = reactor.with_machine(|m| m.uart.drain_tx()) {
                    for b in tx {
                        host.uart_write(b);
                    }
                }
                if let Some(p) = power {
                    use vane::vane_riscv::machine::PowerAction;
                    match p {
                        PowerAction::PowerOff { code } => std::process::exit(code as i32),
                        PowerAction::Reboot => {
                            eprintln!("vane-cli: guest requested reboot; stopping");
                            std::process::exit(0);
                        }
                    }
                }
            }
            if let Some(max) = args.max_insns
                && total >= max
            {
                eprintln!("vane-cli: instruction cap reached ({max})");
                std::process::exit(3);
            }
            // S5: a swap integrity/IO failure is a hard VM error, not silent
            // corruption. Halt and report.
            if let Some(se) = reactor.take_swap_error() {
                eprintln!(
                    "vane-cli: swap integrity/IO error: {se:?} (backing store tampered or failed; halting)"
                );
                std::process::exit(4);
            }
            if let Some(tohost) = args.tohost {
                let mut v = 0u64;
                for i in 0..8 {
                    v |= (reactor.read_guest_byte(tohost + i) as u64) << (8 * i);
                }
                if v & 1 == 1 {
                    std::process::exit((v >> 1) as u8 as i32);
                }
            }
        }
    }

    // Legacy userspace-shim mode.
    loop {
        let mut on_ecall = async |regs: &mut [u64; 32]| -> bool {
            let nr = regs[17];
            match nr {
                64 => {
                    // write(fd, buf, len)
                    let (fd, buf, len) = (regs[10], regs[11], regs[12]);
                    if fd == 1 || fd == 2 {
                        for i in 0..len {
                            host.uart_write(reactor.read_guest_byte(buf.wrapping_add(i)));
                        }
                    }
                    regs[10] = len;
                    false
                }
                63 => {
                    // read(fd, buf, len): report EOF.
                    regs[10] = 0;
                    false
                }
                93 | 94 => {
                    exit_code = Some(regs[10]);
                    true
                }
                _ => {
                    regs[10] = ENOSYS;
                    false
                }
            }
        };
        let result = futures::executor::block_on(reactor.interp_core(pc, &mut regs, &mut on_ecall));
        drop(on_ecall);
        match result {
            Ok(stop_pc) => {
                if let Some(code) = exit_code {
                    std::process::exit(code as u8 as i32);
                }
                // Stopped for a reason other than exit (currently
                // impossible: only exit stops). Resume defensively.
                pc = stop_pc + 4;
            }
            Err(e) => {
                eprintln!("vane-cli: interpreter error at pc={pc:#x}: {e}");
                std::process::exit(1);
            }
        }
        if let Some(max) = args.max_insns {
            insns += 1;
            if insns >= max {
                eprintln!("vane-cli: instruction cap reached");
                std::process::exit(3);
            }
        }
    }
}

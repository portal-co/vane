//! `vane-cli` — self-driving vane host (M0 of PLAN_LINUX_WASM.md).
//!
//! Loads a statically linked RV64 ELF and runs it under the interpreter
//! (`Reactor::interp_core` — no JS engine, no runtime codegen), bridging a
//! minimal Linux-userspace syscall set to stdio:
//!
//! - `write(64)`  fd 1/2 → host stdout
//! - `read(63)`   → EOF (0) for now
//! - `exit(93)` / `exit_group(94)` → stop, propagate the exit code
//! - everything else → `-ENOSYS`
//!
//! Usage: `vane-cli --elf <path> [--entry 0xADDR] [--max-insns N]`

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
    elf_path: String,
    entry: Option<u64>,
    max_insns: Option<u64>,
}

fn parse_args() -> Result<Args, String> {
    let mut elf_path = None;
    let mut entry = None;
    let mut max_insns = None;
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
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        elf_path: elf_path.ok_or("missing required --elf <path>")?,
        entry,
        max_insns,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vane-cli: {e}");
            eprintln!("usage: vane-cli --elf <path> [--entry 0xADDR] [--max-insns N]");
            std::process::exit(2);
        }
    };
    let bytes = match std::fs::read(&args.elf_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vane-cli: cannot read {}: {e}", args.elf_path);
            std::process::exit(2);
        }
    };
    let reactor = Reactor::new();
    let entry = match elf::load(&reactor, &bytes) {
        Ok(e) => args.entry.unwrap_or(e),
        Err(e) => {
            eprintln!("vane-cli: {}: {e}", args.elf_path);
            std::process::exit(2);
        }
    };

    let mut host = StdioHost;
    let mut exit_code: Option<u64> = None;
    let mut insns: u64 = 0;
    let mut regs = [0u64; 32];
    let mut pc = entry;

    // The interpreter only yields at ecall boundaries, so the instruction
    // cap is enforced between ecall-separated runs. Good enough for M0's
    // "stub payload" gate; a fueled/stepping interpreter is M1 work.
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

//! `vane-cli` — self-driving vane host.
//!
//! Two modes, both under the privileged architecture — the user-mode
//! syscall-shim "micro-OS" was removed deliberately (it is very noticeable
//! to a trained model: wrong uname, wrong /proc, wrong syscall surface):
//!
//! - **Hart payload mode (`--elf <payload> --hart`)**: a bare-metal RV64
//!   payload (e.g. the riscv-tests-style gate binaries) on the privileged
//!   architecture; completion via `--tohost` or the trap engine.
//! - **Machine mode (`--machine`)**: boot with the QEMU-virt device model
//!   (UART/ACLINT/PLIC/syscon/virtio). Optionally boots OpenSBI + a Linux
//!   Image + initrd + generated DTB — real Linux, real symbols.
//!
//! Usage:
//!   vane-cli --elf <payload.elf> [--hart] [--tohost 0xADDR]
//!   vane-cli --machine --opensbi fw_jump.bin --kernel Image [--initrd INITRD]
//!            [--append "console=ttyS0 ..."] [--ram-size 128MiB]

mod elf;

use vane::Reactor;
use vane_host::HostIo;

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
    /// virtio-blk disk image (raw 512-byte-sector file) attached as a
    /// virtio-mmio device (V2). Requires --machine.
    disk: Option<String>,
    /// Host directory to share with the guest via virtio-9p (V3), mounted in
    /// the guest with `mount -t 9p -o trans=virtio hostshare <mountpoint>`.
    share: Option<String>,
    /// Attach a virtio-net NIC whose TCP is tunneled to host sockets
    /// (PLAN_PI_IN_VM M2). Guest addressing: 10.0.2.15/24, gateway 10.0.2.2,
    /// DNS 10.0.2.3 (host-resolved, answers with synthetic 10.0.2.x).
    net: bool,
    /// Run the tunnel's clock/flow servicing on a background thread
    /// (std::thread; a WASIX thread on wasm32-wasmer-wasi) — PLAN_PI_IN_VM M3.
    net_pump: bool,
    /// Static name mappings for the tunnel: `IP=hostname` (repeatable), for
    /// guests that hardcode addresses or to pin well-known services.
    net_map: Vec<String>,
    /// Restore machine state from a VSNP snapshot file instead of booting
    /// images (PLAN_PI_IN_VM M6). Host devices are re-bound from the usual
    /// flags (--disk/--share/--net), so pass the same ones as at save time.
    snapshot_restore: Option<String>,
    /// Save a VSNP snapshot to this file when the UART output window
    /// contains --snapshot-marker (once per run).
    snapshot_save: Option<String>,
    /// Marker string watched on the UART console; triggers --snapshot-save.
    snapshot_marker: Option<String>,
    /// Exit immediately after the marker-triggered snapshot is saved.
    exit_after_snapshot: bool,
}

const RAM_BASE: u64 = 0x8000_0000;
/// fw_dynamic_info structure address (QEMU convention: a2 points here).
const FW_DYNAMIC_INFO_ADDR: u64 = 0x87E0_0000;
const KERNEL_ADDR: u64 = 0x8020_0000;
const INITRD_ADDR: u64 = 0x8400_0000;
const DTB_ADDR: u64 = 0x87F0_0000;

/// A confined host-directory backend for virtio-9p (V3): all paths are joined
/// beneath `root` (the device's `..`-resistant join prevents escape).
struct DirFs {
    root: std::path::PathBuf,
    handles: std::collections::HashMap<u64, std::fs::File>,
    next_handle: u64,
}

impl DirFs {
    fn new(root: std::path::PathBuf) -> Self {
        DirFs {
            root,
            handles: std::collections::HashMap::new(),
            next_handle: 1,
        }
    }
    fn resolve(&self, path: &str) -> std::path::PathBuf {
        let mut p = self.root.clone();
        for comp in path.split('/') {
            if !comp.is_empty() && comp != "." && comp != ".." {
                p.push(comp);
            }
        }
        p
    }
}

impl vane::vane_riscv::virtio_9p::Host9p for DirFs {
    fn stat(&mut self, path: &str) -> Result<vane::vane_riscv::virtio_9p::Meta9p, i32> {
        let p = self.resolve(path);
        match std::fs::metadata(&p) {
            Ok(m) => Ok(vane::vane_riscv::virtio_9p::Meta9p {
                mode: 0o644,
                size: m.len(),
                is_dir: m.is_dir(),
            }),
            Err(_) => Err(2), // ENOENT
        }
    }
    fn open(&mut self, path: &str, flags: u32) -> Result<u64, i32> {
        let p = self.resolve(path);
        let mut o = std::fs::OpenOptions::new();
        // Linux flags: O_WRONLY=1, O_RDWR=2, O_CREAT=0x40, O_TRUNC=0x200.
        let acc = flags & 3;
        o.read(acc == 0 || acc == 2).write(acc == 1 || acc == 2);
        if flags & 0x40 != 0 {
            o.create(true);
        }
        if flags & 0x200 != 0 {
            o.truncate(true);
        }
        match o.open(&p) {
            Ok(f) => {
                let h = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(h, f);
                Ok(h)
            }
            Err(_) => Err(2),
        }
    }
    fn read(&mut self, handle: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        use std::io::{Read, Seek, SeekFrom};
        let f = self.handles.get_mut(&handle).ok_or(9)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| 5)?;
        f.read(buf).map_err(|_| 5)
    }
    fn write(&mut self, handle: u64, offset: u64, data: &[u8]) -> Result<usize, i32> {
        use std::io::{Seek, SeekFrom, Write};
        let f = self.handles.get_mut(&handle).ok_or(9)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| 5)?;
        f.write(data).map_err(|_| 5)
    }
    fn mkdir(&mut self, path: &str, _mode: u32) -> Result<(), i32> {
        std::fs::create_dir(self.resolve(path)).map_err(|_| 17) // EEXIST on failure
    }
    fn readdir(&mut self, path: &str) -> Result<Vec<(String, bool)>, i32> {
        let mut out = Vec::new();
        match std::fs::read_dir(self.resolve(path)) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    out.push((e.file_name().to_string_lossy().into_owned(), is_dir));
                }
                Ok(out)
            }
            Err(_) => Err(2),
        }
    }
    fn close(&mut self, handle: u64) {
        self.handles.remove(&handle);
    }
}

/// A host-file block backend for virtio-blk (V2): a raw 512-byte-sector image.
struct FileBlock {
    file: std::fs::File,
    capacity_sectors: u64,
}

impl FileBlock {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let len = file.metadata()?.len();
        Ok(FileBlock {
            file,
            capacity_sectors: len / vane::vane_riscv::virtio_blk::SECTOR,
        })
    }
}

impl vane::vane_riscv::virtio_blk::BlockBackend for FileBlock {
    fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) {
        use std::io::{Read, Seek, SeekFrom};
        let _ = self
            .file
            .seek(SeekFrom::Start(lba * vane::vane_riscv::virtio_blk::SECTOR));
        let _ = self.file.read_exact(buf);
    }
    fn write_sectors(&mut self, lba: u64, buf: &[u8]) {
        use std::io::{Seek, SeekFrom, Write};
        let _ = self
            .file
            .seek(SeekFrom::Start(lba * vane::vane_riscv::virtio_blk::SECTOR));
        let _ = self.file.write_all(buf);
    }
    fn flush(&mut self) {
        use std::io::Write;
        let _ = self.file.flush();
    }
}

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
    let mut disk = None;
    let mut share = None;
    let mut net = false;
    let mut net_pump = false;
    let mut net_map: Vec<String> = Vec::new();
    let mut snapshot_restore = None;
    let mut snapshot_save = None;
    let mut snapshot_marker = None;
    let mut exit_after_snapshot = false;
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
            "--disk" => disk = Some(it.next().ok_or("--disk needs a value")?),
            "--share" => share = Some(it.next().ok_or("--share needs a value")?),
            "--net" => net = true,
            "--net-pump" => {
                net = true;
                net_pump = true;
            }
            "--snapshot-restore" => {
                snapshot_restore = Some(it.next().ok_or("--snapshot-restore needs a value")?)
            }
            "--snapshot-save" => {
                snapshot_save = Some(it.next().ok_or("--snapshot-save needs a value")?)
            }
            "--snapshot-marker" => {
                snapshot_marker = Some(it.next().ok_or("--snapshot-marker needs a value")?)
            }
            "--exit-after-snapshot" => exit_after_snapshot = true,
            "--net-map" => net_map.push(it.next().ok_or("--net-map needs a value")?),
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
    if snapshot_save.is_some() && snapshot_marker.is_none() {
        return Err("--snapshot-save requires --snapshot-marker".into());
    }
    if exit_after_snapshot && snapshot_save.is_none() {
        return Err("--exit-after-snapshot requires --snapshot-save".into());
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
        disk,
        share,
        net,
        net_pump,
        net_map,
        snapshot_restore,
        snapshot_save,
        snapshot_marker,
        exit_after_snapshot,
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

        // V2: virtio-blk disk. Registered before the DTB is generated so its
        // virtio,mmio node is emitted (PLIC source 1).
        let mut virtio_dev_ids: Vec<u32> = Vec::new();
        if let Some(disk_path) = &args.disk {
            let backend = FileBlock::open(std::path::Path::new(disk_path)).unwrap_or_else(|e| {
                eprintln!("--disk {disk_path}: {e}");
                std::process::exit(2);
            });
            let dev = vane::vane_riscv::virtio_blk::VirtioBlk::new(backend);
            reactor.add_virtio_device(Box::new(dev));
            virtio_dev_ids.push(vane::vane_riscv::virtio::DEV_BLK);
            eprintln!("virtio-blk: {disk_path} attached");
        }
        if let Some(share_path) = &args.share {
            let backend = DirFs::new(std::path::PathBuf::from(share_path));
            let dev = vane::vane_riscv::virtio_9p::Virtio9p::new(backend, "hostshare");
            reactor.add_virtio_device(Box::new(dev));
            virtio_dev_ids.push(vane::vane_riscv::virtio::DEV_9P);
            eprintln!("virtio-9p: {share_path} shared as 'hostshare'");
        }
        if args.net {
            let mut tunnel = vane_net::Tunnel::new(
                vane_net::StdSockets::new(),
                [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                // Boot-time seed; TCP sequence diversity only, not security.
                0x76616e65,
            );
            for m in &args.net_map {
                let Some((ip, host)) = m.split_once('=') else {
                    eprintln!("--net-map {m}: expected IP=hostname");
                    std::process::exit(2);
                };
                let ip: std::net::Ipv4Addr = match ip.parse() {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("--net-map {m}: bad address: {e}");
                        std::process::exit(2);
                    }
                };
                tunnel.map_static(
                    vane_net::Ipv4Address::from_octets(ip.octets()),
                    host.to_string(),
                );
                eprintln!("virtio-net: {ip} -> {host}");
            }
            let backend: Box<dyn vane::vane_riscv::virtio_net::NetBackend> = if args.net_pump {
                eprintln!("virtio-net: pump thread at 5ms intervals");
                Box::new(vane_net::PumpedTunnel::new(
                    tunnel,
                    std::time::Duration::from_millis(5),
                ))
            } else {
                Box::new(tunnel)
            };
            let dev = vane::vane_riscv::virtio_net::VirtioNet::new(
                [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                backend,
            );
            reactor.add_virtio_device(Box::new(dev));
            virtio_dev_ids.push(vane::vane_riscv::virtio::DEV_NET);
            eprintln!("virtio-net: tunnel up (guest 10.0.2.15/24 gw 10.0.2.2 dns 10.0.2.3)");
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

        // Snapshot restore (M6): skip image loading/DTB/boot regs entirely;
        // guest RAM + hart + device state come from the VSNP file. Host
        // devices above (disk/9p/net) were already re-bound from flags.
        let restored_pc = if let Some(path) = &args.snapshot_restore {
            let data = std::fs::read(path).unwrap_or_else(|e| {
                eprintln!("vane-cli: cannot read snapshot {path}: {e}");
                std::process::exit(2);
            });
            let snap = vane::Reactor::snapshot_from_bytes(&data).unwrap_or_else(|e| {
                eprintln!("vane-cli: bad snapshot {path}: {e}");
                std::process::exit(2);
            });
            let pc = snap.pc;
            let id = snap.id;
            reactor.restore(&snap).unwrap_or_else(|e| {
                eprintln!("vane-cli: snapshot restore failed: {e}");
                std::process::exit(2);
            });
            eprintln!("vane-cli: restored snapshot {path} (id {id:016x}, pc {pc:#x})");
            Some(pc)
        } else {
            None
        };
        if restored_pc.is_none() {
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
            let dtb = vane::vane_riscv::machine::virt_dtb_with_virtio(
                args.ram_size,
                initrd_range,
                bootargs,
                &virtio_dev_ids,
            );
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
            entry = restored_pc.expect("checked above");
        }
    } else {
        // Payload mode: a single ELF, ALWAYS under the privileged
        // architecture (the user-mode syscall-shim micro-OS was removed;
        // payloads complete via --tohost or the trap engine).
        if !args.hart {
            eprintln!(
                "vane-cli: payload mode requires --hart (the user-mode syscall shim was removed; use --machine for Linux)"
            );
            std::process::exit(2);
        }
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
        reactor.set_hart_enabled(true);
    }

    // Boot register state for machine mode: a0 = hartid, a1 = dtb.
    let mut regs = [0u64; 32];
    if args.machine && args.snapshot_restore.is_none() {
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
                        Ok(0) => break, // EOF
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
        // Snapshot trigger state (M6): a rolling window over UART output in
        // which --snapshot-marker is searched.
        let mut uart_window: Vec<u8> = Vec::new();
        let mut snapshot_taken = false;
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
                let (privm, mcause, mip, mepc, mtval, stvec) =
                    reactor.with_hart(|h| (h.priv_mode, h.mcause, h.mip, h.mepc, h.mtval, h.stvec));
                let (mtime, mtimecmp) = reactor
                    .with_machine(|m| (m.aclint.mtime(), m.aclint.mtimecmp))
                    .unwrap_or((0, 0));
                eprintln!(
                    "[{total}M] pc={pc:#x} priv={privm:?} mcause={mcause:#x} mepc={mepc:#x} mtval={mtval:#x} stvec={stvec:#x} mip={mip:#x} mtime={mtime} mtimecmp={mtimecmp:#x}"
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
                // machine_io_tick processes virtio queues (needs guest-memory
                // access) in addition to feeding UART RX and folding IRQs.
                let power = reactor.machine_io_tick(&rx_bytes);
                if let Some(tx) = reactor.with_machine(|m| m.uart.drain_tx()) {
                    for &b in &tx {
                        host.uart_write(b);
                    }
                    if let (Some(marker), Some(save_path)) =
                        (&args.snapshot_marker, &args.snapshot_save)
                        && !snapshot_taken
                    {
                        uart_window.extend_from_slice(&tx);
                        // Keep the window bounded; markers are short.
                        if uart_window.len() > 65_536 {
                            let keep = uart_window.len() - 65_536;
                            uart_window.drain(..keep);
                        }
                        if uart_window
                            .windows(marker.len())
                            .any(|w| w == marker.as_bytes())
                        {
                            let snap = reactor.snapshot(None, pc);
                            let bytes = vane::Reactor::snapshot_to_bytes(&snap);
                            std::fs::write(save_path, bytes).unwrap_or_else(|e| {
                                eprintln!("vane-cli: cannot write snapshot {save_path}: {e}");
                                std::process::exit(2);
                            });
                            eprintln!(
                                "vane-cli: snapshot saved to {save_path} (id {:016x}, pc {pc:#x})",
                                snap.id
                            );
                            snapshot_taken = true;
                            if args.exit_after_snapshot {
                                std::process::exit(0);
                            }
                        }
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
}

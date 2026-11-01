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
    /// Attach a virtio-net NIC backed by the built-in ARP/ICMP responder
    /// (`EchoNet`): the guest (10.0.0.2) can ARP for and ping the host side
    /// (10.0.0.1) through the full guest network stack — a deterministic
    /// loopback for e2e tests, not a real network.
    net: bool,
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

/// Per-message 9P logger wrapping the device (VANE_9P_DEBUG): delegates
/// everything, prints one line per completed request. The response type is
/// req+1 (Rlerror = 7 on errors).
struct Trace9p<B: vane::vane_riscv::virtio_9p::Host9p> {
    inner: vane::vane_riscv::virtio_9p::Virtio9p<B>,
    n: u64,
}

fn p9_name(t: u8) -> &'static str {
    use vane::vane_riscv::virtio_9p::*;
    match t {
        R_LERROR => "Rlerror",
        T_STATFS => "Tstatfs",
        T_LOPEN => "Tlopen",
        T_LCREATE => "Tlcreate",
        T_GETATTR => "Tgetattr",
        T_READDIR => "Treaddir",
        T_MKDIR => "Tmkdir",
        T_VERSION => "Tversion",
        T_ATTACH => "Tattach",
        T_FLUSH => "Tflush",
        T_WALK => "Twalk",
        T_READ => "Tread",
        T_WRITE => "Twrite",
        T_CLUNK => "Tclunk",
        _ => "T?",
    }
}

impl<B: vane::vane_riscv::virtio_9p::Host9p> vane::vane_riscv::virtio::VirtioDevice for Trace9p<B> {
    fn device_id(&self) -> u32 {
        self.inner.device_id()
    }
    fn num_queues(&self) -> usize {
        self.inner.num_queues()
    }
    fn features(&self) -> u32 {
        self.inner.features()
    }
    fn read_config(&self, offset: u64, buf: &mut [u8]) {
        self.inner.read_config(offset, buf)
    }
    fn process(
        &mut self,
        queue: usize,
        chain: &vane::vane_riscv::virtio::DescriptorChain,
        mem: &mut dyn vane::vane_riscv::virtio::QueueMem,
    ) -> u32 {
        self.n += 1;
        // The request is size(u32) + type(u8) + tag(u16): peek the type for
        // the log line (read_request is a non-destructive read).
        let req = chain.read_request(mem);
        let ty = req.get(4).copied().unwrap_or(0);
        let written = self.inner.process(queue, chain, mem);
        eprintln!(
            "9p#{}: {}({}) -> {} resp {}B",
            self.n,
            p9_name(ty),
            ty,
            if ty + 1 == 7 { "Rlerror" } else { p9_name(ty + 1) },
            written
        );
        written
    }
}

impl vane::vane_riscv::virtio_9p::Host9p for DirFs {
    fn stat(&mut self, path: &str) -> Result<vane::vane_riscv::virtio_9p::Meta9p, i32> {
        let p = self.resolve(path);
        let dbg = std::env::var_os("VANE_9P_DEBUG").is_some();
        match std::fs::metadata(&p) {
            Ok(m) => {
                if dbg {
                    eprintln!("9p: stat {p:?} -> ok ({} bytes)", m.len());
                }
                Ok(vane::vane_riscv::virtio_9p::Meta9p {
                    mode: 0o644,
                    size: m.len(),
                    is_dir: m.is_dir(),
                })
            }
            Err(_) => {
                if dbg {
                    eprintln!("9p: stat {p:?} -> ENOENT");
                }
                Err(2) // ENOENT
            }
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

/// A deterministic host-side "network simulator" for e2e tests: answers ARP
/// requests for 10.0.0.1 and ICMP echo requests (ping) addressed to it. No
/// real network — the guest exercises its full stack (driver → ARP → ICMP)
/// against a fixed peer.
struct EchoNet {
    /// Frames awaiting delivery to the guest.
    backlog: std::collections::VecDeque<Vec<u8>>,
}

impl EchoNet {
    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x01];
    /// The host-side IP the guest ARPs for / pings.
    const HOST_IP: [u8; 4] = [10, 0, 0, 1];

    fn new() -> Self {
        EchoNet {
            backlog: std::collections::VecDeque::new(),
        }
    }

    /// Reply to an ARP request for HOST_IP.
    fn reply_arp(&mut self, req: &[u8], requester_mac: &[u8], requester_ip: [u8; 4]) {
        if req.len() < 28 {
            return;
        }
        let mut r = vec![0u8; 42]; // eth(14) + arp(28)
        r[0..6].copy_from_slice(requester_mac);
        r[6..12].copy_from_slice(&Self::MAC);
        r[12..14].copy_from_slice(&0x0806u16.to_be_bytes());
        r[14..16].copy_from_slice(&req[0..2]); // htype
        r[16..18].copy_from_slice(&req[2..4]); // pro
        r[18] = req[4]; // hln
        r[19] = req[5]; // pln
        r[20..22].copy_from_slice(&2u16.to_be_bytes()); // oper = reply
        r[22..28].copy_from_slice(&Self::MAC); // sha
        r[28..32].copy_from_slice(&Self::HOST_IP); // spa
        r[32..38].copy_from_slice(requester_mac); // tha
        r[38..42].copy_from_slice(&requester_ip); // tpa
        self.backlog.push_back(r);
    }

    /// Reply to an ICMP echo request. `ip` starts at the IPv4 header (i.e.
    /// `frame[14..]`); `icmp` at `ip[ihl..]`. All indexing is bounds-checked —
    /// this parses GUEST-supplied frames and must never panic (an earlier
    /// draft double-offset the IP fields and crashed vane-cli on the first
    /// guest frame).
    fn reply_icmp_echo(&mut self, eth: &[u8], ip: &[u8], icmp: &[u8]) {
        let ihl = (ip[0] & 0x0f) as usize * 4;
        if icmp.len() < 8 || icmp[0] != 8 || ip.len() < ihl + 8 {
            return; // only well-formed echo requests
        }
        let mut r = vec![0u8; 14 + ihl + icmp.len()];
        // Ethernet: to the guest's MAC, from ours.
        r[0..6].copy_from_slice(&eth[6..12]);
        r[6..12].copy_from_slice(&Self::MAC);
        r[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        // IPv4 header: swap src/dst, recompute checksum. (src @ 12..16,
        // dst @ 16..20 within the header — NOT frame offsets.)
        r[14..14 + ihl].copy_from_slice(&ip[..ihl]);
        r[14 + 12..14 + 16].copy_from_slice(&ip[16..20]); // src (was dst)
        r[14 + 16..14 + 20].copy_from_slice(&ip[12..16]); // dst (was src)
        r[14 + 10..14 + 12].copy_from_slice(&[0, 0]);
        let sum = internet_checksum(&r[14..14 + ihl]);
        r[14 + 10..14 + 12].copy_from_slice(&sum.to_be_bytes());
        // ICMP: type 0 (echo reply), recompute checksum.
        r[14 + ihl..].copy_from_slice(icmp);
        r[14 + ihl] = 0;
        r[14 + ihl + 2..14 + ihl + 4].copy_from_slice(&[0, 0]);
        let sum = internet_checksum(&r[14 + ihl..]);
        r[14 + ihl + 2..14 + ihl + 4].copy_from_slice(&sum.to_be_bytes());
        self.backlog.push_back(r);
    }
}

/// The internet checksum (ones-complement sum folded).
fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0u32;
    for pair in bytes.chunks(2) {
        let w = match pair.len() {
            2 => ((pair[0] as u32) << 8) | pair[1] as u32,
            _ => (pair[0] as u32) << 8,
        };
        sum += w;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

impl vane::vane_riscv::virtio_net::NetBackend for EchoNet {
    fn send_frame(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        match ethertype {
            0x0806 => {
                // ARP: payload starts at 14. Reply to requests for HOST_IP.
                let arp = &frame[14..];
                if arp.len() >= 28
                    && u16::from_be_bytes([arp[6], arp[7]]) == 1
                    && arp[24..28] == Self::HOST_IP
                {
                    // spa @ 14..18 — the REQUESTER's IP (arp is exactly 28
                    // bytes; [28..32] panicked on the first guest ARP).
                    let ip = [arp[14], arp[15], arp[16], arp[17]];
                    self.reply_arp(arp, &frame[6..12], ip);
                }
            }
            0x0800 => {
                let ip = &frame[14..];
                if ip.len() >= 20 && (ip[0] >> 4) == 4 && ip[9] == 1 {
                    let ihl = (ip[0] & 0x0f) as usize * 4;
                    if ip.len() >= ihl {
                        self.reply_icmp_echo(frame, ip, &ip[ihl..]);
                    }
                }
            }
            _ => {}
        }
    }
    fn poll_recv(&mut self) -> Option<Vec<u8>> {
        self.backlog.pop_front()
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
        disk,
        share,
        net,
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
            if std::env::var_os("VANE_9P_DEBUG").is_some() {
                // Live per-message trace of the 9P exchange (the mount-flake
                // diagnostic): every request that REACHED the device and was
                // completed. A retried mount printing nothing = the transport
                // never delivered (queue/IRQ wedge on the device side).
                let inner = Trace9p { inner: dev, n: 0 };
                reactor.add_virtio_device(Box::new(inner));
            } else {
                reactor.add_virtio_device(Box::new(dev));
            }
            virtio_dev_ids.push(vane::vane_riscv::virtio::DEV_9P);
            eprintln!("virtio-9p: {share_path} shared as 'hostshare'");
        }
        if args.net {
            let dev = vane::vane_riscv::virtio_net::VirtioNet::new(
                [0x52, 0x54, 0x00, 0x12, 0x34, 0x01],
                Box::new(EchoNet::new()),
            );
            reactor.add_virtio_device(Box::new(dev));
            virtio_dev_ids.push(vane::vane_riscv::virtio::DEV_NET);
            eprintln!("virtio-net: echo NIC attached (guest 10.0.0.2, host peer 10.0.0.1)");
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
        // Watchdog: dump the trace-dispatch prof accumulators every 5s so a
        // long-running quantum reveals which segment is accumulating time
        // (the per-quantum heartbeat only prints at quantum end).
        if std::env::var_os("VANE_HEARTBEAT").is_some() {
            std::thread::spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    let (h, m, c) = vane::trace_dispatch_counters();
                    let (lk, irq, hk, it, cc) = vane::vane_riscv::prof_counters();
                    let (fb0, fb1, fb2) = vane::step_fallback_counters();
                    eprintln!(
                        "  ..wd td=({h},{m},{c}) prof=(lk={lk} irq={irq} hk={hk} it={it} cc={cc}) fb=({fb0},{fb1},{fb2})"
                    );
                }
            });
        }
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
                let (privm, mcause, mip, mepc, _mtval, _stvec, mie, mstatus) =
                    reactor.with_hart(|h| {
                        (
                            h.priv_mode,
                            h.mcause,
                            h.mip,
                            h.mepc,
                            h.mtval,
                            h.stvec,
                            h.mie,
                            h.mstatus,
                        )
                    });
                let (mtime, mtimecmp, msip) = reactor
                    .with_machine(|m| (m.aclint.mtime(), m.aclint.mtimecmp, m.aclint.msip))
                    .unwrap_or((0, 0, false));
                let (ra, sp, a0, a1) = (
                    reactor.read_reg(1),
                    reactor.read_reg(2),
                    reactor.read_reg(10),
                    reactor.read_reg(11),
                );
                let (a6, a7) = (reactor.read_reg(16), reactor.read_reg(17));
                let (il_pc, il_raw, il_n) = (
                    vane::vane_riscv::hart::LAST_ILLEGAL_PC
                        .load(std::sync::atomic::Ordering::Relaxed),
                    vane::vane_riscv::hart::LAST_ILLEGAL_RAW
                        .load(std::sync::atomic::Ordering::Relaxed),
                    vane::vane_riscv::hart::LAST_ILLEGAL_COUNT
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
                let (fb_bail, fb_needs, fb_decode) = vane::step_fallback_counters();
                let (t_hit, t_miss, t_compile) = vane::trace_dispatch_counters();
                let (p_lock, p_irq, p_hook, p_interp, p_compile) =
                    vane::vane_riscv::prof_counters();
                eprintln!(
                    "[{total}M] pc={pc:#x} priv={privm:?} mcause={mcause:#x} mepc={mepc:#x} mip={mip:#x} mie={mie:#x} mstatus={mstatus:#x} msip={msip} mtime={mtime} mtimecmp={mtimecmp:#x} ra={ra:#x} sp={sp:#x} a0={a0:#x} a1={a1:#x} a6={a6:#x} a7={a7:#x} fb=({fb_bail},{fb_needs},{fb_decode}) td=({t_hit},{t_miss},{t_compile}) il=({il_pc:#x},{il_raw:#x},{il_n}) prof=(lk={p_lock} irq={p_irq} hk={p_hook} it={p_interp} cc={p_compile})"
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
                let _hb2 = std::env::var_os("VANE_HEARTBEAT").is_some();
                if _hb2 {
                    eprintln!("  ..io_tick begin");
                }
                let power = reactor.machine_io_tick(&rx_bytes);
                if _hb2 {
                    eprintln!("  ..io_tick end");
                }
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
}

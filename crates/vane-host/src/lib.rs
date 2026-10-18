//! `vane-host` — host-side IO traits for driving a vane emulator instance
//! outside a JS engine.
//!
//! This crate is the seam defined by `PLAN_LINUX_WASM.md` §5.2: every host
//! dependency (console, clocks, yielding, later block devices and swap) enters
//! the emulator core through traits defined here, so `browser`, `WASI`,
//! `WASIX`, and `native` are just different assemblies of the same core.
//!
//! The crate is `no_std` + `alloc` so it can be used from every target vane
//! compiles to, including `wasm32-unknown-unknown` and bare test harnesses.
//!
//! ## Roadmap alignment
//!
//! - **M0 (this milestone)**: [`HostIo`] carries the console and clock
//!   surface needed by `vane-cli`'s syscall shim.
//! - **M4**: device models (ns16550a, ACLINT, PLIC, virtio-mmio) consume
//!   [`HostIo::uart_write`]/[`HostIo::uart_read`]/[`HostIo::monotonic_nanos`].
//! - **M5**: an async `SwapStore` trait for encrypted paging lands here (see
//!   `PLAN_LINUX_WASM.md` §6).

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// Host capabilities the emulator core may rely on while executing guest
/// code.
///
/// All methods have minimal default implementations so a trivial host (e.g.
/// a unit test) can implement only what it needs. The defaults are
/// deliberately fail-quiet rather than panicking: a guest reading a clock on
/// a host with no clock sees a frozen zero, matching a powered-off RTC.
pub trait HostIo {
    /// Write one byte to the console (UART TX stand-in until the ns16550a
    /// device model lands in M4).
    fn uart_write(&mut self, byte: u8);

    /// Read one byte of pending console input, or `None` if the RX FIFO is
    /// empty. Default: no input ever arrives.
    fn uart_read(&mut self) -> Option<u8> {
        None
    }

    /// Monotonic clock in nanoseconds. Will drive the ACLINT `mtime`
    /// counter (M4); must never go backwards within a run. Default: frozen
    /// at zero.
    fn monotonic_nanos(&mut self) -> u64 {
        0
    }

    /// Wall clock as seconds since the UNIX epoch. Used for the RTC and for
    /// entropy seeding (virtio-rng). Default: zero (host has no RTC).
    fn wall_clock_secs(&mut self) -> u64 {
        0
    }

    /// Yield to the host scheduler / event loop.
    ///
    /// The interpreter loop calls this periodically so that single-threaded
    /// hosts (WASI runtimes, browser workers) can service timers and IO. The
    /// default returns a ready future, i.e. cooperative multitasking hosts
    /// may ignore it.
    fn yield_now(&mut self) -> impl core::future::Future<Output = ()> {
        core::future::ready(())
    }
}

/// A host socket handle handed out by [`SocketBackend::connect`].
/// Opaque to the emulator; backends may recycle values after `close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SockFd(pub u32);

/// Errors reported by [`SocketBackend`]. `Other` carries a host errno-style
/// code where one exists (0 = unspecified).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockErr {
    /// Non-blocking operation would block; retry later.
    WouldBlock,
    /// Orderly shutdown by the peer (recv side).
    Eof,
    /// Connection refused by the remote end.
    Refused,
    /// Connection reset by the peer.
    Reset,
    /// Name resolution failed.
    ResolveFailed,
    /// The backend does not implement this operation.
    Unsupported,
    /// Anything else.
    Other(i32),
}

/// Host-side TCP streams and DNS for the guest network tunnel
/// (PLAN_PI_IN_VM §4.2). One guest TCP flow maps to one `connect`ed host
/// socket; the guest never sees real host addresses — name resolution is
/// host-side, so the tunnel can hand out synthetic IPs.
///
/// All operations are non-blocking from the caller's perspective:
/// `recv`/`send` report `Ok(0)` when no progress can be made right now
/// (equivalent to `WouldBlock` kept out of the error path for ergonomics).
/// `connect` may block briefly (host connect timeout); backends should keep
/// that bounded.
///
/// Bindings (PLAN_PI_IN_VM M2–M4): native `std::net` (also works on
/// `wasm32-wasmer-wasi`, whose std sockets lower to WASIX `sock_*`), and a
/// raw binding for the unofficial preview1 socket extension.
pub trait SocketBackend {
    /// Resolve a hostname to IPv4 addresses (A records). The tunnel uses
    /// this to validate names before handing out synthetic IPs; the actual
    /// connection re-resolves by name in `connect`.
    fn resolve(&mut self, name: &str) -> Result<Vec<[u8; 4]>, SockErr>;

    /// Open a host TCP connection to `host:port` (hostname or dotted IPv4).
    fn connect(&mut self, host: &str, port: u16) -> Result<SockFd, SockErr>;

    /// Send bytes on a connected socket. Returns the number of bytes
    /// accepted; `Ok(0)` means the send buffer is full — retry later.
    fn send(&mut self, fd: SockFd, buf: &[u8]) -> Result<usize, SockErr>;

    /// Receive bytes into `buf`. `Ok(0)` means no data available right now;
    /// `Err(SockErr::Eof)` means the peer performed an orderly shutdown.
    fn recv(&mut self, fd: SockFd, buf: &mut [u8]) -> Result<usize, SockErr>;

    /// Half-close the send side (guest finished sending but still reads).
    fn shutdown(&mut self, _fd: SockFd) {}

    /// Close and recycle a socket. Must be idempotent.
    fn close(&mut self, fd: SockFd);
}

/// A `SocketBackend` that refuses everything — the default when no
/// networking is configured, so a missing backend fails loudly at connect
/// time rather than silently black-holing guest traffic.
pub struct NoSockets;

impl SocketBackend for NoSockets {
    fn resolve(&mut self, _name: &str) -> Result<Vec<[u8; 4]>, SockErr> {
        Err(SockErr::Unsupported)
    }
    fn connect(&mut self, _host: &str, _port: u16) -> Result<SockFd, SockErr> {
        Err(SockErr::Unsupported)
    }
    fn send(&mut self, _fd: SockFd, _buf: &[u8]) -> Result<usize, SockErr> {
        Err(SockErr::Unsupported)
    }
    fn recv(&mut self, _fd: SockFd, _buf: &mut [u8]) -> Result<usize, SockErr> {
        Err(SockErr::Unsupported)
    }
    fn close(&mut self, _fd: SockFd) {}
}

/// Why the guest asked the host to stop, reported via
/// [`HostIo::poweroff`]-style flows (M4) — placeholder documentation anchor
/// for now; power management arrives with the syscon device.
///
/// This type already exists so CLI/host shells can share one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// Guest requested poweroff (SBI SRST `shutdown`).
    PowerOff,
    /// Guest requested reboot (SBI SRST `cold`/`warm` reboot).
    Reboot,
}

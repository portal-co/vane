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

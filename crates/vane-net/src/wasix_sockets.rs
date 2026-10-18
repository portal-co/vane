//! Raw WASIX / unofficial-socket-extension [`SocketBackend`] (PLAN_PI_IN_VM
//! M4, Binding B).
//!
//! This binds the `wasix_32v1` import module directly (the de-facto
//! "unofficial WASI socket extension": `sock_open`/`sock_connect`/
//! `sock_send`/`sock_recv`/`sock_shutdown`/`resolve` — the set exported by
//! wasmer's WASIX and by runtimes carrying the pre-preview2 socket
//! patches). It exists for targets where Rust std has no socket support
//! (plain `wasm32-wasip1` + extension); on `wasm32-wasmer-wasi`,
//! [`crate::StdSockets`] already lowers to these same syscalls through std.
//!
//! ABI notes (verified against wasix 0.12.5 `lib_generated32.rs` and
//! wasmer's `net::read_ip_port`):
//! - `AddrPort` memory layout for IPv4: `tag: u8 (=1)`, pad, then
//!   `port: u16 native-endian` followed by 4 address octets. wasmer reads
//!   the port with `from_ne_bytes` — wasm32 is little-endian, so a plain
//!   `#[repr(C)]` `u16` is correct. (wasmer's own *write* path uses
//!   `to_be_bytes`, an upstream inconsistency that does not affect the
//!   guest→host direction we use.)
//! - `resolve(host, port, out, max) -> count` writes `Addr` entries
//!   (`tag u8, pad, union{inet4: [u8;4], inet6, unix}` — 20 bytes stride).
//! - Sockets are set non-blocking via `fd_fdstat_set_flags(fd,
//!   FDFLAGS_NONBLOCK=4)`.
//!
//! NOT YET VALIDATED against a live runtime — first wasmer run may require
//! adjustment (tracked as R3 in PLAN_PI_IN_VM). Compile-checked for
//! `wasm32-wasip1`; the module is inert unless `WasixSockets` is
//! constructed (the imports are then required at instantiation).

use alloc::vec::Vec;
use core::mem::MaybeUninit;

use vane_host::{SockErr, SockFd, SocketBackend};

const ERRNO_SUCCESS: i32 = 0;
const ERRNO_AGAIN: i32 = 6;
const ERRNO_BADF: i32 = 8;
const ERRNO_CONNREFUSED: i32 = 14;
const ERRNO_CONNRESET: i32 = 15;
const ERRNO_INPROGRESS: i32 = 26;
const ERRNO_ISCONN: i32 = 30;
const ERRNO_TIMEDOUT: i32 = 73;

const ADDRESS_FAMILY_INET4: i32 = 1;
// NOTE: wasmer's Socktype is Unknown=0, Stream=1, Dgram=2 (verified against
// wasmer-wasix-types 0.705 / wasmer 7.5). The wasix *crate* 0.12.5 claims
// Dgram=0/Stream=1 — stale; trust the runtime.
const SOCK_TYPE_STREAM: i32 = 1;
const SOCK_TYPE_DGRAM: i32 = 2;
const SOCK_PROTO_IP: i32 = 0;
const SDFLAGS_WR: i32 = 2;
const FDFLAGS_NONBLOCK: i32 = 4;
const SOCK_STATUS_OPENED: u8 = 1;

/// Connect budget in milliseconds (poll_oneoff-slept, not spun).
const CONNECT_TIMEOUT_MS: u64 = 30_000;
const CONNECT_POLL_STEP_MS: u64 = 10;

/// IPv4 address-with-port, matching wasmer's `__wasi_addr_port_t` memory
/// layout for the inet4 tag.
#[repr(C)]
#[derive(Copy, Clone)]
struct AddrIp4Port {
    port: u16, // native-endian (wasmer reads with from_ne_bytes)
    addr: [u8; 4],
}

#[repr(C)]
#[derive(Copy, Clone)]
union AddrPortU {
    inet4: AddrIp4Port,
    _pad: [u8; 26],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct AddrPort {
    tag: u8,
    _pad: u8,
    u: AddrPortU,
}

/// `Addr` as produced by `resolve` (stride 20, alignment 2).
#[repr(C)]
#[derive(Copy, Clone)]
union AddrU {
    inet4: [u8; 4],
    _pad: [u8; 18],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Addr {
    tag: u8,
    _pad: u8,
    u: AddrU,
}

/// WASI iovec/ciovec on wasm32: (ptr, len).
#[repr(C)]
struct Iovec {
    ptr: u32,
    len: u32,
}

/// WASI preview1 clock subscription for `poll_oneoff` (used as a sleep).
/// Layout: userdata@0, type@8 (0=clock), clock_id@16 (1=monotonic),
/// timeout@24 ns (relative when flags=0), precision@32, flags@40.
#[repr(C)]
struct ClockSubscription {
    userdata: u64,
    tag: u8,
    _pad0: [u8; 7],
    clock_id: u32,
    timeout: u64,
    precision: u64,
    flags: u16,
    _pad1: [u8; 6],
}

/// WASI preview1 event out-record (32 bytes); only userdata/error matter
/// for a clock sleep.
#[repr(C)]
#[derive(Copy, Clone)]
struct Event {
    userdata: u64,
    error: u16,
    ty: u8,
    _pad0: [u8; 5],
    rw_nbytes: u64,
    rw_flags: u16,
    _pad1: [u8; 6],
}

#[allow(non_snake_case)]
mod sys {
    #[link(wasm_import_module = "wasix_32v1")]
    unsafe extern "C" {
        pub fn sock_open(family: i32, ty: i32, proto: i32, ret_fd: i32) -> i32;
        pub fn sock_connect(fd: i32, addr: i32) -> i32;
        pub fn sock_send(fd: i32, data: i32, len: i32, flags: i32, ret: i32) -> i32;
        pub fn sock_recv(
            fd: i32,
            data: i32,
            len: i32,
            flags: i32,
            ret_n: i32,
            ret_flags: i32,
        ) -> i32;
        pub fn sock_shutdown(fd: i32, how: i32) -> i32;
        pub fn sock_status(fd: i32, ret: i32) -> i32;
        pub fn resolve(host: i32, host_len: i32, port: i32, addrs: i32, max: i32, ret: i32) -> i32;
        pub fn fd_close(fd: i32) -> i32;
        pub fn fd_fdstat_set_flags(fd: i32, flags: i32) -> i32;
        pub fn poll_oneoff(subs: i32, events: i32, nsubs: i32, ret: i32) -> i32;
    }
}

fn ptr<T>(p: *const T) -> i32 {
    p as usize as i32
}
fn ptr_mut<T>(p: *mut T) -> i32 {
    p as usize as i32
}

/// Sleep via a WASI clock subscription (works on any runtime with
/// poll_oneoff; no busy-wait).
fn sleep_ms(ms: u64) {
    let sub = ClockSubscription {
        userdata: 0,
        tag: 0, // clock
        _pad0: [0; 7],
        clock_id: 1, // monotonic
        timeout: ms * 1_000_000,
        precision: 1_000_000,
        flags: 0, // relative
        _pad1: [0; 6],
    };
    let mut ev: MaybeUninit<Event> = MaybeUninit::uninit();
    let mut n: MaybeUninit<usize> = MaybeUninit::uninit();
    unsafe {
        sys::poll_oneoff(
            ptr(&sub),
            ptr_mut(ev.as_mut_ptr()),
            1,
            ptr_mut(n.as_mut_ptr()),
        );
    }
}

fn map_errno(e: i32) -> SockErr {
    match e {
        ERRNO_AGAIN | ERRNO_INPROGRESS => SockErr::WouldBlock,
        ERRNO_CONNREFUSED => SockErr::Refused,
        ERRNO_CONNRESET => SockErr::Reset,
        ERRNO_BADF => SockErr::Other(9),
        ERRNO_TIMEDOUT => SockErr::Other(110),
        other => SockErr::Other(other),
    }
}

/// [`SocketBackend`] over raw `wasix_32v1` socket syscalls.
pub struct WasixSockets;

impl WasixSockets {
    pub fn new() -> Self {
        WasixSockets
    }

    /// Raw probe result: the errno from a UDP `sock_open`, for debugging.
    /// `0` means the extension answered; the fd is closed on success.
    pub fn probe_raw() -> i32 {
        let mut fd: MaybeUninit<u32> = MaybeUninit::uninit();
        let rc = unsafe {
            sys::sock_open(
                ADDRESS_FAMILY_INET4,
                SOCK_TYPE_DGRAM,
                SOCK_PROTO_IP,
                ptr_mut(fd.as_mut_ptr()),
            )
        };
        if rc != ERRNO_SUCCESS {
            return rc;
        }
        let fd = unsafe { fd.assume_init() };
        unsafe { sys::fd_close(fd as i32) };
        0
    }

    /// Probe whether the runtime actually provides the extension: a UDP
    /// socket is opened and closed. Cheap; run once at startup before
    /// advertising networking.
    pub fn probe() -> bool {
        Self::probe_raw() == 0
    }

    fn set_nonblocking(fd: u32) {
        // Best-effort: a runtime that rejects this leaves the socket
        // blocking, which the Tunnel tolerates (recv would stall the pump —
        // surfaced by the gates, not hidden).
        unsafe {
            sys::fd_fdstat_set_flags(fd as i32, FDFLAGS_NONBLOCK);
        }
    }
}

impl Default for WasixSockets {
    fn default() -> Self {
        Self::new()
    }
}

impl SocketBackend for WasixSockets {
    fn resolve(&mut self, name: &str) -> Result<Vec<[u8; 4]>, SockErr> {
        let mut addrs: [MaybeUninit<Addr>; 8] = [const { MaybeUninit::uninit() }; 8];
        let mut n: MaybeUninit<usize> = MaybeUninit::uninit();
        let rc = unsafe {
            sys::resolve(
                ptr(name.as_ptr()),
                name.len() as i32,
                0,
                ptr_mut(addrs.as_mut_ptr() as *mut Addr),
                addrs.len() as i32,
                ptr_mut(n.as_mut_ptr()),
            )
        };
        if rc != ERRNO_SUCCESS {
            return Err(SockErr::ResolveFailed);
        }
        let n = unsafe { n.assume_init() }.min(addrs.len());
        let mut out = Vec::new();
        for i in 0..n {
            let a = unsafe { addrs[i].assume_init_ref() };
            if a.tag == ADDRESS_FAMILY_INET4 as u8 {
                out.push(unsafe { a.u.inet4 });
            }
        }
        if out.is_empty() {
            Err(SockErr::ResolveFailed)
        } else {
            Ok(out)
        }
    }

    fn connect(&mut self, host: &str, port: u16) -> Result<SockFd, SockErr> {
        let addrs = self.resolve(host)?;
        let mut fd: MaybeUninit<u32> = MaybeUninit::uninit();
        let rc = unsafe {
            sys::sock_open(
                ADDRESS_FAMILY_INET4,
                SOCK_TYPE_STREAM,
                SOCK_PROTO_IP,
                ptr_mut(fd.as_mut_ptr()),
            )
        };
        if rc != ERRNO_SUCCESS {
            return Err(map_errno(rc));
        }
        let fd = unsafe { fd.assume_init() };
        Self::set_nonblocking(fd);

        let addr = AddrPort {
            tag: ADDRESS_FAMILY_INET4 as u8,
            _pad: 0,
            u: AddrPortU {
                inet4: AddrIp4Port {
                    port, // native-endian; wasmer reads from_ne_bytes
                    addr: addrs[0],
                },
            },
        };
        let rc = unsafe { sys::sock_connect(fd as i32, ptr(&addr)) };
        if rc != ERRNO_SUCCESS && rc != ERRNO_INPROGRESS && rc != ERRNO_ISCONN {
            unsafe { sys::fd_close(fd as i32) };
            return Err(map_errno(rc));
        }
        // Wait for the connection to come up: poll sock_status with a real
        // (poll_oneoff clock) sleep between polls — no busy spin (R9).
        let mut status: MaybeUninit<u8> = MaybeUninit::uninit();
        let mut waited = 0u64;
        loop {
            let rc = unsafe { sys::sock_status(fd as i32, ptr_mut(status.as_mut_ptr())) };
            if rc != ERRNO_SUCCESS {
                unsafe { sys::fd_close(fd as i32) };
                return Err(map_errno(rc));
            }
            if unsafe { status.assume_init() } == SOCK_STATUS_OPENED {
                return Ok(SockFd(fd));
            }
            if waited >= CONNECT_TIMEOUT_MS {
                unsafe { sys::fd_close(fd as i32) };
                return Err(SockErr::Other(110)); // ETIMEDOUT
            }
            sleep_ms(CONNECT_POLL_STEP_MS);
            waited += CONNECT_POLL_STEP_MS;
        }
    }

    fn send(&mut self, fd: SockFd, buf: &[u8]) -> Result<usize, SockErr> {
        let io = Iovec {
            ptr: buf.as_ptr() as usize as u32,
            len: buf.len() as u32,
        };
        let mut n: MaybeUninit<usize> = MaybeUninit::uninit();
        let rc = unsafe { sys::sock_send(fd.0 as i32, ptr(&io), 1, 0, ptr_mut(n.as_mut_ptr())) };
        match rc {
            ERRNO_SUCCESS => Ok(unsafe { n.assume_init() }),
            e => Err(map_errno(e)),
        }
    }

    fn recv(&mut self, fd: SockFd, buf: &mut [u8]) -> Result<usize, SockErr> {
        let io = Iovec {
            ptr: buf.as_mut_ptr() as usize as u32,
            len: buf.len() as u32,
        };
        let mut n: MaybeUninit<usize> = MaybeUninit::uninit();
        let mut roflags: MaybeUninit<u32> = MaybeUninit::uninit();
        let rc = unsafe {
            sys::sock_recv(
                fd.0 as i32,
                ptr(&io),
                1,
                0,
                ptr_mut(n.as_mut_ptr()),
                ptr_mut(roflags.as_mut_ptr()),
            )
        };
        match rc {
            ERRNO_SUCCESS => {
                let n = unsafe { n.assume_init() };
                if n == 0 {
                    Err(SockErr::Eof) // orderly shutdown
                } else {
                    Ok(n)
                }
            }
            e => Err(map_errno(e)),
        }
    }

    fn shutdown(&mut self, fd: SockFd) {
        unsafe {
            sys::sock_shutdown(fd.0 as i32, SDFLAGS_WR);
        }
    }

    fn close(&mut self, fd: SockFd) {
        unsafe {
            sys::fd_close(fd.0 as i32);
        }
    }
}

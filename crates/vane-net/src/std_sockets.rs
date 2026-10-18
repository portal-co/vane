//! `std::net`-based [`SocketBackend`] (PLAN_PI_IN_VM M2, Binding A).
//!
//! Works native (macOS/Linux) and — importantly — on `wasm32-wasmer-wasi`,
//! where std sockets lower to WASIX `sock_*` calls, so this one
//! implementation doubles as the WASIX binding. The raw unofficial-socket-
//! extension binding (M4) is a separate implementation of the same trait.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use std::vec::Vec;

use vane_host::{SockErr, SockFd, SocketBackend};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct StdSockets {
    conns: BTreeMap<u32, TcpStream>,
    next: u32,
}

impl StdSockets {
    pub fn new() -> Self {
        StdSockets {
            conns: BTreeMap::new(),
            next: 1,
        }
    }
}

impl Default for StdSockets {
    fn default() -> Self {
        Self::new()
    }
}

fn map_io(e: std::io::Error) -> SockErr {
    use std::io::ErrorKind::*;
    match e.kind() {
        WouldBlock => SockErr::WouldBlock,
        ConnectionRefused => SockErr::Refused,
        ConnectionReset | ConnectionAborted | BrokenPipe => SockErr::Reset,
        UnexpectedEof => SockErr::Eof,
        _ => SockErr::Other(e.raw_os_error().unwrap_or(0)),
    }
}

impl SocketBackend for StdSockets {
    fn resolve(&mut self, name: &str) -> Result<Vec<[u8; 4]>, SockErr> {
        let addrs = (name, 0)
            .to_socket_addrs()
            .map_err(|_| SockErr::ResolveFailed)?;
        let v4: Vec<[u8; 4]> = addrs
            .filter_map(|a| match a.ip() {
                std::net::IpAddr::V4(v4) => Some(v4.octets()),
                _ => None,
            })
            .collect();
        if v4.is_empty() {
            Err(SockErr::ResolveFailed)
        } else {
            Ok(v4)
        }
    }

    fn connect(&mut self, host: &str, port: u16) -> Result<SockFd, SockErr> {
        let addrs = (host, port)
            .to_socket_addrs()
            .map_err(|_| SockErr::ResolveFailed)?;
        let mut last = SockErr::Refused;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    s.set_nonblocking(true).ok();
                    s.set_nodelay(true).ok();
                    let fd = SockFd(self.next);
                    self.next += 1;
                    self.conns.insert(fd.0, s);
                    return Ok(fd);
                }
                Err(e) => last = map_io(e),
            }
        }
        Err(last)
    }

    fn send(&mut self, fd: SockFd, buf: &[u8]) -> Result<usize, SockErr> {
        let Some(s) = self.conns.get_mut(&fd.0) else {
            return Err(SockErr::Other(9)); // EBADF
        };
        match s.write(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(map_io(e)),
        }
    }

    fn recv(&mut self, fd: SockFd, buf: &mut [u8]) -> Result<usize, SockErr> {
        let Some(s) = self.conns.get_mut(&fd.0) else {
            return Err(SockErr::Other(9));
        };
        match s.read(buf) {
            Ok(0) => Err(SockErr::Eof),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(map_io(e)),
        }
    }

    fn shutdown(&mut self, fd: SockFd) {
        if let Some(s) = self.conns.get(&fd.0) {
            s.shutdown(std::net::Shutdown::Write).ok();
        }
    }

    fn close(&mut self, fd: SockFd) {
        self.conns.remove(&fd.0);
    }
}

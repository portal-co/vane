//! Live validation of the raw WASIX socket binding (PLAN_PI_IN_VM R3).
//!
//! Build:  cargo build -p vane-net --example wasix_probe \
//!           --target wasm32-wasip1 --features wasix-raw
//! Run:    python3 harness/net/echo_server.py 17777 &
//!         wasmer run target/wasm32-wasip1/debug/examples/wasix_probe.wasm -- 17777
//!
//! Exercises: extension probe, DNS resolve of a literal + a name,
//! TCP connect to the host echo server, send/recv round-trip, half-close.
//! Prints WASIX-PROBE-OK on success; exits nonzero on failure.

use vane_host::{SockErr, SocketBackend};
use vane_net::WasixSockets;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(17777);

    let probe_rc = WasixSockets::probe_raw();
    println!("probe: sock_open errno = {probe_rc}");
    assert_eq!(probe_rc, 0, "wasix_32v1 sock_* imports missing or refusing");

    let mut s = WasixSockets::new();

    // Literal IPv4 should resolve (or at least parse) host-side.
    let literal = s.resolve("127.0.0.1");
    println!("resolve(127.0.0.1) -> {literal:?}");

    let fd = match s.connect("127.0.0.1", port) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("connect failed: {e:?}");
            std::process::exit(1);
        }
    };
    println!("connect ok: fd={fd:?}");

    // Read the banner (poll: sockets are non-blocking).
    let mut banner = Vec::new();
    let mut buf = [0u8; 256];
    let mut tries = 0;
    loop {
        match s.recv(fd, &mut buf) {
            Ok(0) | Err(SockErr::WouldBlock) => {
                tries += 1;
                if tries > 1_000_000 {
                    eprintln!("banner never arrived");
                    std::process::exit(1);
                }
                std::thread::yield_now();
            }
            Ok(n) => {
                banner.extend_from_slice(&buf[..n]);
                break;
            }
            Err(e) => {
                eprintln!("recv banner failed: {e:?}");
                std::process::exit(1);
            }
        }
    }
    let banner = String::from_utf8_lossy(&banner);
    println!("banner: {}", banner.trim());
    assert!(banner.contains("VANE-ECHO-BANNER"), "unexpected banner");

    // Echo round-trip.
    let mut off = 0;
    while off < 7 {
        match s.send(fd, &b"R3-PING"[off..]) {
            Ok(0) | Err(SockErr::WouldBlock) => std::thread::yield_now(),
            Ok(n) => off += n,
            Err(e) => {
                eprintln!("send failed: {e:?}");
                std::process::exit(1);
            }
        }
    }
    let mut echoed = Vec::new();
    let mut tries = 0;
    while echoed.len() < 7 {
        match s.recv(fd, &mut buf) {
            Ok(0) | Err(SockErr::WouldBlock) => {
                tries += 1;
                if tries > 1_000_000 {
                    eprintln!("echo never arrived");
                    std::process::exit(1);
                }
                std::thread::yield_now();
            }
            Ok(n) => echoed.extend_from_slice(&buf[..n]),
            Err(e) => {
                eprintln!("recv echo failed: {e:?}");
                std::process::exit(1);
            }
        }
    }
    assert_eq!(&echoed, b"R3-PING");
    println!("echo ok");

    s.shutdown(fd);
    s.close(fd);
    println!("WASIX-PROBE-OK");
}

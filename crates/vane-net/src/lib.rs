//! `vane-net` — the guest TCP tunnel of PLAN_PI_IN_VM §4.
//!
//! A smoltcp userspace TCP/IP stack terminates the guest's TCP/UDP inside
//! the vane host process. Every accepted guest TCP flow is proxied to one
//! host socket opened through [`vane_host::SocketBackend`], so the same
//! tunnel runs native (std sockets), under WASIX (std on
//! `wasm32-wasmer-wasi` lowers to `sock_*`), and — once the binding lands —
//! on the unofficial preview1 socket extension.
//!
//! Addressing (D1/D3): the guest gets `10.0.2.15/24`, gateway `10.0.2.2`,
//! DNS `10.0.2.3` — all answered by this stack. DNS A queries are resolved
//! host-side and answered with **synthetic** addresses allocated from
//! `10.0.2.16/28`...`10.0.2.254`; when the guest connects to a synthetic
//! address the tunnel looks the hostname back up and calls
//! `SocketBackend::connect(host, port)`. No NAT rewriting, no TUN/TAP.
//!
//! The guest-facing side is a [`vane_riscv::virtio_net::NetBackend`], so it
//! plugs straight into `VirtioNet`.

#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

pub mod dns;
mod tunnel;

#[cfg(feature = "std")]
mod pumped;
#[cfg(feature = "std")]
mod std_sockets;

// Binding B (M4): raw wasix_32v1 / unofficial-socket-extension syscalls.
// Only meaningful on wasm32-wasi targets; gated on the `wasix-raw` feature
// so plain builds never carry the imports.
#[cfg(all(target_os = "wasi", feature = "wasix-raw"))]
mod wasix_sockets;
#[cfg(all(target_os = "wasi", feature = "wasix-raw"))]
pub use wasix_sockets::WasixSockets;

pub use smoltcp::wire::Ipv4Address;
pub use tunnel::{QueueDevice, Tunnel, GATEWAY_IP, GUEST_DNS_IP, GUEST_IP, SYNTHETIC_BASE};

#[cfg(feature = "std")]
pub use pumped::PumpedTunnel;
#[cfg(feature = "std")]
pub use std_sockets::StdSockets;

//! End-to-end tunnel test (PLAN_PI_IN_VM M2, pre-gate): a guest-side
//! smoltcp stack (standing in for the Linux guest's virtio-net driver)
//! resolves a name through the tunnel's DNS responder, connects to the
//! synthetic address, and echoes bytes through a real host TCP listener —
//! all through the `SocketBackend` seam.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr};

use vane_host::{SockErr, SockFd, SocketBackend};
use vane_riscv::virtio_net::NetBackend;
use vane_net::{QueueDevice, StdSockets, Tunnel};

/// StdSockets with a name override map, so tests can "resolve" fake names
/// to loopback without touching real DNS.
struct MapSockets {
    inner: StdSockets,
    map: BTreeMap<String, String>,
}
impl SocketBackend for MapSockets {
    fn resolve(&mut self, name: &str) -> Result<Vec<[u8; 4]>, SockErr> {
        if self.map.contains_key(name) {
            Ok(vec![[127, 0, 0, 1]])
        } else {
            self.inner.resolve(name)
        }
    }
    fn connect(&mut self, host: &str, port: u16) -> Result<SockFd, SockErr> {
        let real = self.map.get(host).cloned().unwrap_or_else(|| host.to_string());
        self.inner.connect(&real, port)
    }
    fn send(&mut self, fd: SockFd, buf: &[u8]) -> Result<usize, SockErr> {
        self.inner.send(fd, buf)
    }
    fn recv(&mut self, fd: SockFd, buf: &mut [u8]) -> Result<usize, SockErr> {
        self.inner.recv(fd, buf)
    }
    fn shutdown(&mut self, fd: SockFd) {
        self.inner.shutdown(fd)
    }
    fn close(&mut self, fd: SockFd) {
        self.inner.close(fd)
    }
}

fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let mut conn = match conn {
                Ok(c) => c,
                Err(_) => break,
            };
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if conn.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

struct Guest {
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    now_ms: i64,
}

impl Guest {
    fn new() -> Self {
        let mut device = QueueDevice::new();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress([
            0x52, 0x54, 0, 0, 0, 2,
        ])));
        config.random_seed = 0x5eed;
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24)))
                .ok();
        });
        Guest {
            device,
            iface,
            sockets: SocketSet::new(Vec::new()),
            now_ms: 0,
        }
    }
    fn poll(&mut self, elapsed: i64) {
        self.now_ms += elapsed;
        let now = Instant::from_millis(self.now_ms);
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }
}

/// Pump frames between the guest stack and the tunnel until `done` says
/// stop (or the iteration budget runs out).
fn pump_until<B: SocketBackend>(
    guest: &mut Guest,
    tunnel: &mut Tunnel<B>,
    mut done: impl FnMut(&mut Guest) -> bool,
) {
    for _ in 0..20_000 {
        guest.poll(1);
        while let Some(f) = guest.device.tx.pop_front() {
            tunnel.send_frame(&f);
        }
        let mut moved = false;
        while let Some(f) = tunnel.poll_recv() {
            guest.device.rx.push_back(f);
            moved = true;
        }
        if moved {
            guest.poll(0);
        }
        if done(guest) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("pump budget exhausted");
}

fn dns_query_packet(id: u16, name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x0100u16.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&[0; 6]);
    for label in name.split('.') {
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.push(0);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p
}

/// Extract the first A record address from a DNS response.
fn first_a_record(resp: &[u8]) -> Option<Ipv4Address> {
    if resp.len() < 12 || resp[2] & 0x80 == 0 {
        return None;
    }
    let an = u16::from_be_bytes([resp[6], resp[7]]);
    if an == 0 {
        return None;
    }
    let mut off = 12;
    loop {
        let l = *resp.get(off)? as usize;
        if l == 0 {
            off += 1;
            break;
        }
        off += 1 + l;
    }
    off += 4; // qtype+qclass
    // Answer: name ptr(2) type(2) class(2) ttl(4) rdlen(2) rdata(4)
    let rdlen = u16::from_be_bytes([*resp.get(off + 10)?, *resp.get(off + 11)?]);
    if rdlen != 4 {
        return None;
    }
    Some(Ipv4Address::new(
        *resp.get(off + 12)?,
        *resp.get(off + 13)?,
        *resp.get(off + 14)?,
        *resp.get(off + 15)?,
    ))
}

#[test]
fn guest_dns_and_tcp_echo_through_tunnel() {
    let echo_port = echo_server();

    let mut backend = MapSockets {
        inner: StdSockets::new(),
        map: BTreeMap::new(),
    };
    backend
        .map
        .insert("echo.test".to_string(), "127.0.0.1".to_string());
    let mut tunnel = Tunnel::new(backend, [0x52, 0x54, 0, 0, 0, 1], 0x5eed);
    tunnel.add_listen_port(echo_port);

    let mut guest = Guest::new();

    // 1. DNS: resolve echo.test via 10.0.2.3 -> expect a synthetic 10.0.2.x.
    let dns = guest.sockets.add(udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0u8; 2048]),
    ));
    guest.sockets.get_mut::<udp::Socket>(dns).bind(49153).unwrap();
    let query = dns_query_packet(0x1234, "echo.test");
    guest
        .sockets
        .get_mut::<udp::Socket>(dns)
        .send_slice(&query, (IpAddress::Ipv4(vane_net::GUEST_DNS_IP), 53u16))
        .unwrap();

    let mut synthetic = None;
    eprintln!("stage: dns");
    pump_until(&mut guest, &mut tunnel, |g| {
        let sock = g.sockets.get_mut::<udp::Socket>(dns);
        if let Ok((data, _)) = sock.recv() {
            synthetic = first_a_record(data);
        }
        synthetic.is_some()
    });
    let synthetic = synthetic.expect("DNS answer");
    assert_eq!(
        synthetic.octets()[..3],
        [10, 0, 2],
        "synthetic address in 10.0.2.0/24"
    );

    // 2. TCP: connect to the synthetic address on the echo port.
    let sock_h = guest.sockets.add(tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; 8192]),
        tcp::SocketBuffer::new(vec![0u8; 8192]),
    ));
    {
        let (cx, sock) = (
            guest.iface.context(),
            guest.sockets.get_mut::<tcp::Socket>(sock_h),
        );
        sock.connect(cx, (IpAddress::Ipv4(synthetic), echo_port), 49152)
            .unwrap();
    }
    eprintln!("stage: tcp connect to {synthetic}");
    pump_until(&mut guest, &mut tunnel, |g| {
        g.sockets.get::<tcp::Socket>(sock_h).state() == tcp::State::Established
    });

    // 3. Echo roundtrip.
    guest
        .sockets
        .get_mut::<tcp::Socket>(sock_h)
        .send_slice(b"ping")
        .unwrap();
    let mut got = Vec::new();
    eprintln!("stage: echo");
    pump_until(&mut guest, &mut tunnel, |g| {
        let sock = g.sockets.get_mut::<tcp::Socket>(sock_h);
        if sock.can_recv() {
            sock.recv(|d| {
                got.extend_from_slice(d);
                (d.len(), ())
            })
            .ok();
        }
        got.len() >= 4
    });
    assert_eq!(&got, b"ping");

    // 4. Orderly close: guest FIN reaches the host, host EOF reaches the
    // guest.
    guest.sockets.get_mut::<tcp::Socket>(sock_h).close();
    eprintln!("stage: close");
    pump_until(&mut guest, &mut tunnel, |g| {
        matches!(
            g.sockets.get::<tcp::Socket>(sock_h).state(),
            tcp::State::Closed | tcp::State::TimeWait
        )
    });
}

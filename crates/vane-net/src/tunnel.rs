//! The tunnel core: frame queues <-> smoltcp <-> per-flow host sockets.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::VecDeque;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address, Ipv4Cidr,
};

use vane_host::{SockErr, SockFd, SocketBackend};
use vane_riscv::virtio_net::NetBackend;

/// Guest-side address plan (QEMU-user-style; the guest configures these
/// statically or via the DNS/forwarding rules below).
pub const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
pub const GUEST_DNS_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 3);
pub const GUEST_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
/// First synthetic address handed out by the DNS responder.
pub const SYNTHETIC_BASE: u8 = 16;

/// TCP ports we accept on any local (synthetic) address by default:
/// http, https, ssh, git.
pub const DEFAULT_LISTEN_PORTS: &[u16] = &[80, 443, 22, 9418];

const TCP_BUF: usize = 16384;
const MAX_SYNTHETIC: u8 = 254;

/// Frame queue pair implementing smoltcp's [`Device`]. `rx` holds frames
/// from the guest awaiting processing; frames smoltcp transmits accumulate
/// in `tx` for [`NetBackend::poll_recv`] to hand to the virtio device.
/// Public so tests can build a guest-side stack from the same pieces.
pub struct QueueDevice {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: VecDeque<Vec<u8>>,
}

impl QueueDevice {
    pub fn new() -> Self {
        QueueDevice {
            rx: VecDeque::new(),
            tx: VecDeque::new(),
        }
    }
}

impl Default for QueueDevice {
    fn default() -> Self {
        Self::new()
    }
}

pub struct QueueRx(Vec<u8>);
impl RxToken for QueueRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

pub struct QueueTx<'a>(&'a mut VecDeque<Vec<u8>>);
impl TxToken for QueueTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = alloc::vec![0u8; len];
        let r = f(&mut frame);
        self.0.push_back(frame);
        r
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = QueueRx;
    type TxToken<'a> = QueueTx<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((QueueRx(frame), QueueTx(&mut self.tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(QueueTx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(4);
        // Real checksums both ways: Linux computes them; no offload claimed.
        caps.checksum = ChecksumCapabilities::default();
        caps
    }
}

/// One proxied flow: a guest TCP socket paired with a host connection.
struct Flow {
    sock: SocketHandle,
    conn: SockFd,
    /// Guest half-closed its send side; forwarded to the host.
    guest_tx_closed: bool,
    /// Host sent EOF; we called `close` on the guest socket.
    host_eof: bool,
}

enum ServiceResult {
    Keep,
    Drop,
}

/// The tunnel itself. Drives smoltcp against a [`SocketBackend`].
///
/// Time: the stack needs a monotonic clock for TCP timers. `poll` advances
/// an internal millisecond counter by `elapsed_ms`; the [`NetBackend`]
/// methods advance it by 1 ms per call so the device works without any
/// clock wiring, and hosts with a real clock should prefer calling
/// [`Tunnel::poll`] with real elapsed time (the CLI does).
pub struct Tunnel<B: SocketBackend> {
    device: QueueDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    backend: B,
    now_ms: i64,
    /// TCP listen sockets, by port.
    listeners: BTreeMap<u16, SocketHandle>,
    dns: SocketHandle,
    flows: BTreeMap<SocketHandle, Flow>,
    /// Synthetic IP -> hostname, plus static (possibly real-IP) mappings.
    names: BTreeMap<Ipv4Address, String>,
    next_synthetic: u8,
}

impl<B: SocketBackend> Tunnel<B> {
    /// Create a tunnel on `backend` with interface MAC `mac`, listening on
    /// `DEFAULT_LISTEN_PORTS`. `seed` feeds smoltcp's TCP sequence RNG.
    pub fn new(backend: B, mac: [u8; 6], seed: u64) -> Self {
        let mut device = QueueDevice::new();
        let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = seed;
        let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(GATEWAY_IP, 24))).ok();
        });
        // AnyIP: accept packets addressed to ANY address, not just assigned
        // ones. smoltcp's interface address table is fixed-capacity (2 by
        // default, build-time configurable) so per-synthetic-address
        // assignment does not scale; with AnyIP the DNS responder can hand
        // out arbitrary synthetic addresses and TCP to any of them is
        // accepted locally. Safe here: this is an isolated two-node segment
        // (guest + tunnel), and smoltcp discards ARP probes from the
        // unspecified address, so guest DAD is unaffected.
        iface.set_any_ip(true);
        // No gateway of our own: every flow is guest-initiated and proxied.
        let mut sockets = SocketSet::new(Vec::new());
        let dns = sockets.add(udp::Socket::new(
            udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 8], alloc::vec![0u8; 4096]),
            udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 8], alloc::vec![0u8; 8192]),
        ));
        let mut t = Tunnel {
            device,
            iface,
            sockets,
            backend,
            now_ms: 0,
            listeners: BTreeMap::new(),
            dns,
            flows: BTreeMap::new(),
            names: BTreeMap::new(),
            next_synthetic: SYNTHETIC_BASE,
        };
        // Bind DNS to its advertised address so responses carry 10.0.2.3 as
        // source (strict resolvers check this).
        t.sockets.get_mut::<udp::Socket>(t.dns)
            .bind(IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(GUEST_DNS_IP)),
                port: 53,
            })
            .ok();
        for &p in DEFAULT_LISTEN_PORTS {
            t.add_listen_port(p);
        }
        t
    }

    /// Accept guest TCP connections to `port` (on any local/synthetic
    /// address) and proxy them to `host:port`.
    pub fn add_listen_port(&mut self, port: u16) {
        if self.listeners.contains_key(&port) {
            return;
        }
        let h = self.sockets.add(Self::tcp_socket());
        if self.sockets.get_mut::<tcp::Socket>(h).listen(port).is_ok() {
            self.listeners.insert(port, h);
        }
    }

    /// Statically map a guest-visible address to a hostname, e.g.
    /// `map_static(10.0.2.100, "registry.npmjs.org")`. Useful for fixed
    /// forwarding rules and tests that bypass DNS. With AnyIP no address
    /// assignment is needed — the name table is the whole mapping.
    pub fn map_static(&mut self, ip: Ipv4Address, host: String) {
        self.names.insert(ip, host);
    }

    /// Current interface addresses (diagnostics; also used by tests to
    /// verify synthetic-address allocation).
    pub fn local_addrs(&self) -> Vec<IpCidr> {
        self.iface.ip_addrs().to_vec()
    }

    /// Look up the hostname a guest-visible address maps to (diagnostics).
    pub fn name_of(&self, ip: Ipv4Address) -> Option<&str> {
        self.names.get(&ip).map(String::as_str)
    }

    fn tcp_socket() -> tcp::Socket<'static> {
        tcp::Socket::new(
            tcp::SocketBuffer::new(alloc::vec![0u8; TCP_BUF]),
            tcp::SocketBuffer::new(alloc::vec![0u8; TCP_BUF]),
        )
    }

    /// Advance the clock and run one service pass: smoltcp poll, DNS,
    /// listener accepts, flow pumping. Hosts with a real clock call this
    /// with real elapsed milliseconds.
    pub fn poll(&mut self, elapsed_ms: u32) {
        self.now_ms += elapsed_ms as i64;
        let now = Instant::from_millis(self.now_ms);
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        self.service_dns();
        self.service_listeners();
        self.service_flows();
        self.reap_flows();
    }

    fn alloc_synthetic(&mut self, name: &str) -> Option<Ipv4Address> {
        // Reuse an existing mapping for the same name (idempotent DNS).
        for (ip, n) in &self.names {
            if n == name && ip.octets()[0] == 10 {
                return Some(*ip);
            }
        }
        if self.next_synthetic > MAX_SYNTHETIC {
            return None;
        }
        let ip = Ipv4Address::new(10, 0, 2, self.next_synthetic);
        self.next_synthetic += 1;
        self.names.insert(ip, String::from(name));
        Some(ip)
    }

    fn service_dns(&mut self) {
        loop {
            let (pkt, meta) = {
                let sock = self.sockets.get_mut::<udp::Socket>(self.dns);
                match sock.recv() {
                    Ok(v) => (v.0.to_vec(), v.1),
                    Err(_) => break,
                }
            };
            let Some(q) = crate::dns::parse_query(&pkt) else {
                continue;
            };
            // Validate the name host-side, then hand out a synthetic IP.
            let answer = match self.backend.resolve(&q.name) {
                Ok(addrs) if !addrs.is_empty() => self.alloc_synthetic(&q.name),
                _ => None,
            };
            let Some(ip) = answer else {
                continue; // NXDOMAIN handling: drop for now (guest retries/fails fast)
            };
            let mut out = Vec::new();
            crate::dns::build_response(&q, ip.octets(), 60, &mut out);
            let sock = self.sockets.get_mut::<udp::Socket>(self.dns);
            sock.send_slice(&out, meta).ok();
        }
    }

    fn service_listeners(&mut self) {
        let ports: Vec<u16> = self.listeners.keys().copied().collect();
        for port in ports {
            let Some(&h) = self.listeners.get(&port) else { continue };
            let (state, local) = {
                let s = self.sockets.get::<tcp::Socket>(h);
                (s.state(), s.local_endpoint())
            };
            if state == tcp::State::Listen {
                continue;
            }
            // Accepted connection: the listen socket became the flow socket.
            // Replace the listener, then connect the host side.
            self.listeners.remove(&port);
            self.add_listen_port(port);
            let Some(smoltcp::wire::IpAddress::Ipv4(ip)) = local.map(|ep| ep.addr) else {
                self.sockets.get_mut::<tcp::Socket>(h).abort();
                self.sockets.remove(h);
                continue;
            };
            let Some(host) = self.names.get(&ip).cloned() else {
                // Direct-IP connect to a synthetic we don't know: refuse.
                self.sockets.get_mut::<tcp::Socket>(h).abort();
                self.sockets.remove(h);
                continue;
            };
            match self.backend.connect(&host, port) {
                Ok(fd) => {
                    self.flows.insert(
                        h,
                        Flow {
                            sock: h,
                            conn: fd,
                            guest_tx_closed: false,
                            host_eof: false,
                        },
                    );
                }
                Err(_) => {
                    self.sockets.get_mut::<tcp::Socket>(h).abort();
                    self.sockets.remove(h);
                }
            }
        }
    }

    fn service_one_flow(&mut self, h: SocketHandle) -> ServiceResult {
        let Some(flow) = self.flows.get_mut(&h) else {
            return ServiceResult::Drop;
        };
        let fd = flow.conn;
        // Guest -> host.
        {
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            if s.can_recv() {
                let mut host_err = None;
                s.recv(|data| {
                    match self.backend.send(fd, data) {
                        Ok(n) => (n, ()),
                        Err(SockErr::WouldBlock) => (0, ()),
                        Err(e) => {
                            host_err = Some(e);
                            (0, ())
                        }
                    }
                })
                .ok();
                if host_err.is_some() {
                    self.sockets.get_mut::<tcp::Socket>(h).abort();
                    return ServiceResult::Drop;
                }
            }
        }
        // Host -> guest.
        {
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            if s.can_send() && !flow.host_eof {
                let mut buf = alloc::vec![0u8; 8192];
                match self.backend.recv(fd, &mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        s.send_slice(&buf[..n]).ok();
                    }
                    Err(SockErr::WouldBlock) => {}
                    Err(SockErr::Eof) => {
                        flow.host_eof = true;
                        s.close();
                    }
                    Err(_) => {
                        s.abort();
                        return ServiceResult::Drop;
                    }
                }
            }
        }
        // Guest half-close -> host half-close.
        let state = self.sockets.get::<tcp::Socket>(h).state();
        if !flow.guest_tx_closed
            && matches!(state, tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closed)
        {
            flow.guest_tx_closed = true;
            self.backend.shutdown(fd);
        }
        if state == tcp::State::Closed {
            return ServiceResult::Drop;
        }
        ServiceResult::Keep
    }

    fn service_flows(&mut self) {
        let handles: Vec<SocketHandle> = self.flows.keys().copied().collect();
        for h in handles {
            if matches!(self.service_one_flow(h), ServiceResult::Drop) {
                let flow = self.flows.remove(&h);
                if let Some(f) = flow {
                    self.backend.close(f.conn);
                }
                // Invariant: flow sockets are only ever removed here, so the
                // handle is still present in the set.
                self.sockets.remove(h);
            }
        }
    }

    fn reap_flows(&mut self) {
        // Currently a no-op by invariant (flow sockets are only removed in
        // `service_flows`, which closes the host side in the same pass).
        // Kept as the place to add defensive reconciliation if that ever
        // changes.
    }
}

impl<B: SocketBackend> NetBackend for Tunnel<B> {
    fn send_frame(&mut self, frame: &[u8]) {
        self.device.rx.push_back(frame.to_vec());
        self.poll(1);
    }

    fn poll_recv(&mut self) -> Option<Vec<u8>> {
        self.poll(1);
        self.device.tx.pop_front()
    }

    fn link_up(&self) -> bool {
        true
    }
}

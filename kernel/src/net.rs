//! TCP/IP networking on top of smoltcp: DHCP, DNS, and blocking TCP streams.
//!
//! One global stack owns the NIC. A background task ("net") polls it every timer
//! tick so DHCP, ARP and TCP retransmissions progress even when nobody is reading.
//! Blocking calls ([`resolve`], [`TcpStream`]) poll the stack themselves and park in
//! `task::wait_event` between polls, so other tasks keep running.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr};

use crate::drivers::Nic;
use crate::sync::SpinLock;
use crate::{pit, rand, task};

struct NicDevice(Box<dyn Nic>);

static RX_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TX_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// (received, sent) Ethernet bytes since boot.
pub fn counters() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (RX_BYTES.load(Relaxed), TX_BYTES.load(Relaxed))
}

struct RxToken(Vec<u8>);
struct TxToken<'a>(&'a mut dyn Nic);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        TX_BYTES.fetch_add(len as u64, core::sync::atomic::Ordering::Relaxed);
        // If the ring is full the frame is dropped; TCP retransmits.
        self.0.transmit(&frame);
        result
    }
}

impl phy::Device for NicDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _: Instant) -> Option<(RxToken, TxToken<'_>)> {
        loop {
            let frame = self.0.receive()?;
            if !frame.is_empty() {
                RX_BYTES.fetch_add(frame.len() as u64, core::sync::atomic::Ordering::Relaxed);
                return Some((RxToken(frame), TxToken(&mut *self.0)));
            }
        }
    }

    fn transmit(&mut self, _: Instant) -> Option<TxToken<'_>> {
        self.0.can_transmit().then(|| TxToken(&mut *self.0))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(32);
        caps
    }
}

struct Stack {
    iface: Interface,
    sockets: SocketSet<'static>,
    device: NicDevice,
    dhcp: SocketHandle,
    dns: SocketHandle,
    ip: Option<Ipv4Cidr>,
    gateway: Option<Ipv4Address>,
    dns_servers: Vec<Ipv4Address>,
    next_port: u16,
}

static STACK: SpinLock<Option<Stack>> = SpinLock::new(None);

fn now() -> Instant {
    Instant::from_millis((pit::ticks() * 1000 / pit::HZ) as i64)
}

/// Run `f` on the stack. Never called while the stack is borrowed: tasks do not
/// yield inside `f`, and IRQ handlers do not use the network.
fn with_stack<R>(f: impl FnOnce(&mut Stack) -> R) -> Result<R, String> {
    let mut guard = STACK.try_lock().expect("network stack re-entered");
    let stack = guard.as_mut().ok_or("no network interface")?;
    Ok(f(stack))
}

impl Stack {
    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.device, &mut self.sockets);
        match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(config)) => {
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(config.address));
                });
                match config.router {
                    Some(router) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(router);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                self.ip = Some(config.address);
                self.gateway = config.router;
                self.dns_servers = config.dns_servers.iter().copied().collect();
                let servers: Vec<IpAddress> = self.dns_servers.iter().map(|&a| a.into()).collect();
                self.sockets.get_mut::<dns::Socket>(self.dns).update_servers(&servers);
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.iface.update_ip_addrs(|addrs| addrs.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.ip = None;
                self.gateway = None;
            }
            None => {}
        }
    }
}

/// Take ownership of the first NIC and start DHCP and the polling task.
pub fn init(nic: Box<dyn Nic>) {
    let mut device = NicDevice(nic);
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(device.0.mac())));
    config.random_seed = rand::u64();
    let iface = Interface::new(config, &mut device, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let dns = sockets.add(dns::Socket::new(&[], Vec::new()));
    let next_port = 49152 + (rand::u64() % 8192) as u16;
    *STACK.try_lock().expect("network init re-entered") = Some(Stack {
        iface,
        sockets,
        device,
        dhcp,
        dns,
        ip: None,
        gateway: None,
        dns_servers: Vec::new(),
        next_port,
    });
    task::spawn("net", || loop {
        let _ = with_stack(|s| {
            s.poll();
            reap_orphans(s);
        });
        // Blocked socket calls and epoll waits re-check after every poll.
        crate::ipc::notify();
        task::wait_event();
    });
}

pub fn available() -> bool {
    STACK.try_lock().is_some_and(|s| s.is_some())
}

/// Network status for `ifconfig`/the desktop: MAC, link, address, gateway, DNS.
pub struct Info {
    pub mac: [u8; 6],
    pub link: bool,
    pub ip: Option<Ipv4Cidr>,
    pub gateway: Option<Ipv4Address>,
    pub dns: Vec<Ipv4Address>,
}

/// One line per TCP socket: state, endpoints, bytes waiting to be read and to be sent,
/// and the peer's advertised window (the `netstat` shell command).
pub fn tcp_sockets() -> Vec<String> {
    with_stack(|s| {
        let mut v = Vec::new();
        for (h, sock) in s.sockets.iter() {
            if let smoltcp::socket::Socket::Tcp(t) = sock {
                v.push(alloc::format!(
                    "{h:?} {:?} local {:?} remote {:?} recv-q {} send-q {} can-recv {} can-send {}",
                    t.state(),
                    t.local_endpoint(),
                    t.remote_endpoint(),
                    t.recv_queue(),
                    t.send_queue(),
                    t.can_recv(),
                    t.can_send()
                ));
            }
        }
        v
    })
    .unwrap_or_default()
}

pub fn info() -> Option<Info> {
    with_stack(|s| Info { mac: s.device.0.mac(), link: s.device.0.link_up(), ip: s.ip, gateway: s.gateway, dns: s.dns_servers.clone() })
        .ok()
}

/// Wait until DHCP has configured an address, up to `timeout_ticks`.
pub fn wait_configured(timeout_ticks: u64) -> bool {
    let end = pit::ticks() + timeout_ticks;
    loop {
        if with_stack(|s| s.ip.is_some()).unwrap_or(false) {
            return true;
        }
        if pit::ticks() >= end {
            return false;
        }
        task::wait_event();
    }
}

/// Resolve a host name (or dotted IPv4 literal) to an IPv4 address.
pub fn resolve(host: &str) -> Result<Ipv4Address, String> {
    if let Ok(ip) = host.parse::<Ipv4Address>() {
        return Ok(ip);
    }
    if !wait_configured(10 * pit::HZ) {
        return Err("network not configured (no DHCP lease)".into());
    }
    let query = with_stack(|s| {
        let cx = s.iface.context();
        s.sockets.get_mut::<dns::Socket>(s.dns).start_query(cx, host, DnsQueryType::A)
    })?
    .map_err(|e| format!("DNS query for {host} failed to start: {e:?}"))?;
    let end = pit::ticks() + 10 * pit::HZ;
    loop {
        let result = with_stack(|s| {
            s.poll();
            s.sockets.get_mut::<dns::Socket>(s.dns).get_query_result(query)
        })?;
        match result {
            Ok(addrs) => {
                return addrs
                    .iter()
                    .find_map(|a| match a {
                        IpAddress::Ipv4(v4) => Some(*v4),
                        #[allow(unreachable_patterns)]
                        _ => None,
                    })
                    .ok_or_else(|| format!("{host} has no IPv4 address"));
            }
            Err(dns::GetQueryResultError::Pending) => {}
            Err(e) => return Err(format!("could not resolve {host}: {e:?}")),
        }
        if pit::ticks() >= end {
            return Err(format!("DNS timeout resolving {host}"));
        }
        task::wait_event();
    }
}

/// A blocking TCP connection. Dropping it aborts the connection.
pub struct TcpStream {
    handle: SocketHandle,
}

const TIMEOUT_TICKS: u64 = 30 * pit::HZ;

impl TcpStream {
    pub fn connect(ip: Ipv4Address, port: u16) -> Result<Self, String> {
        if !wait_configured(10 * pit::HZ) {
            return Err("network not configured (no DHCP lease)".into());
        }
        let handle = with_stack(|s| {
            let socket = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 256 * 1024]), tcp::SocketBuffer::new(vec![0; 64 * 1024]));
            let handle = s.sockets.add(socket);
            let local = s.next_port;
            s.next_port = if s.next_port >= 65000 { 49152 } else { s.next_port + 1 };
            let cx = s.iface.context();
            s.sockets.get_mut::<tcp::Socket>(handle).connect(cx, (ip, port), local).map(|_| handle).map_err(|e| {
                s.sockets.remove(handle);
                format!("connect to {ip}:{port}: {e:?}")
            })
        })??;
        let stream = Self { handle };
        let end = pit::ticks() + TIMEOUT_TICKS;
        loop {
            let state = with_stack(|s| {
                s.poll();
                s.sockets.get::<tcp::Socket>(handle).state()
            })?;
            match state {
                tcp::State::Established => return Ok(stream),
                tcp::State::Closed | tcp::State::TimeWait => return Err(format!("connection to {ip}:{port} refused")),
                _ if pit::ticks() >= end => return Err(format!("connection to {ip}:{port} timed out")),
                _ => task::wait_event(),
            }
        }
    }

    /// Send all of `data`.
    pub fn write_all(&mut self, mut data: &[u8]) -> Result<(), String> {
        let end = pit::ticks() + TIMEOUT_TICKS;
        while !data.is_empty() {
            let sent = with_stack(|s| {
                let socket = s.sockets.get_mut::<tcp::Socket>(self.handle);
                let r = if socket.can_send() { socket.send_slice(data).map_err(|e| e.to_string()) } else if socket.may_send() { Ok(0) } else { Err("connection closed".into()) };
                s.poll();
                r
            })??;
            data = &data[sent..];
            if sent == 0 {
                if pit::ticks() >= end {
                    return Err("send timed out".into());
                }
                task::wait_event();
            }
        }
        Ok(())
    }

    /// Read up to `buf.len()` bytes; Ok(0) means the peer closed the connection.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        let end = pit::ticks() + TIMEOUT_TICKS;
        loop {
            let r = with_stack(|s| {
                s.poll();
                let socket = s.sockets.get_mut::<tcp::Socket>(self.handle);
                if socket.can_recv() {
                    socket.recv_slice(buf).map(Some).map_err(|e| e.to_string())
                } else if !socket.may_recv() {
                    Ok(Some(0))
                } else {
                    Ok(None)
                }
            })??;
            match r {
                Some(n) => return Ok(n),
                None if pit::ticks() >= end => return Err("receive timed out".into()),
                None => task::wait_event(),
            }
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = with_stack(|s| {
            s.sockets.get_mut::<tcp::Socket>(self.handle).abort();
            s.poll();
            s.sockets.remove(self.handle);
        });
    }
}

// ---------------------------------------------------------------------------
// Sockets for user programs (linux.rs): non-blocking primitives over smoltcp.

/// Why a socket operation could not complete now.
pub enum SockErr {
    WouldBlock,
    Closed,
    Refused,
    NoNetwork,
}

/// Sockets closed by their program but still finishing (TCP FIN handshake).
static ORPHANS: SpinLock<Vec<(SocketHandle, u64)>> = SpinLock::new(Vec::new());

fn alloc_port(s: &mut Stack) -> u16 {
    let p = s.next_port;
    s.next_port = if s.next_port >= 65000 { 49152 } else { s.next_port + 1 };
    p
}

pub fn tcp_open() -> Result<SocketHandle, SockErr> {
    with_stack(|s| s.sockets.add(tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 512 * 1024]), tcp::SocketBuffer::new(vec![0; 128 * 1024]))))
        .map_err(|_| SockErr::NoNetwork)
}

pub fn udp_open() -> Result<SocketHandle, SockErr> {
    use smoltcp::socket::udp;
    with_stack(|s| {
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 128 * 1024]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 64 * 1024]);
        s.sockets.add(udp::Socket::new(rx, tx))
    })
    .map_err(|_| SockErr::NoNetwork)
}

/// Start a TCP connection; completes in the background.
pub fn tcp_connect(h: SocketHandle, ip: Ipv4Address, port: u16) -> Result<u16, SockErr> {
    let r = with_stack(|s| {
        let local = alloc_port(s);
        let cx = s.iface.context();
        let r = s.sockets.get_mut::<tcp::Socket>(h).connect(cx, (ip, port), local).map(|_| local);
        s.poll();
        r
    })
    .map_err(|_| SockErr::NoNetwork)?;
    r.map_err(|_| SockErr::Refused)
}

/// Readiness of a socket: (readable, writable, hung up, error).
pub fn sock_poll(h: SocketHandle, udp: bool) -> (bool, bool, bool, bool) {
    with_stack(|s| {
        if udp {
            let u = s.sockets.get::<smoltcp::socket::udp::Socket>(h);
            (u.can_recv(), u.can_send(), false, false)
        } else {
            let t = s.sockets.get::<tcp::Socket>(h);
            let state = t.state();
            let failed = state == tcp::State::Closed && !t.is_open();
            (t.can_recv() || (!t.may_recv() && state != tcp::State::SynSent), t.can_send(), !t.may_recv() && state != tcp::State::SynSent && state != tcp::State::Listen, failed)
        }
    })
    .unwrap_or((false, false, true, true))
}

/// Has the connection been established (or failed)? None = still connecting.
pub fn tcp_connected(h: SocketHandle) -> Option<bool> {
    with_stack(|s| match s.sockets.get::<tcp::Socket>(h).state() {
        tcp::State::SynSent | tcp::State::SynReceived => None,
        tcp::State::Closed | tcp::State::TimeWait => Some(false),
        _ => Some(true),
    })
    .unwrap_or(Some(false))
}

pub fn tcp_send(h: SocketHandle, data: &[u8]) -> Result<usize, SockErr> {
    with_stack(|s| {
        let t = s.sockets.get_mut::<tcp::Socket>(h);
        let r = if !t.may_send() {
            if t.state() == tcp::State::SynSent { Err(SockErr::WouldBlock) } else { Err(SockErr::Closed) }
        } else if !t.can_send() {
            Err(SockErr::WouldBlock)
        } else {
            t.send_slice(data).map_err(|_| SockErr::Closed)
        };
        s.poll();
        r
    })
    .map_err(|_| SockErr::NoNetwork)?
}

/// Receive TCP data. With `peek` the data stays queued (MSG_PEEK: Chromium peeks one
/// byte to check that an idle connection is still alive; consuming it corrupts TLS).
pub fn tcp_recv(h: SocketHandle, buf: &mut [u8], peek: bool) -> Result<usize, SockErr> {
    with_stack(|s| {
        s.poll();
        let t = s.sockets.get_mut::<tcp::Socket>(h);
        if t.can_recv() {
            if peek { t.peek_slice(buf) } else { t.recv_slice(buf) }.map_err(|_| SockErr::Closed)
        } else if !t.may_recv() && t.state() != tcp::State::SynSent {
            Ok(0)
        } else {
            Err(SockErr::WouldBlock)
        }
    })
    .map_err(|_| SockErr::NoNetwork)?
}

pub fn udp_bind(h: SocketHandle, port: u16) -> Result<u16, SockErr> {
    with_stack(|s| {
        let p = if port == 0 { alloc_port(s) } else { port };
        s.sockets.get_mut::<smoltcp::socket::udp::Socket>(h).bind(p).map(|_| p).map_err(|_| SockErr::Refused)
    })
    .map_err(|_| SockErr::NoNetwork)?
}

pub fn udp_send(h: SocketHandle, data: &[u8], ip: Ipv4Address, port: u16) -> Result<usize, SockErr> {
    with_stack(|s| {
        let u = s.sockets.get_mut::<smoltcp::socket::udp::Socket>(h);
        let r = if !u.can_send() { Err(SockErr::WouldBlock) } else { u.send_slice(data, (ip, port)).map(|_| data.len()).map_err(|_| SockErr::Refused) };
        s.poll();
        r
    })
    .map_err(|_| SockErr::NoNetwork)?
}

pub fn udp_recv(h: SocketHandle, buf: &mut [u8], peek: bool) -> Result<(usize, Ipv4Address, u16), SockErr> {
    with_stack(|s| {
        s.poll();
        let u = s.sockets.get_mut::<smoltcp::socket::udp::Socket>(h);
        let r = if peek {
            u.peek().map(|(data, meta)| {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                (n, *meta)
            })
        } else {
            u.recv_slice(buf)
        };
        match r {
            Ok((n, meta)) => match meta.endpoint.addr {
                IpAddress::Ipv4(a) => Ok((n, a, meta.endpoint.port)),
                #[allow(unreachable_patterns)]
                _ => Err(SockErr::WouldBlock),
            },
            Err(_) => Err(SockErr::WouldBlock),
        }
    })
    .map_err(|_| SockErr::NoNetwork)?
}

pub fn local_ip() -> Ipv4Address {
    info().and_then(|i| i.ip).map(|c| c.address()).unwrap_or(Ipv4Address::new(0, 0, 0, 0))
}

/// Close a user socket: UDP goes away now, TCP finishes its handshake first.
pub fn sock_close(h: SocketHandle, udp: bool) {
    let _ = with_stack(|s| {
        if udp {
            s.sockets.remove(h);
        } else {
            s.sockets.get_mut::<tcp::Socket>(h).close();
            s.poll();
        }
    });
    if !udp {
        ORPHANS.lock().push((h, pit::ticks()));
    }
}

/// Remove orphaned TCP sockets that are done (or stuck for 30 s). Called by the net task.
fn reap_orphans(s: &mut Stack) {
    let Some(mut orphans) = ORPHANS.try_lock() else { return };
    let now = pit::ticks();
    orphans.retain(|&(h, since)| {
        let t = s.sockets.get_mut::<tcp::Socket>(h);
        let done = matches!(t.state(), tcp::State::Closed | tcp::State::TimeWait) || now - since > 30 * pit::HZ;
        if done {
            t.abort();
            s.sockets.remove(h);
        }
        !done
    });
}

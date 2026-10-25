//! net: TCP/IP stack (smoltcp) over one link at a time (`net` protocol):
//! Wi-Fi ("wlan0"), the PC's connection shared over USB-C ("usb0") and a
//! USB Ethernet adapter ("eth0"), the first with a carrier in the order
//! `PREFER` sets (Wi-Fi first by default). DHCP/DNS plus deferred replies
//! for connect/accept/receive/resolve.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libnx::abi::err::{
    EADDRINUSE, EAGAIN, EBADF, ECONNREFUSED, ECONNRESET, EINVAL, ENETDOWN, ENETUNREACH, ENOENT,
    ENOTCONN, EPIPE, ETIMEDOUT,
};
use libnx::abi::proto::{eth, net as proto};
use libnx::drv;
use libnx::ipc::{self, Msg};
use libnx::sync::Mutex;
use libnx::{ns, println, sys, thread, time};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp, udp};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
    IpEndpoint, Ipv4Address,
};

/// TCP buffers: the receive window is what a connection can have in
/// flight, so its rate is at most window / round trip (64 KiB at 40 ms:
/// 13 Mb/s); 2 MiB makes a few hundred Mb/s possible (window scaling is on
/// past 64 KiB).
const TCP_RX_BUF: usize = 2 * 1024 * 1024;
const TCP_TX_BUF: usize = 256 * 1024;
/// Echo request payload, and echo replies kept for the pings waiting.
const PING_DATA: [u8; 32] = *b"nxk ping 0123456789abcdefghijklm";
const PONGS_MAX: usize = 64;
const UDP_BUF: usize = 64 * 1024;
/// Data bytes per reply message.
const MAX_IO: usize = 3 * 1024;
/// Largest frame (Ethernet MTU with header).
const FRAME: usize = 1514;

/// Traffic counters (`proto::STATS`): frames and bytes each way.
static RX_FRAMES: AtomicU64 = AtomicU64::new(0);
static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static TX_FRAMES: AtomicU64 = AtomicU64::new(0);
static TX_BYTES: AtomicU64 = AtomicU64::new(0);

/// Links that went down (`ENETDOWN` from their driver), one bit per
/// `LINKS` entry: DHCP starts over on the one in use, since the network
/// may be another one once it is back.
static LINK_DOWN: AtomicU64 = AtomicU64::new(0);
/// Frames dropped because the main loop did not take them in time.
static RX_DROPS: AtomicU64 = AtomicU64::new(0);

/// The links: name and `LINK_*`.
const LINKS: [(&str, u64); 3] = [
    ("usb0", proto::LINK_USB0),
    ("eth0", proto::LINK_ETH0),
    ("wlan0", proto::LINK_WLAN0),
];
/// `LINKS` indices in order of preference, for each `PREF_*`.
const ORDER_WIFI: [usize; 3] = [2, 0, 1];
const ORDER_WIRED: [usize; 3] = [0, 1, 2];
/// How often links are looked up and their carriers asked.
const LINK_CHECK_NS: u64 = 2_000_000_000;
/// A link without a DHCP address this long gives way to another (a PC
/// that does not share its connection, a Wi-Fi network without DHCP),
/// until its carrier drops.
const DHCP_GIVE_UP_NS: u64 = 10_000_000_000;
/// A link that gave way is tried again after this long, then twice, four
/// times... as long, up to 16 times: each try costs the network 10 s.
const SKIP_RETRY_NS: u64 = 60_000_000_000;
/// A better link takes over from the one in use once its carrier has been
/// up this long: a Wi-Fi blip would otherwise cost two switches, and each
/// one aborts every connection.
const SETTLE_NS: u64 = 5_000_000_000;
/// DHCP retransmissions: a lost first DISCOVER (the port is often not
/// open yet right after a Wi-Fi join) costs 2 s, not smoltcp's 10.
const DHCP_RETRY: Duration = Duration::from_secs(2);

fn count(frames: &AtomicU64, bytes: &AtomicU64, n: usize) {
    frames.fetch_add(1, Ordering::Relaxed);
    bytes.fetch_add(n as u64, Ordering::Relaxed);
}

// ------------------------------------------------------------ interface ---

/// Frames of all links, tagged with their `LINKS` index.
type Frames = Arc<Mutex<VecDeque<(usize, Vec<u8>)>>>;

struct Dev {
    /// Link in use (0: none) and its `LINKS` index.
    eth: u32,
    link: usize,
    rx: Frames,
}

struct Rx(Vec<u8>);
struct Tx(u32);

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = [0u8; FRAME];
        let r = f(&mut b[..len]);
        count(&TX_FRAMES, &TX_BYTES, len);
        // Driver TX ring full: let what is there drain.
        for _ in 0..100 {
            match ipc::call(self.0, &ipc::pack(eth::SEND, &b[..len])) {
                Ok(m) if m.label() == EAGAIN as u32 => time::sleep_us(100),
                _ => break,
            }
        }
        r
    }
}

impl Device for Dev {
    type RxToken<'a>
        = Rx
    where
        Self: 'a;
    type TxToken<'a>
        = Tx
    where
        Self: 'a;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx)> {
        if self.eth == 0 {
            return None;
        }
        let mut q = self.rx.lock();
        // Frames of links not in use are dropped.
        let f = loop {
            match q.pop_front()? {
                (l, f) if l == self.link => break f,
                _ => {}
            }
        };
        drop(q);
        count(&RX_FRAMES, &RX_BYTES, f.len());
        Some((Rx(f), Tx(self.eth)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx> {
        (self.eth != 0).then_some(Tx(self.eth))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = FRAME;
        c
    }
}

// --------------------------------------------------------------- sockets ---

enum Sock {
    Tcp(SocketHandle),
    Listener { port: u16, pool: Vec<SocketHandle> },
    Udp(SocketHandle),
}

enum Op {
    Connect { sock: usize },
    Accept { listener: usize },
    Recv { sock: usize, max: usize },
    Send { sock: usize, data: Vec<u8> },
    RecvFrom { sock: usize },
    Resolve { query: dns::QueryHandle },
    Ping { addr: u32, seq: u16, sent: u64 },
}

/// Pending operation: what is awaited, who to reply to, until when.
struct Wait {
    op: Op,
    token: u64,
    deadline: u64,
}

fn ip(v: u64) -> Ipv4Address {
    Ipv4Address::from(v as u32)
}

fn word(a: IpAddress) -> u64 {
    match a {
        IpAddress::Ipv4(v) => u32::from(v) as u64,
    }
}

fn deadline(timeout: u64) -> u64 {
    if timeout == 0 {
        u64::MAX
    } else {
        time::now_ns().saturating_add(timeout)
    }
}

struct Net {
    iface: Interface,
    dev: Dev,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    dns: SocketHandle,
    table: Vec<Option<Sock>>,
    /// Reference count of each `table` entry (DUP).
    refs: Vec<u32>,
    /// Owner process of each entry (pid stamped by the kernel): only it
    /// uses it, and its sockets close if it dies.
    owners: Vec<u32>,
    /// Requests since the last orphan-socket reap.
    since_reap: u32,
    waits: Vec<Wait>,
    next_port: u16,
    addr: Option<(Ipv4Address, u8)>,
    gateway: Option<Ipv4Address>,
    dns_server: Option<Ipv4Address>,
    /// TCP sockets closed by their client, at end of connection.
    closing: Vec<SocketHandle>,
    /// The ICMP socket (bound to `ident`), the last sequence number sent,
    /// and echo replies received: (address, sequence, when).
    icmp: SocketHandle,
    ident: u16,
    seq: u16,
    pongs: VecDeque<(u32, u16, u64)>,
    /// Interface in use (`proto::LINK_*`, 0: none).
    link: u64,
    links: [Link; 3],
    /// `LINKS` index in use and since when.
    active: Option<usize>,
    since: u64,
    /// `PREF_*`: which links come first.
    pref: u64,
    /// The preference just changed: take the best link now, not once it
    /// has settled.
    retarget: bool,
    rxq: Frames,
    notif: u32,
}

/// A link as last seen.
#[derive(Default)]
struct Link {
    /// Service handle (0: not published yet).
    handle: u32,
    mac: [u8; 6],
    carrier: bool,
    /// When it gave way for want of a DHCP address (0: it did not); cleared
    /// when the carrier drops, and tried again after `SKIP_RETRY_NS`.
    skipped_at: u64,
    /// How many times in a row (0 once it had an address, or its carrier dropped).
    skips: u32,
    /// When the carrier came up (ns).
    up_since: u64,
    /// Ends the receive thread of the handle held (its driver is gone).
    rx_stop: Option<Arc<AtomicBool>>,
}

/// Receive thread of link `k`: blocks on its driver, queues frames and
/// signals the main thread.
fn receive(k: usize, eth: u32, q: Frames, notif: u32, stop: Arc<AtomicBool>) {
    let eth = sys::handle_dup(eth, libnx::abi::rights::SEND, 0).unwrap_or(eth);
    let r = thread::spawn(move || {
        let mut frame = vec![0u8; ipc::MAX_BYTES];
        while !stop.load(Ordering::Relaxed) {
            let mut m = Msg::new(eth::RECV, 1, 0);
            m.mr[0] = 0;
            match ipc::call(eth, &m) {
                Ok(r) if r.label() == 0 => {
                    let n = ipc::unpack(&r, &mut frame);
                    let mut g = q.lock();
                    if g.len() < 256 {
                        g.push_back((k, frame[..n].to_vec()));
                    } else if RX_DROPS.fetch_add(1, Ordering::Relaxed).is_multiple_of(256) {
                        println!("[net] receive queue full: frames dropped");
                    }
                    drop(g);
                    let _ = sys::notify_signal(notif, 1);
                }
                Ok(r) if r.label() == ENETDOWN as u32 => {
                    LINK_DOWN.fetch_or(1 << k, Ordering::Relaxed);
                    let _ = sys::notify_signal(notif, 1);
                }
                _ => time::sleep_ms(10),
            }
        }
        let _ = sys::handle_close(eth);
    });
    if r.is_err() {
        println!("[net] {}: no receive thread", LINKS[k].0);
    }
}

impl Net {
    fn new_tcp(&mut self) -> SocketHandle {
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_RX_BUF]),
            tcp::SocketBuffer::new(vec![0; TCP_TX_BUF]),
        );
        self.sockets.add(s)
    }

    fn insert(&mut self, s: Sock, owner: u32) -> u64 {
        let k = match self.table.iter().position(|x| x.is_none()) {
            Some(k) => k,
            None => {
                self.table.push(None);
                self.table.len() - 1
            }
        };
        self.table[k] = Some(s);
        if self.refs.len() < self.table.len() {
            self.refs.resize(self.table.len(), 0);
            self.owners.resize(self.table.len(), 0);
        }
        self.refs[k] = 1;
        self.owners[k] = owner;
        k as u64 + 1
    }

    /// Socket `id` exists and belongs to `pid`.
    fn owned(&self, id: u64, pid: u32) -> Result<(), i64> {
        self.sock(id)?;
        if self.owners[id as usize - 1] == pid {
            Ok(())
        } else {
            Err(-EBADF)
        }
    }

    /// Close sockets of dead processes; true if there were any.
    fn reap(&mut self) -> bool {
        self.since_reap = 0;
        let live = libnx::process::live_pids();
        let dead: Vec<u64> = (0..self.table.len())
            .filter(|&k| self.table[k].is_some() && !live.contains(&self.owners[k]))
            .map(|k| k as u64 + 1)
            .collect();
        for &id in &dead {
            self.refs[id as usize - 1] = 1;
            let _ = self.close(id);
        }
        !dead.is_empty()
    }

    fn sock(&self, id: u64) -> Result<&Sock, i64> {
        self.table
            .get((id as usize).wrapping_sub(1))
            .and_then(|s| s.as_ref())
            .ok_or(-EBADF)
    }

    fn tcp_handle(&self, id: u64) -> Result<SocketHandle, i64> {
        match self.sock(id)? {
            Sock::Tcp(h) => Ok(*h),
            _ => Err(-EINVAL),
        }
    }

    fn ephemeral(&mut self) -> u16 {
        let p = self.next_port;
        self.next_port = if p >= 65000 { 49152 } else { p + 1 };
        p
    }

    fn close(&mut self, id: u64) -> Result<(), i64> {
        let k = (id as usize).wrapping_sub(1);
        self.sock(id)?;
        if self.refs[k] > 1 {
            self.refs[k] -= 1;
            return Ok(());
        }
        self.refs[k] = 0;
        let s = self.table.get_mut(k).and_then(|s| s.take()).ok_or(-EBADF)?;
        match s {
            Sock::Tcp(h) => {
                // Clean close; the socket is recycled once closed.
                self.sockets.get_mut::<tcp::Socket>(h).close();
                self.closing.push(h);
            }
            Sock::Listener { pool, .. } => {
                for h in pool {
                    self.sockets.remove(h);
                }
            }
            Sock::Udp(h) => {
                self.sockets.remove(h);
            }
        }
        Ok(())
    }

    /// Client request: `Some` = immediate reply, `None` = pending.
    fn request(
        &mut self,
        m: &Msg,
        token: u64,
        pid: u32,
        buf: &mut [u8],
    ) -> Option<Result<Msg, i64>> {
        self.since_reap += 1;
        if self.since_reap >= 256 {
            self.reap();
        }
        if matches!(
            m.label(),
            proto::SEND
                | proto::RECV
                | proto::CLOSE
                | proto::SHUTDOWN
                | proto::SEND_TO
                | proto::RECV_FROM
                | proto::ADDRS
                | proto::TCP_ACCEPT
                | proto::DUP
        ) {
            if let Err(e) = self.owned(ipc::header(m, 0), pid) {
                return Some(Err(e));
            }
        }
        // Port busy, maybe by a dead process: reap and retry once.
        let r = self.request_inner(m, token, pid, buf);
        if matches!(r, Some(Err(e)) if e == -EADDRINUSE) && self.reap() {
            return self.request_inner(m, token, pid, buf);
        }
        r
    }

    fn request_inner(
        &mut self,
        m: &Msg,
        token: u64,
        pid: u32,
        buf: &mut [u8],
    ) -> Option<Result<Msg, i64>> {
        Some(match m.label() {
            proto::TCP_CONNECT => {
                let (addr, port, t) = (ip(m.mr[0]), m.mr[1] as u16, m.mr[2]);
                if self.addr.is_none() {
                    return Some(Err(-ENETUNREACH));
                }
                let h = self.new_tcp();
                let local = self.ephemeral();
                let cx = self.iface.context();
                if self
                    .sockets
                    .get_mut::<tcp::Socket>(h)
                    .connect(cx, (IpAddress::Ipv4(addr), port), local)
                    .is_err()
                {
                    self.sockets.remove(h);
                    return Some(Err(-EINVAL));
                }
                let id = self.insert(Sock::Tcp(h), pid);
                self.waits.push(Wait {
                    op: Op::Connect { sock: id as usize },
                    token,
                    deadline: deadline(t),
                });
                return None;
            }
            proto::TCP_LISTEN => {
                let (port, backlog) = (m.mr[0] as u16, (m.mr[1] as usize).clamp(1, 16));
                if port == 0 {
                    return Some(Err(-EINVAL));
                }
                if self
                    .table
                    .iter()
                    .flatten()
                    .any(|s| matches!(s, Sock::Listener { port: p, .. } if *p == port))
                {
                    return Some(Err(-EADDRINUSE));
                }
                let mut pool = Vec::new();
                for _ in 0..backlog {
                    let h = self.new_tcp();
                    let _ = self.sockets.get_mut::<tcp::Socket>(h).listen(port);
                    pool.push(h);
                }
                Ok(Msg::from_words(
                    0,
                    &[self.insert(Sock::Listener { port, pool }, pid)],
                ))
            }
            proto::TCP_ACCEPT => {
                if !matches!(self.sock(m.mr[0]), Ok(Sock::Listener { .. })) {
                    return Some(Err(-EINVAL));
                }
                self.waits.push(Wait {
                    op: Op::Accept {
                        listener: m.mr[0] as usize,
                    },
                    token,
                    deadline: deadline(m.mr[1]),
                });
                return None;
            }
            proto::SEND => {
                let (id, t) = (ipc::header(m, 0), ipc::header(m, 1));
                let n = ipc::unpack_at(m, 2, buf);
                if let Err(e) = self.tcp_handle(id) {
                    return Some(Err(e));
                }
                self.waits.push(Wait {
                    op: Op::Send {
                        sock: id as usize,
                        data: buf[..n].to_vec(),
                    },
                    token,
                    deadline: deadline(t),
                });
                return None;
            }
            proto::RECV => {
                if let Err(e) = self.tcp_handle(m.mr[0]) {
                    return Some(Err(e));
                }
                let max = (m.mr[1] as usize).clamp(1, MAX_IO);
                self.waits.push(Wait {
                    op: Op::Recv {
                        sock: m.mr[0] as usize,
                        max,
                    },
                    token,
                    deadline: deadline(m.mr[2]),
                });
                return None;
            }
            proto::CLOSE => self.close(m.mr[0]).map(|_| Msg::new(0, 0, 0)),
            proto::DUP => match self.sock(m.mr[0]) {
                Ok(_) => {
                    self.refs[m.mr[0] as usize - 1] += 1;
                    Ok(Msg::new(0, 0, 0))
                }
                Err(e) => Err(e),
            },
            proto::SHUTDOWN => match self.tcp_handle(m.mr[0]) {
                Ok(h) => {
                    self.sockets.get_mut::<tcp::Socket>(h).close();
                    Ok(Msg::new(0, 0, 0))
                }
                Err(e) => Err(e),
            },
            proto::UDP_BIND => {
                let port = if m.mr[0] == 0 {
                    self.ephemeral()
                } else {
                    m.mr[0] as u16
                };
                let meta = || vec![udp::PacketMetadata::EMPTY; 32];
                let s = udp::Socket::new(
                    udp::PacketBuffer::new(meta(), vec![0; UDP_BUF]),
                    udp::PacketBuffer::new(meta(), vec![0; UDP_BUF]),
                );
                let h = self.sockets.add(s);
                if self.sockets.get_mut::<udp::Socket>(h).bind(port).is_err() {
                    self.sockets.remove(h);
                    return Some(Err(-EADDRINUSE));
                }
                Ok(Msg::from_words(
                    0,
                    &[self.insert(Sock::Udp(h), pid), port as u64],
                ))
            }
            proto::SEND_TO => {
                let (id, addr, port) = (
                    ipc::header(m, 0),
                    ip(ipc::header(m, 1)),
                    ipc::header(m, 2) as u16,
                );
                let n = ipc::unpack_at(m, 3, buf);
                match self.sock(id) {
                    Ok(Sock::Udp(h)) => {
                        let h = *h;
                        let ep = IpEndpoint::new(IpAddress::Ipv4(addr), port);
                        match self
                            .sockets
                            .get_mut::<udp::Socket>(h)
                            .send_slice(&buf[..n], ep)
                        {
                            Ok(()) => Ok(Msg::from_words(0, &[n as u64])),
                            Err(_) => Err(-EAGAIN),
                        }
                    }
                    Ok(_) => Err(-EINVAL),
                    Err(e) => Err(e),
                }
            }
            proto::RECV_FROM => {
                if !matches!(self.sock(m.mr[0]), Ok(Sock::Udp(_))) {
                    return Some(Err(-EINVAL));
                }
                self.waits.push(Wait {
                    op: Op::RecvFrom {
                        sock: m.mr[0] as usize,
                    },
                    token,
                    deadline: deadline(m.mr[1]),
                });
                return None;
            }
            proto::RESOLVE => {
                let n = ipc::unpack(m, buf);
                let name = String::from(core::str::from_utf8(&buf[..n]).unwrap_or(""));
                // Literal address: no query.
                if let Some(a) = parse_ipv4(&name) {
                    return Some(Ok(Msg::from_words(0, &[u32::from(a) as u64])));
                }
                if self.dns_server.is_none() {
                    return Some(Err(-ENETUNREACH));
                }
                let cx = self.iface.context();
                match self.sockets.get_mut::<dns::Socket>(self.dns).start_query(
                    cx,
                    &name,
                    DnsQueryType::A,
                ) {
                    Ok(q) => {
                        self.waits.push(Wait {
                            op: Op::Resolve { query: q },
                            token,
                            deadline: deadline(5_000_000_000),
                        });
                        return None;
                    }
                    Err(_) => Err(-EINVAL),
                }
            }
            proto::PING => {
                if self.addr.is_none() {
                    return Some(Err(-ENETUNREACH));
                }
                let addr = m.mr[0] as u32;
                self.seq = self.seq.wrapping_add(1);
                let repr = Icmpv4Repr::EchoRequest {
                    ident: self.ident,
                    seq_no: self.seq,
                    data: &PING_DATA,
                };
                let s = self.sockets.get_mut::<icmp::Socket>(self.icmp);
                match s.send(repr.buffer_len(), IpAddress::Ipv4(ip(addr as u64))) {
                    Ok(b) => repr.emit(
                        &mut Icmpv4Packet::new_unchecked(b),
                        &ChecksumCapabilities::default(),
                    ),
                    Err(_) => return Some(Err(-EAGAIN)),
                }
                self.waits.push(Wait {
                    op: Op::Ping {
                        addr,
                        seq: self.seq,
                        sent: time::now_ns(),
                    },
                    token,
                    deadline: deadline(m.mr[1]),
                });
                return None;
            }
            proto::INFO => {
                let (a, p) = self
                    .addr
                    .map(|(a, p)| (u32::from(a) as u64, p as u64))
                    .unwrap_or((0, 0));
                let g = self.gateway.map(|g| u32::from(g) as u64).unwrap_or(0);
                let d = self.dns_server.map(|d| u32::from(d) as u64).unwrap_or(0);
                Ok(Msg::from_words(0, &[a, p, g, d, self.pref]))
            }
            proto::PREFER => match m.mr[0] {
                0 => Ok(Msg::from_words(0, &[self.pref])),
                v @ (proto::PREF_WIFI | proto::PREF_WIRED) => {
                    if v != self.pref {
                        println!(
                            "[net] links preferred: {}",
                            if v == proto::PREF_WIFI {
                                "Wi-Fi"
                            } else {
                                "wired"
                            }
                        );
                        (self.pref, self.retarget) = (v, true);
                    }
                    Ok(Msg::from_words(0, &[self.pref]))
                }
                _ => Err(-EINVAL),
            },
            proto::STATS => {
                let open = self.table.iter().filter(|s| s.is_some()).count();
                let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
                Ok(Msg::from_words(
                    0,
                    &[
                        n(&RX_FRAMES),
                        n(&RX_BYTES),
                        n(&TX_FRAMES),
                        n(&TX_BYTES),
                        open as u64,
                        self.link,
                    ],
                ))
            }
            proto::ADDRS => match self.tcp_handle(m.mr[0]) {
                Ok(h) => {
                    let s = self.sockets.get::<tcp::Socket>(h);
                    let l = s.local_endpoint();
                    let r = s.remote_endpoint();
                    Ok(Msg::from_words(
                        0,
                        &[
                            l.map(|e| word(e.addr)).unwrap_or(0),
                            l.map(|e| e.port as u64).unwrap_or(0),
                            r.map(|e| word(e.addr)).unwrap_or(0),
                            r.map(|e| e.port as u64).unwrap_or(0),
                        ],
                    ))
                }
                Err(e) => Err(e),
            },
            _ => Err(-EINVAL),
        })
    }

    /// Drive a pending operation forward: `Some` = reply to deliver.
    fn progress(&mut self, op: &mut Op, now: u64, deadline: u64) -> Option<Result<Msg, i64>> {
        let expired = now >= deadline;
        match op {
            Op::Connect { sock } => {
                let h = self.tcp_handle(*sock as u64).ok()?;
                let st = self.sockets.get::<tcp::Socket>(h).state();
                match st {
                    tcp::State::Established => Some(Ok(Msg::from_words(0, &[*sock as u64]))),
                    tcp::State::Closed => {
                        let _ = self.close(*sock as u64);
                        Some(Err(-ECONNREFUSED))
                    }
                    _ if expired => {
                        self.sockets.get_mut::<tcp::Socket>(h).abort();
                        let _ = self.close(*sock as u64);
                        Some(Err(-ETIMEDOUT))
                    }
                    _ => None,
                }
            }
            Op::Accept { listener } => {
                let id = *listener as u64;
                let (port, ready) = match self.sock(id).ok()? {
                    Sock::Listener { port, pool } => (
                        *port,
                        pool.iter().position(|&h| {
                            matches!(
                                self.sockets.get::<tcp::Socket>(h).state(),
                                tcp::State::Established | tcp::State::CloseWait
                            )
                        }),
                    ),
                    _ => return Some(Err(-EINVAL)),
                };
                match ready {
                    Some(k) => {
                        let fresh = self.new_tcp();
                        let _ = self.sockets.get_mut::<tcp::Socket>(fresh).listen(port);
                        let h = match self.table[*listener - 1].as_mut() {
                            Some(Sock::Listener { pool, .. }) => {
                                core::mem::replace(&mut pool[k], fresh)
                            }
                            _ => return Some(Err(-EBADF)),
                        };
                        let r = self.sockets.get::<tcp::Socket>(h).remote_endpoint();
                        let owner = self.owners[*listener - 1];
                        let sid = self.insert(Sock::Tcp(h), owner);
                        Some(Ok(Msg::from_words(
                            0,
                            &[
                                sid,
                                r.map(|e| word(e.addr)).unwrap_or(0),
                                r.map(|e| e.port as u64).unwrap_or(0),
                            ],
                        )))
                    }
                    None if expired => Some(Err(-ETIMEDOUT)),
                    None => None,
                }
            }
            Op::Recv { sock, max } => {
                let h = match self.tcp_handle(*sock as u64) {
                    Ok(h) => h,
                    Err(e) => return Some(Err(e)),
                };
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                if s.can_recv() {
                    let mut b = vec![0u8; *max];
                    return Some(match s.recv_slice(&mut b) {
                        Ok(n) => Ok(ipc::pack(0, &b[..n])),
                        Err(_) => Err(-ECONNRESET),
                    });
                }
                if !s.may_recv() {
                    // End of stream (FIN received) or done connection.
                    return Some(Ok(ipc::pack(0, &[])));
                }
                if expired {
                    return Some(Err(-ETIMEDOUT));
                }
                None
            }
            Op::Send { sock, data } => {
                let h = match self.tcp_handle(*sock as u64) {
                    Ok(h) => h,
                    Err(e) => return Some(Err(e)),
                };
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                if !s.may_send() {
                    return Some(Err(if s.state() == tcp::State::Closed {
                        -ENOTCONN
                    } else {
                        -EPIPE
                    }));
                }
                if s.can_send() {
                    return Some(match s.send_slice(data) {
                        Ok(n) => Ok(Msg::from_words(0, &[n as u64])),
                        Err(_) => Err(-EPIPE),
                    });
                }
                if expired {
                    return Some(Err(-ETIMEDOUT));
                }
                None
            }
            Op::RecvFrom { sock } => {
                let h = match self.sock(*sock as u64) {
                    Ok(Sock::Udp(h)) => *h,
                    _ => return Some(Err(-EBADF)),
                };
                let s = self.sockets.get_mut::<udp::Socket>(h);
                if s.can_recv() {
                    let mut b = vec![0u8; MAX_IO];
                    return Some(match s.recv_slice(&mut b) {
                        Ok((n, meta)) => Ok(ipc::pack_at(
                            0,
                            &[word(meta.endpoint.addr), meta.endpoint.port as u64],
                            &b[..n],
                        )),
                        Err(_) => Err(-EAGAIN),
                    });
                }
                if expired {
                    return Some(Err(-ETIMEDOUT));
                }
                None
            }
            Op::Resolve { query } => {
                match self
                    .sockets
                    .get_mut::<dns::Socket>(self.dns)
                    .get_query_result(*query)
                {
                    Ok(addrs) => Some(match addrs.first() {
                        Some(&a) => Ok(Msg::from_words(0, &[word(a)])),
                        None => Err(-ENOENT),
                    }),
                    Err(dns::GetQueryResultError::Pending) if expired => {
                        self.sockets
                            .get_mut::<dns::Socket>(self.dns)
                            .cancel_query(*query);
                        Some(Err(-ETIMEDOUT))
                    }
                    Err(dns::GetQueryResultError::Pending) => None,
                    Err(_) => Some(Err(-ENOENT)),
                }
            }
            Op::Ping { addr, seq, sent } => {
                match self.pongs.iter().position(|p| p.0 == *addr && p.1 == *seq) {
                    Some(k) => {
                        let (_, _, at) = self.pongs.remove(k)?;
                        Some(Ok(Msg::from_words(0, &[at.saturating_sub(*sent)])))
                    }
                    None if expired => Some(Err(-ETIMEDOUT)),
                    None => None,
                }
            }
        }
    }

    /// Echo replies to our requests (our identifier), kept with the time
    /// they came for the pings that wait on them.
    fn icmp_events(&mut self) {
        let s = self.sockets.get_mut::<icmp::Socket>(self.icmp);
        while let Ok((data, from)) = s.recv() {
            let Ok(p) = Icmpv4Packet::new_checked(data) else {
                continue;
            };
            if let Ok(Icmpv4Repr::EchoReply { ident, seq_no, .. }) =
                Icmpv4Repr::parse(&p, &ChecksumCapabilities::default())
            {
                if ident == self.ident {
                    if self.pongs.len() == PONGS_MAX {
                        self.pongs.pop_front();
                    }
                    self.pongs
                        .push_back((word(from) as u32, seq_no, time::now_ns()));
                }
            }
        }
    }

    /// Look links up, ask their carriers, and run on the best one.
    fn check_links(&mut self, now: u64) {
        let down = LINK_DOWN.swap(0, Ordering::Relaxed);
        for (k, &(name, _)) in LINKS.iter().enumerate() {
            let l = &mut self.links[k];
            if l.handle == 0 {
                let Ok(h) = ns::lookup(name) else {
                    (l.carrier, l.skipped_at, l.skips) = (false, 0, 0);
                    continue;
                };
                l.handle = h;
                let stop = Arc::new(AtomicBool::new(false));
                l.rx_stop = Some(stop.clone());
                receive(k, h, self.rxq.clone(), self.notif, stop);
            }
            let (mac, carrier) =
                match ipc::call_timeout(l.handle, &Msg::new(eth::INFO, 0, 0), LINK_CALL_NS) {
                    Ok(m) if m.label() == 0 => {
                        let mut mac = [0u8; 6];
                        mac.copy_from_slice(&m.mr[0].to_le_bytes()[..6]);
                        (mac, m.words() < 3 || m.mr[2] != 0)
                    }
                    // A driver busy for seconds (a Wi-Fi scan) is not a link
                    // that went down: it keeps what it had.
                    Err(e) if e == -ETIMEDOUT => (l.mac, l.carrier),
                    // Its driver died (and may be back under a new endpoint):
                    // the handle is looked up again at the next check.
                    Err(_) => {
                        if let Some(s) = l.rx_stop.take() {
                            s.store(true, Ordering::Relaxed);
                        }
                        l.handle = 0;
                        (l.mac, false)
                    }
                    _ => (l.mac, false),
                };
            let carrier = carrier && down & (1 << k) == 0;
            if !carrier {
                (l.skipped_at, l.skips) = (0, 0);
            } else if !l.carrier {
                l.up_since = now;
            }
            (l.mac, l.carrier) = (mac, carrier);
        }
        if down & self.active.map_or(0, |k| 1 << k) != 0 {
            println!("[net] link down: DHCP starts over");
            self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).reset();
            self.since = now;
        }
        // A link without an address (a PC that does not share its
        // connection, a network without DHCP) gives way to another one.
        if let Some(k) = self.active {
            let others = (0..LINKS.len()).any(|j| j != k && self.links[j].carrier);
            if self.addr.is_none() && others && now > self.since + DHCP_GIVE_UP_NS {
                println!("[net] {}: no DHCP answer: moving on", LINKS[k].0);
                self.links[k].skipped_at = now;
                self.links[k].skips += 1;
            }
        }
        // Each failure doubles the wait, up to 16 times the first.
        let usable = |l: &Link| {
            let wait = SKIP_RETRY_NS << l.skips.saturating_sub(1).min(4);
            l.carrier && (l.skipped_at == 0 || now >= l.skipped_at + wait)
        };
        let order = if self.pref == proto::PREF_WIFI {
            ORDER_WIFI
        } else {
            ORDER_WIRED
        };
        let best = order.into_iter().find(|&k| usable(&self.links[k]));
        // The link in use keeps its place until a better one has settled.
        let hold = match (self.active, best) {
            (Some(a), Some(b)) => {
                a != b
                    && usable(&self.links[a])
                    && !self.retarget
                    && now < self.links[b].up_since + SETTLE_NS
            }
            _ => false,
        };
        self.retarget = false;
        if !hold && best != self.active {
            self.switch(best, now);
        }
    }

    /// Leave the network in use (address, routes, connections) for link
    /// `to`: its MAC, DHCP from scratch.
    fn switch(&mut self, to: Option<usize>, now: u64) {
        self.iface.update_ip_addrs(|a| a.clear());
        self.iface.routes_mut().remove_default_ipv4_route();
        (self.addr, self.gateway, self.dns_server) = (None, None, None);
        let tcp: Vec<SocketHandle> = self
            .table
            .iter()
            .filter_map(|s| match s {
                Some(Sock::Tcp(h)) => Some(*h),
                _ => None,
            })
            .collect();
        for h in tcp {
            self.sockets.get_mut::<tcp::Socket>(h).abort();
        }
        self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).reset();
        self.dev.rx.lock().clear();
        match to {
            Some(k) => {
                let mac = EthernetAddress(self.links[k].mac);
                self.iface.set_hardware_addr(HardwareAddress::Ethernet(mac));
                (self.dev.eth, self.dev.link, self.link) = (self.links[k].handle, k, LINKS[k].1);
                println!("[net] link {} ({})", LINKS[k].0, mac);
            }
            None => {
                (self.dev.eth, self.link) = (0, 0);
                println!("[net] no link");
            }
        }
        (self.active, self.since) = (to, now);
    }

    fn dhcp_events(&mut self) {
        let ev = match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            Some(dhcpv4::Event::Configured(c)) => {
                Some((c.address, c.router, c.dns_servers.first().copied()))
            }
            Some(dhcpv4::Event::Deconfigured) => None,
            None => return,
        };
        match ev {
            Some((cidr, router, dns_server)) => {
                self.iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(cidr));
                });
                match router {
                    Some(r) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(r);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                if let Some(d) = dns_server {
                    self.sockets
                        .get_mut::<dns::Socket>(self.dns)
                        .update_servers(&[IpAddress::Ipv4(d)]);
                }
                self.addr = Some((cidr.address(), cidr.prefix_len()));
                if let Some(k) = self.active {
                    self.links[k].skips = 0;
                }
                self.gateway = router;
                self.dns_server = dns_server;
                println!(
                    "[net] DHCP: address {}/{}, gateway {}, DNS {}",
                    cidr.address(),
                    cidr.prefix_len(),
                    router
                        .map(|r| alloc::format!("{}", r))
                        .unwrap_or_else(|| String::from("none")),
                    dns_server
                        .map(|r| alloc::format!("{}", r))
                        .unwrap_or_else(|| String::from("none"))
                );
            }
            None => {
                self.iface.update_ip_addrs(|a| a.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                if self.addr.take().is_some() {
                    println!("[net] DHCP: lease lost");
                    // The link gets its time to answer again before it gives way.
                    self.since = time::now_ns();
                }
            }
        }
    }
}

fn parse_ipv4(s: &str) -> Option<Ipv4Address> {
    let mut o = [0u8; 4];
    let mut n = 0;
    for p in s.split('.') {
        if n == 4 {
            return None;
        }
        o[n] = p.parse().ok()?;
        n += 1;
    }
    (n == 4).then(|| Ipv4Address::new(o[0], o[1], o[2], o[3]))
}

fn now() -> Instant {
    Instant::from_micros((time::now_ns() / 1000) as i64)
}

libnx::entry!(main);

/// Longest wait on a link driver's `INFO`.
const LINK_CALL_NS: u64 = 50_000_000;

fn main() -> i32 {
    // Frames of every link land here; the receive threads signal.
    let rxq: Frames = Arc::new(Mutex::new(VecDeque::new()));
    let notif = sys::notify_create().expect("net: notification");
    let _ = sys::notify_bind(notif);

    // No link yet: a placeholder address until one is chosen.
    let mut dev = Dev {
        eth: 0,
        link: 0,
        rx: rxq.clone(),
    };
    let mac = EthernetAddress([0x02, 0, 0, 0, 0, 0]);
    let mut cfg = Config::new(HardwareAddress::Ethernet(mac));
    let mut seed = [0u8; 8];
    sys::getrandom(&mut seed);
    cfg.random_seed = u64::from_le_bytes(seed);
    let iface = Interface::new(cfg, &mut dev, now());
    let mut sockets = SocketSet::new(Vec::new());
    let mut client = dhcpv4::Socket::new();
    let mut retry = dhcpv4::RetryConfig::default();
    retry.discover_timeout = DHCP_RETRY;
    retry.initial_request_timeout = DHCP_RETRY;
    client.set_retry_config(retry);
    let dhcp = sockets.add(client);
    let dns = sockets.add(dns::Socket::new(
        &[],
        vec![None, None, None, None, None, None, None, None],
    ));
    let mut ping = icmp::Socket::new(
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
    );
    let ident = u16::from_le_bytes([seed[2], seed[3]]);
    ping.bind(icmp::Endpoint::Ident(ident))
        .expect("net: ICMP socket");
    let icmp = sockets.add(ping);
    let mut net = Net {
        iface,
        dev,
        sockets,
        dhcp,
        dns,
        table: Vec::new(),
        refs: Vec::new(),
        owners: Vec::new(),
        since_reap: 0,
        waits: Vec::new(),
        next_port: 49152 + (u16::from_le_bytes([seed[0], seed[1]]) % 10000),
        addr: None,
        gateway: None,
        dns_server: None,
        closing: Vec::new(),
        icmp,
        ident,
        seq: 0,
        pongs: VecDeque::new(),
        link: 0,
        links: Default::default(),
        active: None,
        since: 0,
        pref: proto::PREF_WIFI,
        retarget: false,
        rxq,
        notif,
    };
    let ep = drv::publish("net").expect("net: publish");
    let mut buf = vec![0u8; ipc::MAX_BYTES];
    let mut next_check = 0;
    loop {
        let now_ns = time::now_ns();
        if now_ns >= next_check || net.retarget || LINK_DOWN.load(Ordering::Relaxed) != 0 {
            net.check_links(now_ns);
            next_check = now_ns + LINK_CHECK_NS;
        }
        // Without a link the stack would only retry sends it cannot make.
        let linked = net.dev.eth != 0;
        if linked {
            net.iface.poll(now(), &mut net.dev, &mut net.sockets);
            net.dhcp_events();
            net.icmp_events();
        }

        // Pending operations.
        let now_ns = time::now_ns();
        let mut k = 0;
        while k < net.waits.len() {
            let mut w = net.waits.swap_remove(k);
            match net.progress(&mut w.op, now_ns, w.deadline) {
                Some(r) => {
                    let _ = ipc::reply_to(w.token, &r.unwrap_or_else(Msg::error));
                    // Data may have been queued: push it out.
                    net.iface.poll(now(), &mut net.dev, &mut net.sockets);
                }
                None => {
                    net.waits.push(w);
                    let last = net.waits.len() - 1;
                    net.waits.swap(k, last);
                    k += 1;
                }
            }
        }
        // Sockets closed by their client: recycled once done.
        net.closing.retain(|&h| {
            let done = net.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed;
            if done {
                net.sockets.remove(h);
            }
            !done
        });

        // Next deadline from stack timers, client timeouts and the link
        // check.
        let mut delay = if linked {
            net.iface
                .poll_delay(now(), &net.sockets)
                .map(|d| d.total_micros() * 1000)
        } else {
            None
        };
        let left = next_check.saturating_sub(time::now_ns());
        delay = Some(delay.map_or(left, |x| x.min(left)));
        if let Some(d) = net
            .waits
            .iter()
            .map(|w| w.deadline)
            .filter(|&d| d != u64::MAX)
            .min()
        {
            let left = d.saturating_sub(time::now_ns());
            delay = Some(delay.map_or(left, |x| x.min(left)));
        }
        let delay = delay.map_or(0, |d| d.max(1));
        match ipc::recv_timeout(ep, delay) {
            Ok(rx) if rx.msg.is_notify() => {}
            Ok(rx) => {
                if let Some(r) = net.request(&rx.msg, rx.token, rx.pid, &mut buf) {
                    let _ = ipc::reply(&r.unwrap_or_else(Msg::error));
                }
            }
            Err(_) => {}
        }
    }
}

//! net: TCP/IP stack (smoltcp) over "eth0" (`net` protocol).
//! DHCP/DNS plus deferred replies for connect/accept/receive/resolve.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use libnx::abi::err::{
    EADDRINUSE, EAGAIN, EBADF, ECONNREFUSED, ECONNRESET, EINVAL, ENETUNREACH, ENOENT, ENOTCONN,
    EPIPE, ETIMEDOUT,
};
use libnx::abi::proto::{eth, net as proto};
use libnx::drv;
use libnx::ipc::{self, Msg};
use libnx::sync::Mutex;
use libnx::{ns, println, sys, thread, time};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, dns, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address,
};

const TCP_BUF: usize = 64 * 1024;
const UDP_BUF: usize = 64 * 1024;
/// Data bytes per reply message.
const MAX_IO: usize = 3 * 1024;

// ------------------------------------------------------------ interface ---

struct Dev {
    eth: u32,
    rx: Arc<Mutex<VecDeque<Vec<u8>>>>,
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
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        // Driver TX ring full: let what is there drain.
        for _ in 0..100 {
            match ipc::call(self.0, &ipc::pack(eth::SEND, &b)) {
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
        let f = self.rx.lock().pop_front()?;
        Some((Rx(f), Tx(self.eth)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx> {
        Some(Tx(self.eth))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
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

fn words(w: &[u64]) -> Msg {
    let mut m = Msg::new(0, w.len(), 0);
    m.mr[..w.len()].copy_from_slice(w);
    m
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
}

impl Net {
    fn new_tcp(&mut self) -> SocketHandle {
        let s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_BUF]),
            tcp::SocketBuffer::new(vec![0; TCP_BUF]),
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
                Ok(words(&[self.insert(Sock::Listener { port, pool }, pid)]))
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
                Ok(words(&[self.insert(Sock::Udp(h), pid), port as u64]))
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
                            Ok(()) => Ok(words(&[n as u64])),
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
                    return Some(Ok(words(&[u32::from(a) as u64])));
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
            proto::INFO => {
                let (a, p) = self
                    .addr
                    .map(|(a, p)| (u32::from(a) as u64, p as u64))
                    .unwrap_or((0, 0));
                let g = self.gateway.map(|g| u32::from(g) as u64).unwrap_or(0);
                let d = self.dns_server.map(|d| u32::from(d) as u64).unwrap_or(0);
                Ok(words(&[a, p, g, d]))
            }
            proto::ADDRS => match self.tcp_handle(m.mr[0]) {
                Ok(h) => {
                    let s = self.sockets.get::<tcp::Socket>(h);
                    let l = s.local_endpoint();
                    let r = s.remote_endpoint();
                    Ok(words(&[
                        l.map(|e| word(e.addr)).unwrap_or(0),
                        l.map(|e| e.port as u64).unwrap_or(0),
                        r.map(|e| word(e.addr)).unwrap_or(0),
                        r.map(|e| e.port as u64).unwrap_or(0),
                    ]))
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
                    tcp::State::Established => Some(Ok(words(&[*sock as u64]))),
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
                        Some(Ok(words(&[
                            sid,
                            r.map(|e| word(e.addr)).unwrap_or(0),
                            r.map(|e| e.port as u64).unwrap_or(0),
                        ])))
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
                        Ok(n) => Ok(words(&[n as u64])),
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
                        Some(&a) => Ok(words(&[word(a)])),
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
        }
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

/// Interface to run on. The wired link comes first: the USB driver
/// may still be enumerating. Failing that, wait for the Wi-Fi station,
/// which publishes "wlan0" the moment it associates. Both waits block
/// in the name service: an idle core stays idle.
fn link() -> Result<u32, i64> {
    if let Ok(h) = ns::wait_for_timeout("eth0", 3_000_000_000) {
        return Ok(h);
    }
    println!("[net] no wired link, waiting for Wi-Fi");
    ns::wait_for("wlan0")
}

fn main() -> i32 {
    let eth = match link() {
        Ok(h) => h,
        Err(_) => {
            println!("[net] no network interface");
            return 0;
        }
    };
    let info = ipc::call(eth, &Msg::new(eth::INFO, 0, 0)).expect("net: interface");
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&info.mr[0].to_le_bytes()[..6]);

    // Receive: one thread blocks on the driver, the main one is signaled.
    let rxq: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let notif = sys::notify_create().expect("net: notification");
    let _ = sys::notify_bind(notif);
    {
        let (q, eth2) = (
            rxq.clone(),
            sys::handle_dup(eth, libnx::abi::rights::SEND, 0).unwrap_or(eth),
        );
        thread::spawn(move || {
            let mut frame = vec![0u8; ipc::MAX_BYTES];
            loop {
                let mut m = Msg::new(eth::RECV, 1, 0);
                m.mr[0] = 0;
                match ipc::call(eth2, &m) {
                    Ok(r) if r.label() == 0 => {
                        let n = ipc::unpack(&r, &mut frame);
                        let mut g = q.lock();
                        if g.len() < 256 {
                            g.push_back(frame[..n].to_vec());
                        }
                        drop(g);
                        let _ = sys::notify_signal(notif, 1);
                    }
                    _ => time::sleep_ms(10),
                }
            }
        })
        .expect("net: receive thread");
    }

    let mut dev = Dev { eth, rx: rxq };
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut seed = [0u8; 8];
    sys::getrandom(&mut seed);
    cfg.random_seed = u64::from_le_bytes(seed);
    let iface = Interface::new(cfg, &mut dev, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let dns = sockets.add(dns::Socket::new(
        &[],
        vec![None, None, None, None, None, None, None, None],
    ));
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
    };
    let ep = drv::publish("net").expect("net: publish");
    let mut buf = vec![0u8; ipc::MAX_BYTES];
    loop {
        let t = now();
        net.iface.poll(t, &mut net.dev, &mut net.sockets);
        net.dhcp_events();

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

        // Next deadline from stack timers and client timeouts (else wait).
        let mut delay = net
            .iface
            .poll_delay(now(), &net.sockets)
            .map(|d| d.total_micros() * 1000);
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

//! Client of the `net` service (smoltcp; TCP/UDP/DNS over IPv4); sockets are service ids.
//! Blocking calls complete later (deferred reply); nonblocking is a zero timeout (`EAGAIN`).

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, ToSocketAddrs};
use crate::sync::Arc;
use crate::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::sys::pal::abi::err::{EADDRINUSE, EAGAIN, EMSGSIZE, ENOTCONN, ETIMEDOUT};
use crate::sys::pal::abi::proto::net as proto;
use crate::sys::pal::abi::timeout;
use crate::sys::pal::ipc::{self, Msg};
use crate::sys::pal::{svc, timeout_ns, unsupported};
use crate::time::Duration;

/// Bytes per message (the service takes/returns no more).
const CHUNK: usize = 3 * 1024;

fn call(m: &Msg) -> io::Result<Msg> {
    svc::call(svc::net()?, m)
}

fn v4(a: &SocketAddr) -> io::Result<SocketAddrV4> {
    match a {
        SocketAddr::V4(a) => Ok(*a),
        SocketAddr::V6(_) => Err(io::const_error!(io::ErrorKind::Unsupported, "no IPv6")),
    }
}

fn addr(ip: u64, port: u64) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip as u32), port as u16))
}

/// Operation timeout: nonblocking, set timeout, or none.
struct Timeouts {
    read: AtomicU64,
    write: AtomicU64,
    nonblocking: AtomicBool,
}

impl Timeouts {
    const fn new() -> Timeouts {
        Timeouts { read: AtomicU64::new(0), write: AtomicU64::new(0), nonblocking: AtomicBool::new(false) }
    }

    fn get(&self, slot: &AtomicU64) -> u64 {
        if self.nonblocking.load(Ordering::Relaxed) { timeout::POLL } else { slot.load(Ordering::Relaxed) }
    }

    fn set(slot: &AtomicU64, d: Option<Duration>) -> io::Result<()> {
        if d == Some(Duration::ZERO) {
            return Err(io::Error::ZERO_TIMEOUT);
        }
        slot.store(timeout_ns(d), Ordering::Relaxed);
        Ok(())
    }

    fn read_of(slot: &AtomicU64) -> Option<Duration> {
        match slot.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Duration::from_nanos(ns)),
        }
    }
}

/// An expired timeout becomes `WouldBlock`, like `EAGAIN` on Unix.
fn would_block(e: io::Error) -> io::Error {
    if e.raw_os_error() == Some(ETIMEDOUT as i32) { io::Error::from_raw_os_error(EAGAIN as i32) } else { e }
}

// ------------------------------------------------------------------- TCP ---

/// Socket settings, shared by clones (`try_clone`) like a Unix socket.
struct StreamState {
    t: Timeouts,
    nodelay: AtomicBool,
}

pub struct TcpStream {
    id: u64,
    st: Arc<StreamState>,
}

impl TcpStream {
    fn from_id(id: u64) -> TcpStream {
        TcpStream { id, st: Arc::new(StreamState { t: Timeouts::new(), nodelay: AtomicBool::new(false) }) }
    }

    fn connect_one(a: &SocketAddr, t: u64) -> io::Result<TcpStream> {
        let a = v4(a)?;
        let m = call(&Msg::from_words(proto::TCP_CONNECT, &[u32::from(*a.ip()) as u64, a.port() as u64, t]))?;
        Ok(TcpStream::from_id(m.mr[0]))
    }

    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
        super::each_addr(addr, |a| TcpStream::connect_one(a, timeout::INFINITE))
    }

    pub fn connect_timeout(a: &SocketAddr, t: Duration) -> io::Result<TcpStream> {
        if t.is_zero() {
            return Err(io::Error::ZERO_TIMEOUT);
        }
        TcpStream::connect_one(a, timeout_ns(Some(t)))
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        Timeouts::set(&self.st.t.read, d)
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        Timeouts::set(&self.st.t.write, d)
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(Timeouts::read_of(&self.st.t.read))
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(Timeouts::read_of(&self.st.t.write))
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(CHUNK);
        if len == 0 {
            return Ok(0);
        }
        let t = self.st.t.get(&self.st.t.read);
        let m = call(&Msg::from_words(proto::RECV, &[self.id, len as u64, t])).map_err(would_block)?;
        Ok(ipc::unpack(&m, &mut buf[..len]))
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        crate::io::default_read_buf(|b| self.read(b), cursor)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        crate::io::default_read_vectored(|b| self.read(b), bufs)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let chunk = &buf[..buf.len().min(CHUNK)];
        if chunk.is_empty() {
            return Ok(0);
        }
        let t = self.st.t.get(&self.st.t.write);
        let m = call(&ipc::pack_at(proto::SEND, &[self.id, t], chunk).0).map_err(would_block)?;
        Ok(m.mr[0] as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        crate::io::default_write_vectored(|b| self.write(b), bufs)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    fn addrs(&self) -> io::Result<Msg> {
        call(&Msg::from_words(proto::ADDRS, &[self.id]))
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.addrs().map(|m| addr(m.mr[2], m.mr[3]))
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        self.addrs().map(|m| addr(m.mr[0], m.mr[1]))
    }

    /// Only send-side shutdown exists (FIN); read shutdown is a no-op.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match how {
            Shutdown::Read => Ok(()),
            Shutdown::Write | Shutdown::Both => call(&Msg::from_words(proto::SHUTDOWN, &[self.id])).map(|_| ()),
        }
    }

    pub fn duplicate(&self) -> io::Result<TcpStream> {
        call(&Msg::from_words(proto::DUP, &[self.id]))?;
        Ok(TcpStream { id: self.id, st: self.st.clone() })
    }

    pub fn set_linger(&self, _: Option<Duration>) -> io::Result<()> {
        unsupported()
    }

    pub fn linger(&self) -> io::Result<Option<Duration>> {
        unsupported()
    }

    pub fn set_keepalive(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn keepalive(&self) -> io::Result<bool> {
        unsupported()
    }

    /// Hint only: the service sends as soon as possible.
    pub fn set_nodelay(&self, v: bool) -> io::Result<()> {
        self.st.nodelay.store(v, Ordering::Relaxed);
        Ok(())
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        Ok(self.st.nodelay.load(Ordering::Relaxed))
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, v: bool) -> io::Result<()> {
        self.st.t.nonblocking.store(v, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = call(&Msg::from_words(proto::CLOSE, &[self.id]));
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("TcpStream");
        if let Ok(a) = self.socket_addr() {
            d.field("addr", &a);
        }
        if let Ok(p) = self.peer_addr() {
            d.field("peer", &p);
        }
        d.field("id", &self.id).finish()
    }
}

pub struct TcpListener {
    id: u64,
    local: SocketAddrV4,
    nonblocking: Arc<AtomicBool>,
}

impl TcpListener {
    fn bind_one(a: &SocketAddr) -> io::Result<TcpListener> {
        let a = v4(a)?;
        let listen = |port: u16| call(&Msg::from_words(proto::TCP_LISTEN, &[port as u64, 128]));
        let (m, port) = if a.port() != 0 {
            (listen(a.port())?, a.port())
        } else {
            // Ephemeral port: the service picks none for listening.
            let mut seed = [0u8; 2];
            crate::sys::random::fill_bytes(&mut seed);
            let mut p = 49152 + (u16::from_le_bytes(seed) % 16384);
            let mut tries = 0;
            loop {
                match listen(p) {
                    Ok(m) => break (m, p),
                    Err(e) if e.raw_os_error() == Some(EADDRINUSE as i32) && tries < 64 => {
                        tries += 1;
                        p = if p == 65535 { 49152 } else { p + 1 };
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        Ok(TcpListener { id: m.mr[0], local: SocketAddrV4::new(*a.ip(), port), nonblocking: Arc::new(AtomicBool::new(false)) })
    }

    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<TcpListener> {
        super::each_addr(addr, TcpListener::bind_one)
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(self.local))
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let t = if self.nonblocking.load(Ordering::Relaxed) { timeout::POLL } else { timeout::INFINITE };
        let m = call(&Msg::from_words(proto::TCP_ACCEPT, &[self.id, t])).map_err(would_block)?;
        Ok((TcpStream::from_id(m.mr[0]), addr(m.mr[1], m.mr[2])))
    }

    pub fn duplicate(&self) -> io::Result<TcpListener> {
        call(&Msg::from_words(proto::DUP, &[self.id]))?;
        Ok(TcpListener { id: self.id, local: self.local, nonblocking: self.nonblocking.clone() })
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_only_v6(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn only_v6(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, v: bool) -> io::Result<()> {
        self.nonblocking.store(v, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let _ = call(&Msg::from_words(proto::CLOSE, &[self.id]));
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpListener").field("addr", &self.local).field("id", &self.id).finish()
    }
}

// ------------------------------------------------------------------- UDP ---

struct UdpState {
    /// `connect` peer: 1 << 63 | addr << 16 | port (0: none).
    peer: AtomicU64,
    t: Timeouts,
}

pub struct UdpSocket {
    id: u64,
    local: SocketAddrV4,
    st: Arc<UdpState>,
}

impl UdpSocket {
    fn bind_one(a: &SocketAddr) -> io::Result<UdpSocket> {
        let a = v4(a)?;
        let m = call(&Msg::from_words(proto::UDP_BIND, &[a.port() as u64]))?;
        Ok(UdpSocket {
            id: m.mr[0],
            local: SocketAddrV4::new(*a.ip(), m.mr[1] as u16),
            st: Arc::new(UdpState { peer: AtomicU64::new(0), t: Timeouts::new() }),
        })
    }

    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<UdpSocket> {
        super::each_addr(addr, UdpSocket::bind_one)
    }

    fn peer(&self) -> Option<SocketAddrV4> {
        let p = self.st.peer.load(Ordering::Relaxed);
        (p != 0).then(|| SocketAddrV4::new(Ipv4Addr::from((p >> 16) as u32), p as u16))
    }

    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer().map(SocketAddr::V4).ok_or_else(|| io::Error::from_raw_os_error(ENOTCONN as i32))
    }

    pub fn socket_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::V4(self.local))
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let t = self.st.t.get(&self.st.t.read);
        let m = call(&Msg::from_words(proto::RECV_FROM, &[self.id, t])).map_err(would_block)?;
        let n = ipc::unpack_at(&m, 2, buf);
        Ok((n, addr(ipc::header(&m, 0), ipc::header(&m, 1))))
    }

    pub fn peek_from(&self, _: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsupported()
    }

    pub fn send_to(&self, buf: &[u8], dst: &SocketAddr) -> io::Result<usize> {
        let d = v4(dst)?;
        let (m, n) = ipc::pack_at(proto::SEND_TO, &[self.id, u32::from(*d.ip()) as u64, d.port() as u64], buf);
        if n < buf.len() {
            return Err(io::Error::from_raw_os_error(EMSGSIZE as i32));
        }
        call(&m).map(|r| r.mr[0] as usize)
    }

    pub fn duplicate(&self) -> io::Result<UdpSocket> {
        call(&Msg::from_words(proto::DUP, &[self.id]))?;
        Ok(UdpSocket { id: self.id, local: self.local, st: self.st.clone() })
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        Timeouts::set(&self.st.t.read, d)
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        Timeouts::set(&self.st.t.write, d)
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(Timeouts::read_of(&self.st.t.read))
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(Timeouts::read_of(&self.st.t.write))
    }

    pub fn set_broadcast(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn broadcast(&self) -> io::Result<bool> {
        Ok(false)
    }

    pub fn set_multicast_loop_v4(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v4(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn set_multicast_ttl_v4(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_ttl_v4(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn set_multicast_loop_v6(&self, _: bool) -> io::Result<()> {
        unsupported()
    }

    pub fn multicast_loop_v6(&self) -> io::Result<bool> {
        unsupported()
    }

    pub fn join_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn join_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v4(&self, _: &Ipv4Addr, _: &Ipv4Addr) -> io::Result<()> {
        unsupported()
    }

    pub fn leave_multicast_v6(&self, _: &Ipv6Addr, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn set_ttl(&self, _: u32) -> io::Result<()> {
        unsupported()
    }

    pub fn ttl(&self) -> io::Result<u32> {
        unsupported()
    }

    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(None)
    }

    pub fn set_nonblocking(&self, v: bool) -> io::Result<()> {
        self.st.t.nonblocking.store(v, Ordering::Relaxed);
        Ok(())
    }

    /// Datagram from the peer (others dropped, as on Unix).
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let (n, from) = self.recv_from(buf)?;
            match self.peer() {
                Some(p) if SocketAddr::V4(p) != from => continue,
                _ => return Ok(n),
            }
        }
    }

    pub fn peek(&self, _: &mut [u8]) -> io::Result<usize> {
        unsupported()
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let p = self.peer().ok_or_else(|| io::Error::from_raw_os_error(ENOTCONN as i32))?;
        self.send_to(buf, &SocketAddr::V4(p))
    }

    pub fn connect<A: ToSocketAddrs>(&self, addr: A) -> io::Result<()> {
        super::each_addr(addr, |a| {
            let a = v4(a)?;
            let packed = 1 << 63 | (u32::from(*a.ip()) as u64) << 16 | a.port() as u64;
            self.st.peer.store(packed, Ordering::Relaxed);
            Ok(())
        })
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = call(&Msg::from_words(proto::CLOSE, &[self.id]));
    }
}

impl fmt::Debug for UdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UdpSocket").field("addr", &self.local).field("id", &self.id).finish()
    }
}

// ------------------------------------------------------------------- DNS ---

/// The service resolves one address per name.
pub struct LookupHost {
    addr: Option<SocketAddr>,
}

impl Iterator for LookupHost {
    type Item = SocketAddr;
    fn next(&mut self) -> Option<SocketAddr> {
        self.addr.take()
    }
}

pub fn lookup_host(host: &str, port: u16) -> io::Result<LookupHost> {
    let (m, n) = ipc::pack(proto::RESOLVE, host.as_bytes());
    if n < host.len() {
        return Err(io::const_error!(io::ErrorKind::InvalidInput, "hostname too long"));
    }
    let r = call(&m)?;
    Ok(LookupHost { addr: Some(addr(r.mr[0], port as u64)) })
}

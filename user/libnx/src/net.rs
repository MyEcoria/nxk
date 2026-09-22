//! Network client (TCP/UDP/DNS over IPv4).
//! Timeouts in ns, 0 = no limit.

use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::err::ENOSYS;
use nxk_abi::proto::fs::MAX_IO as CHUNK;
use nxk_abi::proto::net as proto;

use crate::ipc::{self, Msg};
use crate::ns;

static NET: AtomicU32 = AtomicU32::new(0);

fn net() -> Result<u32, i64> {
    let h = NET.load(Ordering::Relaxed);
    if h != 0 {
        return Ok(h);
    }
    let h = ns::wait_for_timeout("net", 3_000_000_000).map_err(|_| -ENOSYS)?;
    NET.store(h, Ordering::Relaxed);
    Ok(h)
}

fn call(m: &Msg) -> Result<Msg, i64> {
    ipc::call_checked(net()?, m)
}

fn words(label: u32, w: &[u64]) -> Msg {
    let mut m = Msg::new(label, w.len(), 0);
    m.mr[..w.len()].copy_from_slice(w);
    m
}

pub const fn ipv4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) << 24 | (b as u32) << 16 | (c as u32) << 8 | d as u32
}

/// Human-readable "a.b.c.d" address.
pub fn fmt_ipv4(a: u32) -> alloc::string::String {
    alloc::format!(
        "{}.{}.{}.{}",
        a >> 24,
        (a >> 16) & 0xFF,
        (a >> 8) & 0xFF,
        a & 0xFF
    )
}

/// Address of a hostname (or a dotted-decimal address).
pub fn resolve(name: &str) -> Result<u32, i64> {
    call(&ipc::pack(proto::RESOLVE, name.as_bytes())).map(|m| m.mr[0] as u32)
}

/// Config: (address, prefix length, gateway, DNS).
pub fn info() -> Result<(u32, u8, u32, u32), i64> {
    let m = call(&Msg::new(proto::INFO, 0, 0))?;
    Ok((
        m.mr[0] as u32,
        m.mr[1] as u8,
        m.mr[2] as u32,
        m.mr[3] as u32,
    ))
}

pub struct TcpStream {
    id: u64,
    timeout: u64,
}

impl TcpStream {
    pub fn connect(addr: u32, port: u16, timeout_ns: u64) -> Result<Self, i64> {
        let m = call(&words(
            proto::TCP_CONNECT,
            &[addr as u64, port as u64, timeout_ns],
        ))?;
        Ok(Self {
            id: m.mr[0],
            timeout: 0,
        })
    }

    /// Timeout for reads and writes (0: no limit).
    pub fn set_timeout(&mut self, ns: u64) {
        self.timeout = ns;
    }

    /// Read what is available (at least one byte); 0 = end of stream.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, i64> {
        let len = buf.len().min(CHUNK);
        let m = call(&words(proto::RECV, &[self.id, len as u64, self.timeout]))?;
        Ok(ipc::unpack(&m, &mut buf[..len]))
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, i64> {
        let chunk = &data[..data.len().min(CHUNK)];
        let m = call(&ipc::pack_at(proto::SEND, &[self.id, self.timeout], chunk))?;
        Ok(m.mr[0] as usize)
    }

    pub fn write_all(&mut self, mut data: &[u8]) -> Result<(), i64> {
        while !data.is_empty() {
            let n = self.write(data)?;
            data = &data[n..];
        }
        Ok(())
    }

    /// End sending (FIN); reading stays possible.
    pub fn shutdown(&self) -> Result<(), i64> {
        call(&words(proto::SHUTDOWN, &[self.id])).map(|_| ())
    }

    /// ((local address, port), (remote address, port)).
    pub fn addrs(&self) -> Result<((u32, u16), (u32, u16)), i64> {
        let m = call(&words(proto::ADDRS, &[self.id]))?;
        Ok((
            (m.mr[0] as u32, m.mr[1] as u16),
            (m.mr[2] as u32, m.mr[3] as u16),
        ))
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = call(&words(proto::CLOSE, &[self.id]));
    }
}

pub struct TcpListener {
    id: u64,
}

impl TcpListener {
    pub fn bind(port: u16, backlog: usize) -> Result<Self, i64> {
        let m = call(&words(proto::TCP_LISTEN, &[port as u64, backlog as u64]))?;
        Ok(Self { id: m.mr[0] })
    }

    /// Next incoming connection: (stream, client address, port).
    pub fn accept(&self, timeout_ns: u64) -> Result<(TcpStream, u32, u16), i64> {
        let m = call(&words(proto::TCP_ACCEPT, &[self.id, timeout_ns]))?;
        Ok((
            TcpStream {
                id: m.mr[0],
                timeout: 0,
            },
            m.mr[1] as u32,
            m.mr[2] as u16,
        ))
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let _ = call(&words(proto::CLOSE, &[self.id]));
    }
}

pub struct UdpSocket {
    id: u64,
    port: u16,
}

impl UdpSocket {
    /// Port 0: ephemeral port picked by the service.
    pub fn bind(port: u16) -> Result<Self, i64> {
        let m = call(&words(proto::UDP_BIND, &[port as u64]))?;
        Ok(Self {
            id: m.mr[0],
            port: m.mr[1] as u16,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn send_to(&self, data: &[u8], addr: u32, port: u16) -> Result<usize, i64> {
        call(&ipc::pack_at(
            proto::SEND_TO,
            &[self.id, addr as u64, port as u64],
            data,
        ))
        .map(|m| m.mr[0] as usize)
    }

    /// Received datagram: (bytes, sender address, port).
    pub fn recv_from(&self, buf: &mut [u8], timeout_ns: u64) -> Result<(usize, u32, u16), i64> {
        let m = call(&words(proto::RECV_FROM, &[self.id, timeout_ns]))?;
        let n = ipc::unpack_at(&m, 2, buf);
        Ok((n, ipc::header(&m, 0) as u32, ipc::header(&m, 1) as u16))
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        let _ = call(&words(proto::CLOSE, &[self.id]));
    }
}

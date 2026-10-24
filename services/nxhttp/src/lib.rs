//! HTTPS for nxk programs: rustls with the pure-Rust RustCrypto provider
//! and Mozilla's roots (`webpki-roots`), HTTP/1.1 over one connection kept
//! open per host, no compression. Random bytes come from the kernel
//! (`GETRANDOM`); certificates need the wall clock (`power` sets it from
//! the network).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustls::{ClientConfig, ClientConnection, StreamOwned};

type Conn = BufReader<StreamOwned<ClientConnection, TcpStream>>;

/// Bodies larger than this are refused.
pub const MAX_BODY: usize = 16 << 20;
const MAX_HEAD: usize = 64 << 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// An HTTP reply: status, headers (names in lowercase), body.
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Requests to one host, over one TLS connection opened on demand.
pub struct Client {
    host: String,
    conn: Option<Conn>,
    connect_timeout: Duration,
    io_timeout: Duration,
}

fn config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let cfg = ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
            .with_safe_default_protocol_versions()
            .expect("nxhttp: TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

impl Client {
    pub fn new(host: &str) -> Self {
        Self {
            host: host.to_string(),
            conn: None,
            connect_timeout: CONNECT_TIMEOUT,
            io_timeout: IO_TIMEOUT,
        }
    }

    /// The same, giving up on the connection or a read past `t`.
    pub fn with_timeout(host: &str, t: Duration) -> Self {
        Self {
            connect_timeout: t,
            io_timeout: t,
            ..Self::new(host)
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    fn connect(&self) -> io::Result<Conn> {
        self.connect_timed().map(|(c, _)| c)
    }

    /// A new connection, with how long its TCP and TLS handshakes took.
    fn connect_timed(&self) -> io::Result<(Conn, (Duration, Duration))> {
        let addr = (self.host.as_str(), 443)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| bad("no address"))?;
        let t0 = Instant::now();
        let sock = TcpStream::connect_timeout(&addr, self.connect_timeout)?;
        let tcp = t0.elapsed();
        sock.set_read_timeout(Some(self.io_timeout))?;
        sock.set_write_timeout(Some(self.io_timeout))?;
        let _ = sock.set_nodelay(true);
        let name = self
            .host
            .clone()
            .try_into()
            .map_err(|_| bad("bad host name"))?;
        let tls = ClientConnection::new(config(), name).map_err(io::Error::other)?;
        let mut stream = StreamOwned::new(tls, sock);
        let t1 = Instant::now();
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        Ok((BufReader::new(stream), (tcp, t1.elapsed())))
    }

    /// A GET whose body is read as it comes, on a connection of its own
    /// (a speed test must not wait behind another request): the head is
    /// read, the body is left in the returned stream.
    pub fn open(&self, path: &str, headers: &[(&str, &str)]) -> io::Result<Stream> {
        let (mut conn, (tcp, tls)) = self.connect_timed()?;
        let head = request_head("GET", path, &self.host, headers, 0)?;
        let t0 = Instant::now();
        conn.get_mut().write_all(head.as_bytes())?;
        conn.get_mut().flush()?;
        let (status, headers) = read_head(&mut conn)?;
        let first_byte = t0.elapsed();
        let header = |n: &str| {
            headers
                .iter()
                .find(|(h, _)| h == n)
                .map(|(_, v)| v.as_str())
        };
        if header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            return Err(bad("chunked body: not streamed"));
        }
        let left = header("content-length").and_then(|l| l.parse().ok());
        Ok(Stream {
            status,
            headers,
            tcp,
            tls,
            first_byte,
            left,
            conn,
        })
    }

    /// One request; a connection that was idle and died is opened again
    /// once. `headers` must not hold line breaks.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Response> {
        let fresh = self.conn.is_none();
        match self.exchange(method, path, headers, body) {
            Err(_) if !fresh => {
                self.conn = None;
                self.exchange(method, path, headers, body)
            }
            r => r,
        }
    }

    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Response> {
        let head = request_head(method, path, &self.host, headers, body.len())?;
        if self.conn.is_none() {
            self.conn = Some(self.connect()?);
        }
        let conn = self.conn.as_mut().expect("connected");
        let sent = conn
            .get_mut()
            .write_all(head.as_bytes())
            .and_then(|_| conn.get_mut().write_all(body))
            .and_then(|_| conn.get_mut().flush());
        let reply = sent.and_then(|_| read_response(conn, method == "HEAD"));
        match &reply {
            Ok(r)
                if r.header("connection")
                    .is_some_and(|c| c.eq_ignore_ascii_case("close")) =>
            {
                self.conn = None
            }
            Ok(_) => {}
            Err(_) => self.conn = None,
        }
        reply
    }

    pub fn get(&mut self, path: &str, headers: &[(&str, &str)]) -> io::Result<Response> {
        self.request("GET", path, headers, &[])
    }

    pub fn post(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> io::Result<Response> {
        self.request("POST", path, headers, body)
    }
}

/// A reply whose body has not been read yet (`Client::open`): `Read`
/// gives it as it arrives, up to `Content-Length` (or to the close).
pub struct Stream {
    pub status: u16,
    /// Names in lowercase.
    pub headers: Vec<(String, String)>,
    /// TCP connect, TLS handshake, and request to first reply byte.
    pub tcp: Duration,
    pub tls: Duration,
    pub first_byte: Duration,
    /// Body bytes still to come, when the server said.
    left: Option<u64>,
    conn: Conn,
}

impl Stream {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body's length, when the server said.
    pub fn len(&self) -> Option<u64> {
        self.header("content-length").and_then(|l| l.parse().ok())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let cap = self.left.map_or(buf.len(), |l| buf.len().min(l as usize));
        if cap == 0 {
            return Ok(0);
        }
        let n = self.conn.read(&mut buf[..cap])?;
        if let Some(l) = self.left.as_mut() {
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            *l -= n as u64;
        }
        Ok(n)
    }
}

fn read_line(conn: &mut Conn, budget: &mut usize) -> io::Result<String> {
    let mut line = Vec::new();
    let n = conn
        .by_ref()
        .take(*budget as u64)
        .read_until(b'\n', &mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed",
        ));
    }
    *budget -= n;
    if line.last() != Some(&b'\n') {
        return Err(bad("header too long"));
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

/// The status and headers (names in lowercase) of a reply.
fn read_head(conn: &mut Conn) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut budget = MAX_HEAD;
    let status_line = read_line(conn, &mut budget)?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("bad status line"))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(conn, &mut budget)?;
        if line.is_empty() {
            break;
        }
        if let Some((n, v)) = line.split_once(':') {
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn read_response(conn: &mut Conn, head_only: bool) -> io::Result<Response> {
    let (status, headers) = read_head(conn)?;
    let mut r = Response {
        status,
        headers,
        body: Vec::new(),
    };
    if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(r);
    }
    let chunked = r
        .header("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    if chunked {
        r.body = read_chunked(conn)?;
    } else if let Some(len) = r.header("content-length") {
        let len: usize = len.parse().map_err(|_| bad("bad length"))?;
        if len > MAX_BODY {
            return Err(bad("body too large"));
        }
        r.body = vec![0; len];
        conn.read_exact(&mut r.body)?;
    } else {
        conn.by_ref()
            .take(MAX_BODY as u64 + 1)
            .read_to_end(&mut r.body)?;
        if r.body.len() > MAX_BODY {
            return Err(bad("body too large"));
        }
        // Ended by the close: nothing more on this connection.
        r.headers.push(("connection".into(), "close".into()));
    }
    Ok(r)
}

fn read_chunked(conn: &mut Conn) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = MAX_HEAD;
        let line = read_line(conn, &mut budget)?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| bad("bad chunk size"))?;
        if size == 0 {
            // Trailers, then the empty line.
            while !read_line(conn, &mut budget)?.is_empty() {}
            return Ok(body);
        }
        if body.len() + size > MAX_BODY {
            return Err(bad("body too large"));
        }
        let at = body.len();
        body.resize(at + size, 0);
        conn.read_exact(&mut body[at..])?;
        let mut crlf = [0u8; 2];
        conn.read_exact(&mut crlf)?;
    }
}

/// The request line and headers. `Content-Length` goes only with a body
/// (or a method that takes one): Amazon's image origin answers 400 to a GET
/// that has `Content-Length: 0` whenever its CDN has no copy.
fn request_head(
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body_len: usize,
) -> io::Result<String> {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n",
        method, path, host
    );
    if body_len > 0 || matches!(method, "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", body_len));
    }
    for (n, v) in headers {
        if n.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(bad("line break in a header"));
        }
        head.push_str(&format!("{}: {}\r\n", n, v));
    }
    head.push_str("\r\n");
    Ok(head)
}

/// Splits `https://host/path` into its host and path.
pub fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    (!host.is_empty()).then_some((host, path))
}

#[cfg(target_os = "nxk")]
fn kernel_random(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    for chunk in buf.chunks_mut(64 * 1024) {
        unsafe {
            nxk_abi::user::syscall2(
                nxk_abi::sys::GETRANDOM,
                chunk.as_mut_ptr() as u64,
                chunk.len() as u64,
            )
        };
    }
    Ok(())
}

#[cfg(target_os = "nxk")]
getrandom::register_custom_getrandom!(kernel_random);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_get_has_no_content_length() {
        let h = request_head(
            "GET",
            "/a.png",
            "m.media-amazon.com",
            &[("Accept", "*/*")],
            0,
        )
        .unwrap();
        assert_eq!(
            h,
            "GET /a.png HTTP/1.1\r\nHost: m.media-amazon.com\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n"
        );
    }

    #[test]
    fn a_post_says_its_length_even_when_empty() {
        assert!(request_head("POST", "/x", "h", &[], 0)
            .unwrap()
            .contains("Content-Length: 0\r\n"));
        assert!(request_head("POST", "/x", "h", &[], 5)
            .unwrap()
            .contains("Content-Length: 5\r\n"));
    }

    #[test]
    fn a_line_break_in_a_header_is_refused() {
        assert!(request_head("GET", "/", "h", &[("A", "b\r\nC: d")], 0).is_err());
    }
}

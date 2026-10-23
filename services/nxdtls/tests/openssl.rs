//! The client against OpenSSL's own DTLS 1.2 server on the loopback
//! (`openssl s_server -dtls1_2`, its certificate cut into fragments by a
//! small MTU): the cookie exchange, the pin, the signature, Finished, then
//! data both ways (the server prints what it gets, sends what it reads).

use std::io::{BufRead, BufReader, Write};
use std::net::UdpSocket;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use nxdtls::{Client, Error, Pin, Secrets};

struct Server {
    child: Child,
    input: ChildStdin,
    printed: Receiver<String>,
    port: u16,
    pin: Pin,
    _dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn openssl(args: &[&str]) {
    let ok = Command::new("openssl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("openssl on the PATH")
        .success();
    assert!(ok, "openssl {:?}", args);
}

/// A server with a fresh RSA certificate named `cn`, on `port`.
fn server(cn: &str, port: u16, extra: &[&str]) -> Server {
    let dir = std::env::temp_dir().join(format!("nxdtls-{}-{}", std::process::id(), port));
    std::fs::create_dir_all(&dir).unwrap();
    let (key, cert, der) = (dir.join("k.pem"), dir.join("c.pem"), dir.join("c.der"));
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
    openssl(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-subj",
        &format!("/O=nxk test/CN={}", cn),
        "-keyout",
        &s(&key),
        "-out",
        &s(&cert),
    ]);
    openssl(&[
        "x509",
        "-in",
        &s(&cert),
        "-outform",
        "DER",
        "-out",
        &s(&der),
    ]);
    let digest = sha256_hex(&std::fs::read(&der).unwrap());
    let mut args = vec![
        "s_server".to_string(),
        "-dtls1_2".into(),
        "-cipher".into(),
        "ECDHE-RSA-CHACHA20-POLY1305".into(),
        "-cert".into(),
        s(&cert),
        "-key".into(),
        s(&key),
        "-accept".into(),
        format!("127.0.0.1:{}", port),
        "-mtu".into(),
        "500".into(),
        "-quiet".into(),
    ];
    args.extend(extra.iter().map(|a| a.to_string()));
    let mut child = Command::new("openssl")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, printed) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    Server {
        child,
        input,
        printed,
        port,
        pin: Pin::from_hex(cn, &digest).unwrap(),
        _dir: dir,
    }
}

fn sha256_hex(d: &[u8]) -> String {
    let out = Command::new("openssl")
        .args(["dgst", "-sha256", "-r"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(d)?;
            c.wait_with_output()
        })
        .unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_string()
}

/// The handshake, `send` to the server (it must print it), then `reply`
/// from it: what the client got (or the error).
fn exchange(srv: &mut Server, pin: Pin, send: &[u8], reply: &[u8]) -> Result<Vec<u8>, Error> {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.connect(("127.0.0.1", srv.port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut c = Client::new(
        pin,
        Secrets {
            random: [0x5A; 32],
            ecdh: [0x33; 32],
        },
    );
    let started = Instant::now();
    for d in c.start(started) {
        sock.send(&d).unwrap();
    }
    let mut sent = false;
    let mut got = Vec::new();
    let mut buf = [0u8; 2048];
    while started.elapsed() < Duration::from_secs(10) {
        if c.is_connected() && !sent {
            sock.send(&c.seal(send).unwrap()).unwrap();
            sent = true;
            let want = String::from_utf8_lossy(send).trim().to_string();
            let seen = srv.printed.recv_timeout(Duration::from_secs(3));
            assert_eq!(seen.map(|l| l.trim().to_string()), Ok(want));
            srv.input.write_all(reply).unwrap();
            srv.input.flush().unwrap();
        }
        match sock.recv(&mut buf) {
            Ok(n) => {
                let (out, app) = c.on_datagram(Instant::now(), &buf[..n])?;
                out.iter().for_each(|d| {
                    sock.send(d).unwrap();
                });
                for a in app {
                    got.extend(a);
                }
                if got.ends_with(b"\n") {
                    return Ok(got);
                }
            }
            Err(_) => {
                for d in c.on_timer(Instant::now()) {
                    sock.send(&d).unwrap();
                }
            }
        }
    }
    Err(Error::Handshake("timed out"))
}

#[test]
fn handshake_and_data_with_openssl() {
    let mut srv = server("luna-test-server", 44331, &[]);
    let pin = srv.pin.clone();
    let got = exchange(&mut srv, pin, b"hello nxk\n", b"hello client\n").unwrap();
    assert_eq!(got, b"hello client\n");
}

#[test]
fn the_cookie_exchange_is_followed() {
    let mut srv = server("luna-test-server", 44332, &["-listen"]);
    let pin = srv.pin.clone();
    let got = exchange(&mut srv, pin, b"abc\n", b"def\n").unwrap();
    assert_eq!(got, b"def\n");
}

#[test]
fn another_digest_is_refused() {
    let mut srv = server("luna-test-server", 44333, &[]);
    let mut pin = srv.pin.clone();
    pin.sha256[0] ^= 1;
    assert_eq!(
        exchange(&mut srv, pin, b"x\n", b"y\n"),
        Err(Error::Pin("certificate digest"))
    );
}

#[test]
fn another_name_is_refused() {
    let mut srv = server("luna-test-server", 44334, &[]);
    let mut pin = srv.pin.clone();
    pin.common_name = "someone-else".into();
    assert_eq!(
        exchange(&mut srv, pin, b"x\n", b"y\n"),
        Err(Error::Pin("certificate name"))
    );
}

#[test]
fn p256_when_the_server_wants_it() {
    let mut srv = server("luna-test-server", 44335, &["-groups", "P-256"]);
    let pin = srv.pin.clone();
    let got = exchange(&mut srv, pin, b"curve\n", b"p-256\n").unwrap();
    assert_eq!(got, b"p-256\n");
}

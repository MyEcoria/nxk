//! `/bin/speedtest`: a download speed test over the network in use. Run
//! by hand it prints its progress (`speedtest [-s SERVER] [-c CONNS] [-t
//! SECS]`). Started by `display` (`speed::KEY_ENV` in its environment) it
//! says hello, then serves `abi::proto::speed`: whoever shows the result
//! (the Network page, an overlay) starts the test and polls its report.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxgfx::os::{self, Endpoint, Service};
use nxk_abi::err::{EBUSY, EINVAL, EPERM};
use nxk_abi::proto::{display, net, speed as proto};
use nxk_abi::rights;
use nxk_abi::user::{self, Msg};
use nxspeed::{run, Opened, Phase, Settings, Snapshot, Source};

/// A file to download, over HTTPS.
struct Server {
    label: &'static str,
    host: &'static str,
    path: &'static str,
}

/// The servers a `START` can pick (index 0 is the default). OVH's proof
/// server keeps files of 1 MiB to 10 GiB; the largest that is quick to
/// open is read until the time is up.
const SERVERS: [Server; 1] = [Server {
    label: "OVH proof",
    host: "proof.ovh.net",
    path: "/files/1Gb.dat",
}];
const CONNS: usize = 4;
const SECS: u64 = 10;
/// OVH answers 429 past a few connections from one address, then drops its SYNs for a while.
const MAX_CONNS: usize = 4;
const MAX_SECS: u64 = 60;
/// A connection that says nothing for this long is given up on.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct Https {
    host: &'static str,
    path: &'static str,
}

impl Source for Https {
    fn open(&self) -> io::Result<Opened> {
        let client = nxhttp::Client::with_timeout(self.host, IO_TIMEOUT);
        let s = client.open(self.path, &[("Accept-Encoding", "identity")])?;
        match s.status {
            200 => {}
            // OVH limits the connections of one address.
            429 => return Err(io::Error::new(io::ErrorKind::WouldBlock, "HTTP 429")),
            n => return Err(io::Error::other(format!("HTTP {}", n))),
        }
        Ok(Opened {
            tcp: s.tcp,
            tls: s.tls,
            first_byte: s.first_byte,
            body: Box::new(s),
        })
    }
}

fn source(server: usize) -> Arc<dyn Source> {
    let s = &SERVERS[server];
    Arc::new(Https {
        host: s.host,
        path: s.path,
    })
}

/// The `net::LINK_*` in use (0: none, or `net` not there).
fn link() -> u64 {
    Service::lookup("net")
        .and_then(|n| n.call(&Msg::new(net::STATS, 0, 0)))
        .map_or(0, |m| m.mr[5])
}

// ------------------------------------------------------------- service ---

struct State {
    report: proto::Report,
    /// `STATUS` calls waiting for a newer report: token, version known,
    /// deadline (ns).
    waiters: Vec<(u64, u64, u64)>,
    /// The test running.
    stop: Option<Arc<AtomicBool>>,
}

type Shared = Arc<Mutex<State>>;

fn status_msg(r: &proto::Report) -> Msg {
    user::pack_at(0, &[r.version], &r.encode()).0
}

/// Replace the report and answer the calls that wait for it.
fn publish(sh: &Shared, f: impl FnOnce(&mut proto::Report)) {
    let mut s = sh.lock().unwrap();
    f(&mut s.report);
    s.report.version += 1;
    let (r, msg) = (s.report, status_msg(&s.report));
    s.waiters.retain(|&(token, known, _)| {
        let keep = known == r.version;
        if !keep {
            let _ = Endpoint::reply_to(token, &msg);
        }
        keep
    });
}

fn from_snapshot(r: &mut proto::Report, s: &Snapshot) {
    r.state = match s.phase {
        Phase::Connecting => proto::ST_CONNECTING,
        Phase::Running => proto::ST_RUNNING,
        Phase::Done => proto::ST_DONE,
        Phase::Failed => proto::ST_FAILED,
    };
    r.error = s.errno as u64;
    r.bytes = s.bytes;
    r.elapsed_ms = s.elapsed.as_millis() as u64;
    r.now_kbps = s.now_kbps;
    r.avg_kbps = s.avg_kbps;
    r.peak_kbps = s.peak_kbps;
    r.tcp_us = s.tcp.as_micros() as u64;
    r.tls_us = s.tls.as_micros() as u64;
    r.first_byte_us = s.first_byte.as_micros() as u64;
}

fn start(sh: &Shared, m: &Msg) -> Msg {
    let (server, conns, secs) = (user::header(m, 0), user::header(m, 1), user::header(m, 2));
    let server = server as usize;
    if server >= SERVERS.len() || conns > MAX_CONNS as u64 || secs > MAX_SECS * 1000 {
        return Msg::error(-EINVAL);
    }
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut s = sh.lock().unwrap();
        if s.stop.is_some() {
            return Msg::error(-EBUSY);
        }
        s.stop = Some(stop.clone());
    }
    let settings = Settings {
        conns: if conns == 0 { CONNS } else { conns as usize },
        max: Duration::from_millis(if secs == 0 { SECS * 1000 } else { secs }),
        ..Settings::default()
    };
    let start_link = link();
    let label = SERVERS[server].label.as_bytes();
    publish(sh, |r| {
        *r = proto::Report {
            version: r.version,
            state: proto::ST_CONNECTING,
            link: start_link,
            link_end: start_link,
            conns: settings.conns as u64,
            server: server as u64,
            ..Default::default()
        };
        r.label[..label.len()].copy_from_slice(label);
    });
    let sh = sh.clone();
    let spawned = std::thread::Builder::new().spawn({
        let sh = sh.clone();
        move || {
            let last = run(source(server), &settings, &stop, &mut |s| {
                publish(&sh, |r| from_snapshot(r, s))
            });
            let end = link();
            publish(&sh, |r| {
                from_snapshot(r, &last);
                r.link_end = end;
            });
            sh.lock().unwrap().stop = None;
        }
    });
    if spawned.is_err() {
        publish(&sh, |r| (r.state, r.error) = (proto::ST_FAILED, 11));
        sh.lock().unwrap().stop = None;
        return Msg::error(-nxk_abi::err::EAGAIN);
    }
    Msg::new(0, 0, 0)
}

fn serve(ep: Endpoint, sh: Shared) {
    loop {
        let next = sh.lock().unwrap().waiters.iter().map(|w| w.2).min();
        let wait = next.map_or(0, |d| d.saturating_sub(os::now_ns()).max(1));
        let rx = match ep.recv(wait) {
            Ok(rx) => rx,
            Err(_) => {
                let now = os::now_ns();
                let mut s = sh.lock().unwrap();
                let msg = status_msg(&s.report);
                s.waiters.retain(|&(token, _, at)| {
                    let keep = at > now;
                    if !keep {
                        let _ = Endpoint::reply_to(token, &msg);
                    }
                    keep
                });
                continue;
            }
        };
        let m = &rx.msg;
        let reply = match m.label() {
            proto::STATUS => {
                let (known, ns) = (user::header(m, 0), user::header(m, 1));
                let mut s = sh.lock().unwrap();
                if known != s.report.version || ns == 0 {
                    Some(status_msg(&s.report))
                } else {
                    let at = os::now_ns() + ns.min(proto::MAX_WAIT_NS);
                    s.waiters.push((rx.token, known, at));
                    None
                }
            }
            proto::START => Some(start(&sh, m)),
            proto::STOP => {
                if let Some(s) = &sh.lock().unwrap().stop {
                    s.store(true, Ordering::Relaxed);
                }
                Some(Msg::new(0, 0, 0))
            }
            _ => Some(Msg::error(-EINVAL)),
        };
        if let Some(r) = reply {
            let _ = Endpoint::reply_to(rx.token, &r);
        }
    }
}

/// Hand the endpoint to `display` (the key it gave) and serve it.
fn service(key: u64) -> Result<(), i64> {
    let ep = Endpoint::new()?;
    let send = os::handle_dup(ep.handle(), rights::SEND | rights::GRANT)?;
    let said =
        Service::lookup("display")?.call_with(Msg::from_words(display::HELPER, &[key]), send);
    os::handle_close(send);
    said?;
    let sh: Shared = Arc::new(Mutex::new(State {
        report: proto::Report::default(),
        waiters: Vec::new(),
        stop: None,
    }));
    serve(ep, sh);
    Ok(())
}

// ----------------------------------------------------------------- cli ---

fn mbps(kbps: u64) -> String {
    format!("{:7.2} Mb/s", kbps as f64 / 1000.0)
}

fn cli() -> i32 {
    let (mut server, mut conns, mut secs) = (0usize, CONNS, SECS);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v = args.next().and_then(|v| v.parse::<u64>().ok());
        match (a.as_str(), v) {
            ("-s", Some(v)) if (v as usize) < SERVERS.len() => server = v as usize,
            ("-c", Some(v)) if (1..=MAX_CONNS as u64).contains(&v) => conns = v as usize,
            ("-t", Some(v)) if (1..=MAX_SECS).contains(&v) => secs = v,
            _ => {
                eprintln!(
                    "usage: speedtest [-s SERVER 0..{}] [-c CONNECTIONS 1..{}] [-t SECONDS 1..{}]",
                    SERVERS.len() - 1,
                    MAX_CONNS,
                    MAX_SECS
                );
                return 2;
            }
        }
    }
    let s = &SERVERS[server];
    println!(
        "{} (https://{}{}), {} connections, {} s, link {}",
        s.label,
        s.host,
        s.path,
        conns,
        secs,
        link()
    );
    let settings = Settings {
        conns,
        max: Duration::from_secs(secs),
        tick: Duration::from_millis(1000),
        ..Settings::default()
    };
    let r = run(
        source(server),
        &settings,
        &AtomicBool::new(false),
        &mut |s| {
            if s.phase == Phase::Running {
                println!(
                    "{:5.1} s {:9} KiB  now {}  avg {}",
                    s.elapsed.as_secs_f32(),
                    s.bytes / 1024,
                    mbps(s.now_kbps),
                    mbps(s.avg_kbps)
                );
            }
        },
    );
    if r.phase == Phase::Failed {
        println!(
            "failed: {} (errno {})",
            r.error.unwrap_or_default(),
            r.errno
        );
        return 1;
    }
    println!(
        "result: avg {}  best {}  {} KiB in {:.1} s; connect {} ms, TLS {} ms, first byte {} ms",
        mbps(r.avg_kbps),
        mbps(r.peak_kbps),
        r.bytes / 1024,
        r.elapsed.as_secs_f32(),
        r.tcp.as_millis(),
        r.tls.as_millis(),
        r.first_byte.as_millis()
    );
    0
}

fn main() {
    let key = std::env::var(proto::KEY_ENV)
        .ok()
        .and_then(|k| k.parse().ok());
    let code = match key {
        Some(k) => match service(k) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("speedtest: {}", e);
                if e == -EPERM {
                    3
                } else {
                    1
                }
            }
        },
        None => cli(),
    };
    std::process::exit(code);
}

//! Download speed measurement. A `Source` opens one connection to a file
//! (the program says how: HTTPS in `speedtest`, a fake in the tests);
//! `run` reads from several of them at once for a while, discards the
//! bytes and reports the rate over the last second, since the first
//! byte, and its best second. Nothing here knows about the network, so
//! `cargo test --manifest-path services/nxspeed/Cargo.toml` runs on the PC.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A connection that has sent its request and read the reply's head.
pub struct Opened {
    pub body: Box<dyn Read + Send>,
    /// TCP connect, TLS handshake, request to first reply byte.
    pub tcp: Duration,
    pub tls: Duration,
    pub first_byte: Duration,
}

/// Opens connections to the file measured, one per call.
pub trait Source: Send + Sync {
    fn open(&self) -> io::Result<Opened>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// No byte yet.
    Connecting,
    Running,
    /// Over: the figures are the result.
    Done,
    /// Nothing was measured: see `Snapshot::error`.
    Failed,
}

/// What the test shows at one moment; rates in kbit/s.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub phase: Phase,
    pub bytes: u64,
    pub elapsed: Duration,
    pub now_kbps: u64,
    pub avg_kbps: u64,
    pub peak_kbps: u64,
    /// Of the first connection that opened.
    pub tcp: Duration,
    pub tls: Duration,
    pub first_byte: Duration,
    /// The last error met, and as an errno (`errno`).
    pub error: Option<String>,
    pub errno: i32,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            phase: Phase::Connecting,
            bytes: 0,
            elapsed: Duration::ZERO,
            now_kbps: 0,
            avg_kbps: 0,
            peak_kbps: 0,
            tcp: Duration::ZERO,
            tls: Duration::ZERO,
            first_byte: Duration::ZERO,
            error: None,
            errno: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// Connections read at once.
    pub conns: usize,
    /// The test ends after this long (from the start).
    pub max: Duration,
    /// Without a byte after this long, it failed.
    pub connect_max: Duration,
    /// The live rate is over this long.
    pub window: Duration,
    /// How often `run` reports.
    pub tick: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            conns: 4,
            max: Duration::from_secs(10),
            connect_max: Duration::from_secs(15),
            window: Duration::from_secs(1),
            tick: Duration::from_millis(250),
        }
    }
}

/// Linux errno of an I/O error (what `raw_os_error` says, or its kind's).
pub fn errno(e: &io::Error) -> i32 {
    use io::ErrorKind::*;
    e.raw_os_error().unwrap_or(match e.kind() {
        TimedOut => 110,
        ConnectionRefused => 111,
        ConnectionReset | ConnectionAborted => 104,
        NotFound => 2,
        // The server is busy (HTTP 429): try again later.
        WouldBlock => 11,
        InvalidData | InvalidInput => 22,
        UnexpectedEof | BrokenPipe => 32,
        _ => 5,
    })
}

/// kbit/s of `bytes` over `d`.
fn kbps(bytes: u64, d: Duration) -> u64 {
    (bytes * 8).checked_div(d.as_millis() as u64).unwrap_or(0)
}

/// Byte counts over time: the rate over the last `window`.
pub struct Window {
    window: Duration,
    samples: VecDeque<(Duration, u64)>,
}

impl Window {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
        }
    }

    /// The count `bytes` seen at `at`; the rate over the window, in kbit/s.
    pub fn push(&mut self, at: Duration, bytes: u64) -> u64 {
        self.samples.push_back((at, bytes));
        // Keep one sample at or before the window's start as its anchor.
        while self.samples.len() > 2 && at.saturating_sub(self.samples[1].0) >= self.window {
            self.samples.pop_front();
        }
        let (t0, b0) = self.samples[0];
        kbps(bytes - b0, at - t0)
    }

    /// The window is as long as it should be (a rate before that is a ramp).
    pub fn full(&self) -> bool {
        match (self.samples.front(), self.samples.back()) {
            (Some(a), Some(b)) => {
                b.0.saturating_sub(a.0) + Duration::from_millis(50) >= self.window
            }
            _ => false,
        }
    }
}

/// The first connection's timings (TCP, TLS, first byte), and the last
/// error with its errno.
type Seen = (
    Option<(Duration, Duration, Duration)>,
    Option<(String, i32)>,
);

struct Shared {
    bytes: AtomicU64,
    /// When the first byte came, ns since the start plus one (0: none yet).
    first: AtomicU64,
    stop: AtomicBool,
    alive: AtomicUsize,
    seen: Mutex<Seen>,
}

fn worker(src: Arc<dyn Source>, sh: Arc<Shared>, t0: Instant) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut failures = 0;
    while !sh.stop.load(Ordering::Relaxed) && failures < 4 {
        let opened = match src.open() {
            Ok(o) => o,
            Err(e) => {
                sh.seen.lock().unwrap().1 = Some((e.to_string(), errno(&e)));
                failures += 1;
                thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        {
            let mut seen = sh.seen.lock().unwrap();
            seen.0
                .get_or_insert((opened.tcp, opened.tls, opened.first_byte));
        }
        let mut body = opened.body;
        while !sh.stop.load(Ordering::Relaxed) {
            match body.read(&mut buf) {
                // The file ended: another connection takes over.
                Ok(0) => break,
                Ok(n) => {
                    failures = 0;
                    let _ = sh.first.compare_exchange(
                        0,
                        t0.elapsed().as_nanos() as u64 + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    sh.bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    sh.seen.lock().unwrap().1 = Some((e.to_string(), errno(&e)));
                    failures += 1;
                    break;
                }
            }
        }
    }
    sh.alive.fetch_sub(1, Ordering::Relaxed);
}

/// Run a test, calling `update` every tick, until `settings.max`, `stop`
/// or every connection gave up; returns the final figures.
pub fn run(
    src: Arc<dyn Source>,
    settings: &Settings,
    stop: &AtomicBool,
    update: &mut dyn FnMut(&Snapshot),
) -> Snapshot {
    let t0 = Instant::now();
    let conns = settings.conns.max(1);
    let sh = Arc::new(Shared {
        bytes: AtomicU64::new(0),
        first: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        alive: AtomicUsize::new(conns),
        seen: Mutex::new((None, None)),
    });
    let mut threads = Vec::new();
    for _ in 0..conns {
        let (s, h, t) = (src.clone(), sh.clone(), t0);
        match thread::Builder::new().spawn(move || worker(s, h, t)) {
            Ok(j) => threads.push(j),
            Err(_) => {
                sh.alive.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
    let mut win = Window::new(settings.window);
    let mut snap = Snapshot::new();
    update(&snap);
    loop {
        thread::sleep(settings.tick);
        let at = t0.elapsed();
        let bytes = sh.bytes.load(Ordering::Relaxed);
        let first = sh.first.load(Ordering::Relaxed);
        snap.bytes = bytes;
        snap.elapsed = at;
        if first != 0 {
            snap.phase = Phase::Running;
            let from = Duration::from_nanos(first - 1);
            snap.avg_kbps = kbps(bytes, at.saturating_sub(from));
            let now = win.push(at, bytes);
            snap.now_kbps = now;
            // A rate over a window not yet filled is the ramp: not a best.
            if win.full() {
                snap.peak_kbps = snap.peak_kbps.max(now);
            }
        }
        let dead = sh.alive.load(Ordering::Relaxed) == 0;
        let late = first == 0 && at >= settings.connect_max;
        let over = at >= settings.max || stop.load(Ordering::Relaxed) || dead || late;
        if let Some((tcp, tls, fb)) = sh.seen.lock().unwrap().0 {
            (snap.tcp, snap.tls, snap.first_byte) = (tcp, tls, fb);
        }
        if over {
            break;
        }
        update(&snap);
    }
    sh.stop.store(true, Ordering::Relaxed);
    // Readers blocked in the network give up at their own timeout: wait a
    // little, then leave them to end by themselves.
    let end = Instant::now() + Duration::from_millis(1500);
    while sh.alive.load(Ordering::Relaxed) > 0 && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    if let Some((e, n)) = sh.seen.lock().unwrap().1.clone() {
        (snap.error, snap.errno) = (Some(e), n);
    }
    if snap.bytes == 0 {
        snap.phase = Phase::Failed;
        if snap.error.is_none() {
            (snap.error, snap.errno) = (Some("no data".into()), 110);
        }
    } else {
        snap.phase = Phase::Done;
        // Errors met while data flowed (a reconnection) do not fail it.
        (snap.error, snap.errno) = (None, 0);
        // A test shorter than the window has no best second: its average.
        if snap.peak_kbps == 0 {
            snap.peak_kbps = snap.avg_kbps;
        }
    }
    update(&snap);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gives `chunk` bytes every `every`, forever (or `total` bytes).
    struct Paced {
        chunk: usize,
        every: Duration,
        total: Option<usize>,
    }

    impl Read for Paced {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.total == Some(0) {
                return Ok(0);
            }
            thread::sleep(self.every);
            let mut n = self.chunk.min(buf.len());
            if let Some(t) = self.total.as_mut() {
                n = n.min(*t);
                *t -= n;
            }
            Ok(n)
        }
    }

    struct Fake {
        chunk: usize,
        every_ms: u64,
        total: Option<usize>,
        opened: AtomicUsize,
    }

    impl Source for Fake {
        fn open(&self) -> io::Result<Opened> {
            self.opened.fetch_add(1, Ordering::Relaxed);
            Ok(Opened {
                body: Box::new(Paced {
                    chunk: self.chunk,
                    every: Duration::from_millis(self.every_ms),
                    total: self.total,
                }),
                tcp: Duration::from_millis(12),
                tls: Duration::from_millis(30),
                first_byte: Duration::from_millis(18),
            })
        }
    }

    struct Broken;

    impl Source for Broken {
        fn open(&self) -> io::Result<Opened> {
            Err(io::Error::from(io::ErrorKind::ConnectionRefused))
        }
    }

    fn quick(conns: usize, ms: u64) -> Settings {
        Settings {
            conns,
            max: Duration::from_millis(ms),
            tick: Duration::from_millis(50),
            ..Settings::default()
        }
    }

    #[test]
    fn the_window_rate_follows_the_last_second() {
        let ms = Duration::from_millis;
        let mut w = Window::new(ms(1000));
        assert_eq!(w.push(ms(0), 0), 0);
        // 125 000 B/s is 1000 kbit/s.
        assert_eq!(w.push(ms(500), 62_500), 1000);
        assert_eq!(w.push(ms(1000), 125_000), 1000);
        // It stops: the window forgets the early bytes.
        assert_eq!(w.push(ms(2000), 125_000), 0);
        assert!(w.full());
    }

    #[test]
    fn a_window_not_yet_filled_is_not_full() {
        let mut w = Window::new(Duration::from_secs(1));
        w.push(Duration::ZERO, 0);
        w.push(Duration::from_millis(300), 1000);
        assert!(!w.full());
    }

    #[test]
    fn parallel_connections_add_up() {
        // 2 x 10 000 B / 10 ms = 2 MB/s = about 16 000 kbit/s.
        let src = Arc::new(Fake {
            chunk: 10_000,
            every_ms: 10,
            total: None,
            opened: AtomicUsize::new(0),
        });
        let mut ticks = 0;
        let r = run(src, &quick(2, 1500), &AtomicBool::new(false), &mut |_| {
            ticks += 1
        });
        assert_eq!(r.phase, Phase::Done);
        assert!((9_000..=17_000).contains(&r.avg_kbps), "avg {}", r.avg_kbps);
        assert!(r.peak_kbps >= r.now_kbps.min(r.avg_kbps) && r.peak_kbps > 0);
        assert_eq!(r.tls, Duration::from_millis(30));
        assert!(ticks >= 5);
    }

    #[test]
    fn a_file_that_ends_is_opened_again() {
        let src = Arc::new(Fake {
            chunk: 10_000,
            every_ms: 5,
            total: Some(20_000),
            opened: AtomicUsize::new(0),
        });
        let r = run(
            src.clone(),
            &quick(1, 600),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(r.phase, Phase::Done);
        assert!(src.opened.load(Ordering::Relaxed) > 3);
        assert!(r.bytes > 40_000);
    }

    #[test]
    fn stop_ends_it_with_what_was_measured() {
        let src = Arc::new(Fake {
            chunk: 10_000,
            every_ms: 10,
            total: None,
            opened: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            s2.store(true, Ordering::Relaxed);
        });
        let r = run(src, &quick(1, 10_000), &stop, &mut |_| {});
        h.join().unwrap();
        assert_eq!(r.phase, Phase::Done);
        assert!(r.elapsed < Duration::from_secs(2));
    }

    #[test]
    fn no_connection_is_a_failure_with_its_errno() {
        let r = run(
            Arc::new(Broken),
            &quick(2, 5_000),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert_eq!(r.phase, Phase::Failed);
        assert_eq!(r.errno, 111);
        assert!(r.error.is_some());
        // The workers gave up on their own: it did not wait for `max`.
        assert!(r.elapsed < Duration::from_secs(4));
    }
}

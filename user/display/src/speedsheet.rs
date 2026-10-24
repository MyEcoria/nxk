//! The speed test sheet over the Network page: the rate as it runs, then
//! the result (average, best second, data, times), a graph of the rate
//! and the link it ran on. The test itself is the `speedtest` program
//! (`speed.rs`); this keeps the last report and draws it. `speed_card`
//! draws it in any rectangle, so another screen (an overlay) can show the
//! same report.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::proto::{net, speed};

use crate::font::Face;
use crate::screen::{Bytes, Screen};
use crate::stats::Data;
use crate::theme::pal;
use crate::ui::{Action, Id, Sheet, Ui};
use crate::widgets::{heading, kv, Tone};

/// Rates kept for the graph (one per report, about four a second).
const HISTORY: usize = 80;

pub(crate) struct SpeedUi {
    /// The last report of the program.
    pub report: Option<speed::Report>,
    /// The program is starting: a test was asked, no report yet.
    pub starting: bool,
    /// Why nothing runs (the program is missing, it ended).
    pub error: Option<String>,
    /// `now_kbps` of the reports of this test.
    hist: Vec<u32>,
}

impl SpeedUi {
    pub fn new() -> Self {
        Self {
            report: None,
            starting: false,
            error: None,
            hist: Vec::new(),
        }
    }

    /// A test is under way (or about to be).
    pub fn running(&self) -> bool {
        self.starting
            || self
                .report
                .is_some_and(|r| matches!(r.state, speed::ST_CONNECTING | speed::ST_RUNNING))
    }
}

/// The link's interface name.
fn link_name(link: u64) -> &'static str {
    match link {
        net::LINK_USB0 => "usb0",
        net::LINK_ETH0 => "eth0",
        net::LINK_WLAN0 => "wlan0",
        _ => "no link",
    }
}

/// `kbit/s` as Mb/s with a decimal (none past 100).
fn mbps(kbps: u64) -> String {
    if kbps >= 100_000 {
        format!("{}", kbps / 1000)
    } else {
        format!("{}.{}", kbps / 1000, kbps % 1000 / 100)
    }
}

fn millis(us: u64) -> String {
    format!("{}", (us + 500) / 1000)
}

/// What a failed test says, from its errno.
fn fail_text(errno: u64) -> String {
    match errno as i64 {
        110 => "No answer from the server.".into(),
        111 => "The server refused the connection.".into(),
        104 => "The connection was reset.".into(),
        11 => "The server is busy: too many connections. Try again later.".into(),
        101 | 100 => "The network is down.".into(),
        e => format!("It failed: {}", err::name(-e)),
    }
}

impl Ui {
    pub(crate) fn speed_open(&mut self) -> Option<Action> {
        self.sheet = Some(Sheet::Speed);
        self.speed = SpeedUi::new();
        None
    }

    pub(crate) fn speed_open_now(&self) -> bool {
        self.sheet == Some(Sheet::Speed)
    }

    /// A report came in.
    pub fn speed_report(&mut self, r: speed::Report) {
        self.speed.starting = false;
        self.speed.error = None;
        // A new test starts its graph over.
        if self.speed.report.is_none_or(|o| r.bytes < o.bytes) {
            self.speed.hist.clear();
        }
        if r.state == speed::ST_RUNNING {
            if self.speed.hist.len() == HISTORY {
                self.speed.hist.remove(0);
            }
            self.speed.hist.push(r.now_kbps as u32);
        }
        self.speed.report = Some(r);
    }

    /// The program is gone, or would not start (errno or exit code).
    pub fn speed_gone(&mut self, why: String) {
        self.speed.starting = false;
        self.speed.error = Some(why);
    }

    /// A tap or A on `id` while the sheet is up: `None` when it is not
    /// the sheet's.
    pub(crate) fn speed_tap(&mut self, id: Id) -> Option<Option<Action>> {
        if self.sheet != Some(Sheet::Speed) {
            return None;
        }
        Some(match id {
            Id::SpeedRun if self.speed.running() => {
                self.speed.starting = false;
                Some(Action::SpeedStop)
            }
            Id::SpeedRun => {
                self.speed.starting = true;
                self.speed.error = None;
                self.speed.report = None;
                self.speed.hist.clear();
                Some(Action::SpeedStart)
            }
            Id::SheetClose | Id::Scrim => {
                self.sheet = None;
                Some(Action::SpeedStop)
            }
            _ => return None,
        })
    }

    pub(crate) fn speed_sheet(&mut self, s: &mut Screen, d: &Data) {
        let (x, y, w, h) = self.frame(s, "Speed test", 960, 560);
        let (cx, cy, cw, ch) = (x + 40, y + 84, w - 80, h - 84 - 120);
        self.speed_card(s, d, (cx, cy, cw, ch));
        let running = self.speed.running();
        let label = match (running, self.speed.report.is_some()) {
            (true, _) => "Stop",
            (false, true) => "Run again",
            (false, false) => "Start",
        };
        let tone = if running { Tone::Warn } else { Tone::Accent };
        self.button(
            s,
            Id::SpeedRun,
            x + 40,
            y + h - 40 - 60,
            260,
            60,
            label,
            tone,
            true,
        );
        self.button(
            s,
            Id::SheetClose,
            x + w - 40 - 220,
            y + h - 40 - 60,
            220,
            60,
            "Close",
            Tone::Plain,
            true,
        );
    }

    /// The report in a rectangle: the rate, the figures, the graph and a
    /// line about what is going on.
    pub(crate) fn speed_card(
        &self,
        s: &mut Screen,
        d: &Data,
        (x, y, w, h): (usize, usize, usize, usize),
    ) {
        let p = pal();
        let sp = &self.speed;
        let r = sp.report.unwrap_or_default();
        let done = r.state == speed::ST_DONE;
        let live = matches!(r.state, speed::ST_RUNNING | speed::ST_DONE);
        // The rate, big: now while it runs, the average once it is over.
        let (value, caption) = match (live, done) {
            (true, true) => (mbps(r.avg_kbps), "average"),
            (true, false) => (mbps(r.now_kbps), "now"),
            _ => ("0.0".into(), "download"),
        };
        let wv = s.label(
            x,
            y + 34,
            &value,
            Face::Head,
            if live { p.text } else { p.dim },
        );
        s.label(x + wv + 12, y + 42, "Mb/s", Face::Body, p.dim);
        heading(s, x, y + 82, caption);
        // The figures, right half.
        let (kx, kr) = (x + w / 2 + 40, x + w);
        let none = || String::from("-");
        let (avg, best, data, time, hs) = if live {
            (
                format!("{} Mb/s", mbps(r.avg_kbps)),
                format!("{} Mb/s", mbps(r.peak_kbps)),
                format!("{}", Bytes(r.bytes)),
                format!("{}.{} s", r.elapsed_ms / 1000, r.elapsed_ms % 1000 / 100),
                format!(
                    "{} / {} / {} ms",
                    millis(r.tcp_us),
                    millis(r.tls_us),
                    millis(r.first_byte_us)
                ),
            )
        } else {
            (none(), none(), none(), none(), none())
        };
        if !done {
            kv(s, kx, kr, y + 14, "average so far", &avg, None);
        } else {
            kv(s, kx, kr, y + 14, "average", &avg, None);
        }
        kv(s, kx, kr, y + 44, "best second", &best, None);
        kv(s, kx, kr, y + 74, "downloaded", &data, None);
        kv(s, kx, kr, y + 104, "time", &time, None);
        kv(s, kx, kr, y + 134, "connect / TLS / reply", &hs, None);
        // The graph.
        let (gy, gh) = (y + 130, h.saturating_sub(130 + 56));
        s.fill_round(x, gy, w / 2, gh, 6, p.raised);
        let top = sp.hist.iter().copied().max().unwrap_or(0).max(1000) as usize;
        let gw = w / 2 - 24;
        let bar = (gw / HISTORY).max(2);
        for (i, &v) in sp.hist.iter().enumerate() {
            let bh = (gh - 28) * v as usize / top;
            s.fill(
                x + 12 + i * bar,
                gy + gh - 12 - bh,
                bar - 1,
                bh.max(1),
                p.accent,
            );
        }
        s.label(
            x + 14,
            gy + 16,
            &format!("{} Mb/s", mbps(top as u64)),
            Face::Cap,
            p.dim,
        );
        // What is going on.
        let link = if r.state == speed::ST_IDLE || sp.report.is_none() {
            d.active().map_or(0, |n| n.link)
        } else {
            r.link
        };
        let (text, c): (String, u32) = match () {
            _ if sp.error.is_some() => (sp.error.clone().unwrap_or_default(), p.bad),
            _ if r.state == speed::ST_FAILED => (fail_text(r.error), p.bad),
            _ if sp.starting => ("Starting the test...".into(), p.warn),
            _ if r.state == speed::ST_CONNECTING => (
                format!("Connecting to {} over {}...", r.name(), link_name(link)),
                p.warn,
            ),
            _ if r.state == speed::ST_RUNNING => (
                format!(
                    "{} over {}, {} connections",
                    r.name(),
                    link_name(link),
                    r.conns
                ),
                p.dim,
            ),
            _ if done && r.link != r.link_end => (
                format!(
                    "The network changed during the test: {} then {}",
                    link_name(r.link),
                    link_name(r.link_end)
                ),
                p.warn,
            ),
            _ if done => (format!("{} over {}", r.name(), link_name(link)), p.dim),
            _ => (
                format!(
                    "Downloads a test file for a few seconds over {}.",
                    link_name(link)
                ),
                p.dim,
            ),
        };
        s.label(
            x,
            y + h - 20,
            &crate::widgets::fit(Face::Small, &text, w),
            Face::Small,
            c,
        );
    }
}

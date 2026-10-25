//! The performance overlay over a game, switched on in the drawer (`+`):
//! the screen's frame rate and the load of the cores and the GPU, for any
//! game; for a cloud game, also the stream's ping, picture rate, data
//! rate and losses as its catalog tells them (`provider::STATS`). Sampled
//! once a second from sources that cost no call to a service.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::provider::Stats;

use crate::font::Face;
use crate::screen::Screen;
use crate::stats::{Data, Seen};
use crate::theme::{rgb, BLACK, WHITE};
use crate::ui::Ui;
use crate::widgets::{icon, Icon};

/// How often the overlay is sampled and drawn again.
pub const EVERY_NS: u64 = 1_000_000_000;
/// A frame later than this (ms) counts as missed (a 60 Hz screen: 16.7).
pub const LATE_NS: u64 = 25_000_000;

const X: usize = 20;
const Y: usize = 16;
const W: usize = 318;
const HEAD: usize = 46;
const ROW: usize = 28;
const DIM: u32 = rgb(0xBDBDBD);
const GOOD: u32 = rgb(0x5BD75B);
const WARN: u32 = rgb(0xF2C230);
const BAD: u32 = rgb(0xF05A4F);

/// What one second of a game showed.
#[derive(Clone, Default)]
pub struct Sample {
    /// Streamed from a server: the stream's figures lead.
    pub cloud: bool,
    /// Frames shown per second (x10), and the longest wait between two
    /// (ms) and the frames later than `LATE_NS`; none before a full second.
    pub fps_x10: Option<u64>,
    pub worst_ms: u64,
    pub late: u64,
    /// Mean load of the cores that are on (%), the GPU's (%, kHz).
    pub cpu: Option<u64>,
    pub gpu: Option<(u64, u64)>,
    /// Memory in use (MiB).
    pub mem_mb: Option<u64>,
    /// The catalog's figures; none while it has not said.
    pub stream: Option<Stats>,
}

/// Where the last second started: the clock and the game's flip count.
#[derive(Default)]
pub struct Clock {
    at: u64,
    flips: u64,
}

impl Clock {
    pub fn new(now: u64, flips: u64) -> Self {
        Self { at: now, flips }
    }

    /// The second that ends now: `flips` shown in all, `worst` ns the
    /// longest gap and `late` frames past `LATE_NS` since the last call.
    pub fn sample(
        &mut self,
        now: u64,
        flips: u64,
        (worst, late): (u64, u64),
        data: &mut Data,
        cloud: bool,
        stream: Option<Stats>,
    ) -> Sample {
        let ms = ((now - self.at) / 1_000_000).max(1);
        let shown = flips.saturating_sub(self.flips);
        *self = Self::new(now, flips);
        data.refresh_light();
        let loads: Vec<u64> = data.load.iter().flatten().copied().collect();
        Sample {
            cloud,
            fps_x10: Some(shown * 10_000 / ms),
            worst_ms: worst / 1_000_000,
            late,
            cpu: (!loads.is_empty()).then(|| loads.iter().sum::<u64>() / loads.len() as u64),
            gpu: match data.gpu {
                Seen::Value(g) if g.khz > 0 => Some((g.load.unwrap_or(0), g.khz)),
                _ => None,
            },
            mem_mb: data
                .mem
                .map(|m| ((m.total_pages - m.free_pages) * PAGE_SIZE) >> 20),
            stream: stream.filter(|_| cloud),
        }
    }
}

/// One tenth: `59.9`.
fn tenths(v: u64) -> String {
    format!("{}.{}", v / 10, v % 10)
}

/// Green below `warn`, amber below `bad`, red from there.
fn grade(v: u64, warn: u64, bad: u64) -> u32 {
    if v >= bad {
        BAD
    } else if v >= warn {
        WARN
    } else {
        GOOD
    }
}

impl Sample {
    /// The lines: a name, its value and the value's color.
    fn rows(&self) -> Vec<(&'static str, String, u32)> {
        let mut out = Vec::new();
        if self.cloud {
            match &self.stream {
                Some(st) => {
                    if let Some(rtt) = st.rtt_us {
                        let ms = rtt as u64 / 1000;
                        let mut v = format!("{} ms", ms);
                        if let Some(j) = st.jitter_us {
                            v += &format!("  jitter {}", j as u64 / 1000);
                        }
                        out.push(("Ping", v, grade(ms, 60, 120)));
                    }
                    if let Some(f) = st.fps_x10 {
                        let mut v = format!("{} fps", tenths(f as u64));
                        if let Some((w, h)) = st.size {
                            v += &format!("  {}x{}", w, h);
                        }
                        out.push(("Stream", v, WHITE));
                    }
                    if let Some(k) = st.kbps {
                        let c = if k == 0 { WARN } else { WHITE };
                        out.push(("Rate", format!("{} Mbit/s", tenths(k as u64 / 100)), c));
                    }
                    if let Some(l) = st.loss_pm {
                        out.push((
                            "Loss",
                            format!("{} %", tenths(l as u64)),
                            grade(l as u64, 5, 20),
                        ));
                    }
                }
                None => out.push(("Stream", String::from("waiting"), DIM)),
            }
        }
        let name = if self.cloud { "Screen" } else { "FPS" };
        match self.fps_x10 {
            Some(f) => {
                let mut v = tenths(f);
                if self.cloud {
                    v += " fps";
                }
                v += &format!("  worst {} ms", self.worst_ms);
                out.push((name, v, grade(self.late, 1, 5)));
            }
            None => out.push((name, String::from("-"), DIM)),
        }
        if let Some(c) = self.cpu {
            out.push(("CPU", format!("{} %", c), grade(c, 80, 95)));
        }
        if let Some((load, khz)) = self.gpu {
            out.push((
                "GPU",
                format!("{} %  {} MHz", load, khz / 1000),
                grade(load, 90, 101),
            ));
        }
        if let Some(m) = self.mem_mb {
            out.push(("RAM", format!("{} MB", m), WHITE));
        }
        out
    }
}

impl Ui {
    /// The panel at the top left, on the game: no zone, a touch there is
    /// the game's.
    pub(crate) fn perf_panel(&mut self, s: &mut Screen) {
        let Some(v) = self.perf.as_ref() else {
            return;
        };
        let rows = v.rows();
        let kind = if v.cloud { Icon::Cloud } else { Icon::Gauge };
        let title = if v.cloud { "Cloud game" } else { "Local game" };
        let h = HEAD + rows.len() * ROW + 12;
        s.round_a(X, Y, W, h, 14, BLACK, 200);
        icon(s, kind, X + 28, Y + HEAD / 2 + 2, WHITE, BLACK);
        s.label(X + 56, Y + HEAD / 2 + 2, title, Face::BodyB, WHITE);
        for (i, (name, value, c)) in rows.into_iter().enumerate() {
            let y = Y + HEAD + i * ROW + ROW / 2;
            s.label(X + 20, y, name, Face::Small, DIM);
            s.label_r(X + W - 20, y, &value, Face::Small, c);
        }
    }
}

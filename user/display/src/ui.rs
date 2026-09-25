//! The touch UI (docs/ui/home.html): a header, five pages chosen by a
//! tab bar, confirmations and short notices. Drawing records the touch
//! zones of what it drew; a tap is a press and a release on one zone.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::input::{TOUCH_DOWN, TOUCH_UP};
use libnx::abi::proto::{cpufreq, fan, gpu, power, usbcon as usb, wifi};

use crate::screen::{
    self, accent, palette, width, Bytes, Celsius, Hms, Ipv4, Screen, BAD, BG, DIM, EDGE, FG, GOOD,
    PANEL, PRESS, TERM_BG, TRACK, WARN,
};
use crate::stats::{Cpu, Data, Eth, Fan, Now, Seen};
use crate::term::Term;
use crate::Messages;

/// The layout is made for the Switch's 1280x720 (landscape).
const W: usize = 1280;
const H: usize = 720;
const HEAD: usize = 64;
const TABS: usize = 96;
const M: usize = 16;
/// Content area between the header and the tab bar.
const CY: usize = HEAD + M;
const CH: usize = H - HEAD - TABS - 2 * M;
const PAGES: [&str; 5] = ["HOME", "SYSTEM", "NETWORK", "TERMINAL", "LOGS"];
const SYSTEM: usize = 1;
const NETWORK: usize = 2;
pub const TERMINAL: usize = 3;
pub const LOGS: usize = 4;
/// What a header chip opens.
#[derive(Clone, Copy)]
enum Opens {
    Page(usize),
    Fan,
}
/// Header chips (fan, SD, Wi-Fi, Ethernet, USB) and what each opens.
const CHIPS: [(&str, Option<Opens>); 5] = [
    ("FAN", Some(Opens::Fan)),
    ("SD", None),
    ("WI-FI", Some(Opens::Page(NETWORK))),
    ("ETH", Some(Opens::Page(NETWORK))),
    ("USB", Some(Opens::Page(TERMINAL))),
];
/// Fan duty step of the FAN panel's buttons (%).
const FAN_STEP: u64 = 10;
const TAGLINE: &str = "AArch64 microkernel for Tegra X1";
const TOAST_NS: u64 = 2_500_000_000;

#[derive(Clone, Copy, PartialEq)]
enum Id {
    Tab(usize),
    /// Home tile: its slot and the page it opens.
    Tile(usize, usize),
    Chip(usize),
    Reboot,
    PowerOff,
    Scan,
    Retry,
    Leave,
    Back,
    No,
    Yes,
    LogSize,
    LogClear,
    TermSize,
    FanOpen,
    FanAuto,
    FanManual,
    FanDown,
    FanUp,
    FanClose,
    CpuOpen,
    /// A step of `cpufreq::STEPS_MHZ`, by index.
    CpuStep(usize),
    CpuBoot,
    CpuClose,
}

/// What a tap asks the service to do.
#[derive(Clone, Copy)]
pub enum Action {
    Power(u32),
    Scan,
    Retry,
    Leave,
    ClearLogs,
    /// `fan::SET`: `fan::AUTO` or a duty (%).
    Fan(u64),
    /// `cpufreq::SET`: a step (MHz) or `cpufreq::BOOT`.
    Cpu(u64),
}

struct Zone {
    id: Id,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

struct Modal {
    title: String,
    lines: [String; 2],
    ok: &'static str,
    tone: u32,
    action: Action,
}

/// One network of a scan.
pub struct ScanNet {
    pub ssid: String,
    pub channel: u8,
    /// -dBm.
    pub signal: u8,
    pub security: u8,
}

pub struct Ui {
    pub page: usize,
    pressed: Option<Id>,
    modal: Option<Modal>,
    toast: Option<(String, u64)>,
    /// Scan results, shown instead of the Wi-Fi status.
    pub scan: Option<Vec<ScanNet>>,
    /// A Wi-Fi request is running.
    pub busy: bool,
    log_scale: usize,
    term_scale: usize,
    /// Screen messages written since the LOGS page was last shown.
    pub unread: usize,
    /// Network of the Wi-Fi card, for the LEAVE confirmation.
    ssid: String,
    /// The FAN panel is open.
    fan_panel: bool,
    /// Fan state as last drawn: taps count from it.
    fan: Seen<Fan>,
    /// Fan requests in flight, and the last one (`fan::AUTO` or %).
    fan_sent: u32,
    fan_last: u64,
    /// The CPU panel is open.
    cpu_panel: bool,
    cpu: Seen<Cpu>,
    /// A clock change is running: the steps wait for it.
    cpu_sent: bool,
    zones: Vec<Zone>,
}

fn load_color(p: u64) -> u32 {
    match p {
        85.. => BAD,
        50.. => WARN,
        _ => GOOD,
    }
}

fn card(s: &mut Screen, x: usize, y: usize, w: usize, h: usize, title: &str, hot: bool) {
    s.fill(x, y, w, h, PANEL);
    s.frame(x, y, w, h, 2, if hot { accent() } else { EDGE });
    if !title.is_empty() {
        s.text(x + 16, y + 16, title, 2, accent(), PANEL);
    }
}

fn bar(s: &mut Screen, x: usize, y: usize, w: usize, h: usize, pct: u64, c: u32) {
    let f = w * pct.min(100) as usize / 100;
    s.fill(x, y, f, h, c);
    s.fill(x + f, y, w - f, h, TRACK);
}

/// Memory split: kernel, programs, free.
#[allow(clippy::too_many_arguments)]
fn mem_bar(
    s: &mut Screen,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    total: u64,
    kernel: u64,
    user: u64,
) {
    let part = |n: u64| ((w as u64 * n / total.max(1)) as usize).max(2);
    let (k, u) = (part(kernel), part(user));
    s.fill(x, y, k, h, palette(3));
    s.fill(x + k, y, u, h, palette(5));
    s.fill(x + k + u, y, w.saturating_sub(k + u), h, TRACK);
}

/// Legend square; the free part (TRACK) outlined to be seen on PANEL.
fn swatch(s: &mut Screen, x: usize, y: usize, c: u32) {
    s.fill(x, y, 12, 12, c);
    if c == TRACK {
        s.frame(x, y, 12, 12, 2, DIM);
    }
}

/// Label on the left, value on the right of one row.
fn kv(
    s: &mut Screen,
    mut x: usize,
    xr: usize,
    y: usize,
    name: &str,
    value: &str,
    square: Option<u32>,
) {
    if let Some(c) = square {
        swatch(s, x, y + 10, c);
        x += 24;
    }
    s.text(x, y, name, 2, DIM, PANEL);
    s.text_r(xr, y, value, 2, FG, PANEL);
}

/// Four bars from `x`, bottom at `y`.
fn signal_bars(s: &mut Screen, x: usize, y: usize, dbm: u64) {
    let n = match dbm {
        0..55 => 4,
        55..65 => 3,
        65..75 => 2,
        _ => 1,
    };
    for i in 0..4 {
        let h = 8 + i * 6;
        s.fill(x + i * 10, y - h, 6, h, if i < n { GOOD } else { TRACK });
    }
}

/// Framed word ending at `xr`; returns its width.
fn badge(s: &mut Screen, xr: usize, y: usize, text: &str, c: u32) -> usize {
    let w = width(text, 2) + 24;
    s.frame(xr - w, y, w, 40, 2, c);
    s.text(xr - w + 12, y + 4, text, 2, c, PANEL);
    w
}

fn usb_status(d: &Data) -> (String, u32) {
    match d.usb {
        Now::Missing => ("console not running".into(), DIM),
        Now::Busy => ("busy".into(), DIM),
        Now::Value(u) => {
            let speed = if u.state & usb::ST_HIGH_SPEED != 0 {
                "high speed"
            } else {
                "full speed"
            };
            if u.state & usb::ST_TERMINAL != 0 {
                (format!("terminal open, {}", speed), GOOD)
            } else if u.state & usb::ST_CONFIGURED != 0 {
                (format!("connected, {}, terminal closed", speed), WARN)
            } else if u.state & usb::ST_HOST != 0 {
                ("host seen, not configured".into(), WARN)
            } else {
                ("waiting for a PC".into(), DIM)
            }
        }
    }
}

fn usb_short(d: &Data) -> (&'static str, u32) {
    match d.usb {
        Now::Missing => ("not running", DIM),
        Now::Busy => ("busy", DIM),
        Now::Value(u) if u.state & usb::ST_TERMINAL != 0 => ("open", GOOD),
        Now::Value(u) if u.state & usb::ST_CONFIGURED != 0 => ("closed", WARN),
        Now::Value(u) if u.state & usb::ST_HOST != 0 => ("host seen", WARN),
        Now::Value(_) => ("no PC", DIM),
    }
}

fn wifi_short(d: &Data) -> (&'static str, u32) {
    match d.wifi {
        Seen::Unknown => ("starting", DIM),
        Seen::Missing => ("no Wi-Fi driver", DIM),
        Seen::Value(w) => match w.state {
            wifi::ST_CONNECTED => ("connected", GOOD),
            wifi::ST_CONNECTING => ("joining", WARN),
            wifi::ST_SCANNING => ("scanning", WARN),
            wifi::ST_IDLE => ("not connected", DIM),
            _ => ("no adapter", BAD),
        },
    }
}

fn eth_text(d: &Data, short: bool) -> (&'static str, u32) {
    match d.eth {
        Eth::Up if d.eth_name == "usb0" => ("shared by the PC", GOOD),
        Eth::Up => ("connected", GOOD),
        Eth::Dhcp if short => ("waiting for DHCP", WARN),
        Eth::Dhcp => ("link up, waiting for DHCP", WARN),
        Eth::NoSharing if short => ("no DHCP answer", WARN),
        Eth::NoSharing => ("no DHCP: sharing off on the PC?", WARN),
        Eth::Off if short => ("off on the PC", DIM),
        Eth::Off => ("link not turned on by the PC", DIM),
        Eth::NoPc if short => ("no PC", DIM),
        Eth::NoPc => ("no PC on USB-C", DIM),
        Eth::Missing => ("not running", DIM),
    }
}

/// Fan state: its mode and value, and the color it shows in.
fn fan_text(f: &Fan) -> (String, u32) {
    let (mode, c) = if f.state & fan::ST_HOT != 0 {
        ("hot", WARN)
    } else if f.state & fan::ST_NO_SENSOR != 0 {
        ("no sensor", WARN)
    } else if f.state & fan::ST_MANUAL != 0 {
        ("manual", FG)
    } else {
        ("auto", FG)
    };
    let text = match (f.percent, f.rpm) {
        (0, _) => format!("{}: off", mode),
        (p, Some(rpm)) => format!("{}: {}%, {} rpm", mode, p, rpm),
        (p, None) => format!("{}: {}%", mode, p),
    };
    (text, c)
}

/// Header square: spinning, forced (hot, no sensor), or off/missing.
fn fan_color(d: &Data) -> u32 {
    match d.fan {
        Seen::Value(f) if f.state & (fan::ST_HOT | fan::ST_NO_SENSOR) != 0 => WARN,
        Seen::Value(f) if f.percent > 0 => GOOD,
        _ => DIM,
    }
}

/// Clock in whole MHz ("?" unknown).
fn mhz(khz: u64) -> String {
    match khz {
        0 => "?".into(),
        k => format!("{}", (k + 500) / 1000),
    }
}

/// CPU clock and rail, and the color they show in.
fn cpu_text(c: &Cpu) -> (String, u32) {
    let rail = match c.uv {
        0 => String::new(),
        uv => format!(", {} mV", uv / 1000),
    };
    if c.state & cpufreq::ST_UNSUPPORTED != 0 {
        (format!("{} MHz, boot only", mhz(c.khz)), DIM)
    } else if c.state & cpufreq::ST_HOT != 0 {
        (format!("hot: {} MHz{}", mhz(c.khz), rail), WARN)
    } else if c.step == cpufreq::BOOT {
        (format!("boot: {} MHz{}", mhz(c.khz), rail), FG)
    } else {
        (format!("{} MHz{}", mhz(c.khz), rail), FG)
    }
}

/// The GPU row: off, its clock and rail, or where power-up failed.
fn gpu_text(g: &Seen<crate::stats::Gpu>) -> (String, u32) {
    const STAGES: [&str; 11] = [
        "",
        "rail",
        "clocks",
        "PLL",
        "MMU",
        "firmware",
        "secure boot",
        "FECS",
        "context",
        "channel",
        "ready",
    ];
    match g {
        Seen::Value(g) if g.state == gpu::ON => {
            let clock = match g.khz {
                0 => "bypass clock".into(),
                k => format!("{}.{} MHz", k / 1000, k % 1000 / 100),
            };
            (format!("{}, {} mV", clock, g.uv / 1000), GOOD)
        }
        Seen::Value(g) if g.state == gpu::FAILED => (
            format!("failed: {}", STAGES.get(g.stage as usize).unwrap_or(&"?")),
            BAD,
        ),
        Seen::Value(_) => ("off".into(), DIM),
        Seen::Missing => ("no GPU service".into(), DIM),
        Seen::Unknown => ("starting".into(), DIM),
    }
}

fn wifi_connected(d: &Data) -> Option<crate::stats::Wifi> {
    match d.wifi {
        Seen::Value(w) if w.state == wifi::ST_CONNECTED => Some(w),
        _ => None,
    }
}

impl Ui {
    pub fn new() -> Self {
        Self {
            page: 0,
            pressed: None,
            modal: None,
            toast: None,
            scan: None,
            busy: false,
            log_scale: 1,
            term_scale: 1,
            unread: 0,
            ssid: String::new(),
            fan_panel: false,
            fan: Seen::Unknown,
            fan_sent: 0,
            fan_last: fan::AUTO,
            cpu_panel: false,
            cpu: Seen::Unknown,
            cpu_sent: false,
            zones: Vec::new(),
        }
    }

    /// A fan request came back.
    pub fn fan_done(&mut self) {
        self.fan_sent = self.fan_sent.saturating_sub(1);
    }

    fn cpu_step(&self) -> Option<u64> {
        match self.cpu {
            Seen::Value(c) => Some(c.step),
            _ => None,
        }
    }

    /// A clock change came back.
    pub fn cpu_done(&mut self) {
        self.cpu_sent = false;
    }

    /// Duty set by hand (%) as the FAN panel shows it, none in auto:
    /// the last request while one is in flight.
    fn fan_manual(&self) -> Option<u64> {
        if self.fan_sent > 0 {
            (self.fan_last != fan::AUTO).then_some(self.fan_last)
        } else {
            match self.fan {
                Seen::Value(f) => f.manual,
                _ => None,
            }
        }
    }

    fn fan_set(&mut self, v: u64) -> Option<Action> {
        self.fan_sent += 1;
        self.fan_last = v;
        Some(Action::Fan(v))
    }

    /// Short notice above the tab bar.
    pub fn toast(&mut self, text: String, now: u64) {
        self.toast = Some((text, now + TOAST_NS));
    }

    /// When the screen changes by itself (a notice ends), if it will.
    pub fn deadline(&self) -> Option<u64> {
        self.toast.as_ref().map(|t| t.1)
    }

    fn zone(&mut self, id: Id, x: usize, y: usize, w: usize, h: usize) {
        self.zones.push(Zone { id, x, y, w, h });
    }

    fn hit(&self, x: usize, y: usize) -> Option<Id> {
        self.zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x < z.x + z.w && y >= z.y && y < z.y + z.h)
            .map(|z| z.id)
    }

    /// Touch event at landscape (x, y); a tap may ask for an action.
    pub fn touch(&mut self, code: u64, x: usize, y: usize) -> Option<Action> {
        match code {
            TOUCH_DOWN => {
                self.pressed = self.hit(x, y);
                None
            }
            TOUCH_UP => match (self.pressed.take(), self.hit(x, y)) {
                (Some(a), Some(b)) if a == b => self.tap(a),
                _ => None,
            },
            _ => None,
        }
    }

    fn go(&mut self, page: usize) {
        self.page = page;
        if page == LOGS {
            self.unread = 0;
        }
    }

    fn tap(&mut self, id: Id) -> Option<Action> {
        match id {
            Id::Tab(p) | Id::Tile(_, p) => self.go(p),
            Id::Chip(k) => match CHIPS[k].1 {
                Some(Opens::Page(p)) => self.go(p),
                Some(Opens::Fan) => self.fan_panel = true,
                None => {}
            },
            Id::FanOpen => self.fan_panel = true,
            Id::FanClose => self.fan_panel = false,
            Id::FanAuto if self.fan_manual().is_some() => return self.fan_set(fan::AUTO),
            // By hand from the duty running now.
            Id::FanManual if self.fan_manual().is_none() => {
                let Seen::Value(f) = self.fan else {
                    return None;
                };
                let p = f.percent;
                return self.fan_set(p);
            }
            Id::FanAuto | Id::FanManual => {}
            Id::FanDown | Id::FanUp => {
                let p = self.fan_manual()?;
                let q = match id {
                    Id::FanUp => (p / FAN_STEP + 1) * FAN_STEP,
                    _ => p.div_ceil(FAN_STEP).saturating_sub(1) * FAN_STEP,
                }
                .min(100);
                if q != p {
                    return self.fan_set(q);
                }
            }
            Id::CpuOpen => self.cpu_panel = true,
            Id::CpuClose => self.cpu_panel = false,
            Id::CpuStep(_) | Id::CpuBoot if self.cpu_sent => {}
            // Already there.
            Id::CpuStep(i) if self.cpu_step() == Some(cpufreq::STEPS_MHZ[i]) => {}
            Id::CpuBoot if self.cpu_step() == Some(cpufreq::BOOT) => {}
            Id::CpuStep(i) => {
                self.cpu_sent = true;
                return Some(Action::Cpu(cpufreq::STEPS_MHZ[i]));
            }
            Id::CpuBoot => {
                self.cpu_sent = true;
                return Some(Action::Cpu(cpufreq::BOOT));
            }
            Id::Reboot => {
                self.modal = Some(Modal {
                    title: "Reboot the Switch?".into(),
                    lines: [
                        "It goes back to RCM: inject nxk".into(),
                        "again from the PC.".into(),
                    ],
                    ok: "REBOOT",
                    tone: WARN,
                    action: Action::Power(power::REBOOT),
                })
            }
            Id::PowerOff => {
                self.modal = Some(Modal {
                    title: "Power off the Switch?".into(),
                    lines: [
                        "Power on goes to RCM (AutoRCM):".into(),
                        "inject nxk again from the PC.".into(),
                    ],
                    ok: "POWER OFF",
                    tone: BAD,
                    action: Action::Power(power::OFF),
                })
            }
            Id::Leave => {
                self.modal = Some(Modal {
                    title: format!("Leave {}?", self.ssid),
                    lines: [
                        "Rejoining needs the shell:".into(),
                        format!("wifi connect {} PASSPHRASE", self.ssid),
                    ],
                    ok: "LEAVE",
                    tone: WARN,
                    action: Action::Leave,
                })
            }
            Id::Scan => {
                self.busy = true;
                return Some(Action::Scan);
            }
            Id::Retry => {
                self.busy = true;
                self.scan = None;
                return Some(Action::Retry);
            }
            Id::Back => self.scan = None,
            Id::No => self.modal = None,
            Id::Yes => return self.modal.take().map(|m| m.action),
            Id::LogSize => self.log_scale = 3 - self.log_scale,
            Id::LogClear => return Some(Action::ClearLogs),
            Id::TermSize => self.term_scale = 3 - self.term_scale,
        }
        None
    }

    /// Whole frame into `s` (not yet on screen: `Screen::present`).
    pub fn draw(&mut self, s: &mut Screen, d: &Data, msgs: &Messages, term: &Term, now: u64) {
        self.zones.clear();
        s.clear(BG);
        self.header(s, d);
        match self.page {
            0 => self.home(s, d, term),
            SYSTEM => self.system(s, d),
            NETWORK => self.network(s, d),
            TERMINAL => self.terminal(s, d, term, now),
            _ => self.logs(s, msgs),
        }
        self.tab_bar(s);
        if matches!(&self.toast, Some((_, end)) if now >= *end) {
            self.toast = None;
        }
        if let Some((t, _)) = &self.toast {
            let y = H - TABS - M - 48;
            s.fill(M, y, W - 2 * M, 48, PANEL);
            s.frame(M, y, W - 2 * M, 48, 2, accent());
            s.text(2 * M, y + 8, t, 2, FG, PANEL);
        }
        self.fan = d.fan;
        self.cpu = d.cpu;
        if self.fan_panel {
            self.zones.clear();
            self.fan_panel(s);
        }
        if self.cpu_panel {
            self.zones.clear();
            self.cpu_panel(s);
        }
        if self.modal.is_some() {
            self.zones.clear();
            self.modal(s);
        }
    }

    /// Button; `on` false: drawn disabled, no zone.
    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        text: &str,
        tone: Option<u32>,
        on: bool,
    ) {
        let bg = if on && self.pressed == Some(id) {
            PRESS
        } else {
            PANEL
        };
        s.fill(x, y, w, h, bg);
        s.frame(x, y, w, h, 2, if on { tone.unwrap_or(DIM) } else { EDGE });
        let fg = if on { tone.unwrap_or(FG) } else { EDGE };
        s.text_c(x + w / 2, y + (h - 32) / 2, text, 2, fg, bg);
        if on {
            self.zone(id, x, y, w, h);
        }
    }

    /// A card that opens a page.
    #[allow(clippy::too_many_arguments)]
    fn tile(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        title: &str,
    ) {
        card(s, x, y, w, h, title, self.pressed == Some(id));
        self.zone(id, x, y, w, h);
    }

    fn header(&mut self, s: &mut Screen, d: &Data) {
        s.fill(0, 0, W, HEAD, PANEL);
        s.fill(0, HEAD - 2, W, 2, EDGE);
        s.logo(M, 7, 2);
        s.text(
            M + screen::logo_size().0 * 2 + 16,
            24,
            TAGLINE,
            1,
            DIM,
            PANEL,
        );
        let mut x = W - M;
        x -= s.text_r(x, 16, &format!("up {}", Hms(d.up_s)), 2, FG, PANEL) + 32;
        let (t, c) = match d.temps {
            Seen::Value((chip, _)) => (format!("{}", Celsius(chip)), FG),
            _ => ("--.-\u{b0}C".into(), DIM),
        };
        x -= s.text_r(x, 16, &t, 2, c, PANEL) + 32;
        let usb_c = usb_short(d).1;
        let colors = [
            fan_color(d),
            if matches!(d.disk, Seen::Value(_)) {
                GOOD
            } else {
                DIM
            },
            wifi_short(d).1,
            eth_text(d, true).1,
            usb_c,
        ];
        for (k, (name, opens)) in CHIPS.iter().enumerate() {
            let w = 20 + width(name, 2);
            let bx = x - w;
            let bg = if self.pressed == Some(Id::Chip(k)) {
                PRESS
            } else {
                PANEL
            };
            s.fill(bx - 8, 8, w + 16, HEAD - 18, bg);
            s.fill(bx, 26, 12, 12, colors[k]);
            s.text(
                bx + 20,
                16,
                name,
                2,
                if colors[k] == DIM { DIM } else { FG },
                bg,
            );
            if opens.is_some() {
                self.zone(Id::Chip(k), bx - 8, 0, w + 16, HEAD - 2);
            }
            x = bx - 24;
        }
    }

    fn tab_bar(&mut self, s: &mut Screen) {
        let (y, w) = (H - TABS, W / PAGES.len());
        for (i, name) in PAGES.iter().enumerate() {
            let (x, on) = (i * w, self.page == i);
            let bg = if self.pressed == Some(Id::Tab(i)) {
                PRESS
            } else if on {
                PANEL
            } else {
                BG
            };
            s.fill(x, y, w, TABS, bg);
            s.fill(
                x,
                y,
                w,
                if on { 4 } else { 2 },
                if on { accent() } else { EDGE },
            );
            if i > 0 && !on && self.page != i - 1 {
                s.fill(x, y + 24, 2, TABS - 48, EDGE);
            }
            let count = if i == LOGS && self.unread > 0 {
                format!(" {}", self.unread.min(99))
            } else {
                String::new()
            };
            let lx = x + (w - width(name, 2) - width(&count, 2)) / 2;
            let lw = s.text(lx, y + 32, name, 2, if on { accent() } else { DIM }, bg);
            s.text(lx + lw, y + 32, &count, 2, WARN, bg);
            self.zone(Id::Tab(i), x, y, w, TABS);
        }
    }

    fn home(&mut self, s: &mut Screen, d: &Data, term: &Term) {
        // Top: the machine's resources, three tiles; bottom: connectivity, two.
        let (h, yt) = ((CH - M) / 2, CY);
        let yb = CY + h + M;
        let w3 = (W - 4 * M) / 3;
        let (x1, x2, x3) = (M, 2 * M + w3, 3 * M + 2 * w3);
        let w3r = W - M - x3;
        let w = (W - 3 * M) / 2;
        let (xl, xr) = (M, 2 * M + w);

        self.tile(s, Id::Tile(0, SYSTEM), x1, yt, w3, h, "CPU");
        let on: Vec<u64> = d.load.iter().flatten().copied().collect();
        let avg = on.iter().sum::<u64>() / (on.len() as u64).max(1);
        s.text(
            x1 + 16,
            yt + 64,
            &format!("{}%", avg),
            4,
            load_color(avg),
            PANEL,
        );
        s.text(x1 + 16, yt + 136, "average", 2, DIM, PANEL);
        for (k, p) in d.load.iter().enumerate() {
            let y = yt + 16 + k * 40;
            s.text(
                x1 + 168,
                y,
                &format!("{}", k),
                2,
                if p.is_some() { FG } else { DIM },
                PANEL,
            );
            match p {
                Some(p) => {
                    bar(s, x1 + 200, y + 8, 112, 16, *p, load_color(*p));
                    s.text_r(x1 + w3 - 16, y, &format!("{}%", p), 2, FG, PANEL);
                }
                None => {
                    s.fill(x1 + 200, y + 8, 112, 16, TRACK);
                    s.text_r(x1 + w3 - 16, y, "off", 2, DIM, PANEL);
                }
            }
        }
        let line = format!("{} cores   {} processes", on.len(), d.procs);
        s.text(x1 + 16, yt + h - 48, &line, 2, DIM, PANEL);

        self.tile(s, Id::Tile(1, SYSTEM), x2, yt, w3, h, "MEMORY");
        if let Some(m) = d.mem {
            let b = |p: u64| p * PAGE_SIZE;
            let used = m.total_pages - m.free_pages;
            s.text_r(
                x2 + w3 - 16,
                yt + 16,
                &format!("{}", Bytes(b(m.total_pages))),
                2,
                DIM,
                PANEL,
            );
            let mw = s.text(
                x2 + 16,
                yt + 64,
                &format!("{}", Bytes(b(used))),
                4,
                FG,
                PANEL,
            );
            s.text(x2 + 32 + mw, yt + 96, "used", 2, DIM, PANEL);
            let user = used.saturating_sub(m.kernel_pages);
            mem_bar(
                s,
                x2 + 16,
                yt + 144,
                w3 - 32,
                24,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                x2 + 16,
                x2 + w3 - 16,
                yt + 176,
                "kernel",
                &format!("{}", Bytes(b(m.kernel_pages))),
                Some(palette(3)),
            );
            kv(
                s,
                x2 + 16,
                x2 + w3 - 16,
                yt + 208,
                "free",
                &format!("{}", Bytes(b(m.free_pages))),
                Some(TRACK),
            );
        }

        card(s, x3, yt, w3r, h, "STORAGE", false);
        match d.disk {
            Seen::Value((free, total)) => {
                let used = total.saturating_sub(free);
                let pct = used * 100 / total.max(1);
                s.text_r(x3 + w3r - 16, yt + 16, "SD card", 2, DIM, PANEL);
                let fw = s.text(x3 + 16, yt + 64, &format!("{}", Bytes(free)), 4, FG, PANEL);
                s.text(x3 + 32 + fw, yt + 96, "free", 2, DIM, PANEL);
                bar(
                    s,
                    x3 + 16,
                    yt + 144,
                    w3r - 32,
                    24,
                    pct,
                    if pct >= 90 { BAD } else { palette(5) },
                );
                kv(
                    s,
                    x3 + 16,
                    x3 + w3r - 16,
                    yt + 176,
                    "used",
                    &format!("{} ({}%)", Bytes(used), pct),
                    None,
                );
                kv(
                    s,
                    x3 + 16,
                    x3 + w3r - 16,
                    yt + 208,
                    "total",
                    &format!("{}", Bytes(total)),
                    None,
                );
            }
            Seen::Missing => {
                s.text(x3 + 16, yt + 72, "no card", 2, DIM, PANEL);
            }
            Seen::Unknown => {
                s.text(x3 + 16, yt + 72, "starting", 2, DIM, PANEL);
            }
        }

        self.tile(s, Id::Tile(2, NETWORK), xl, yb, w, h, "NETWORK");
        match d.active() {
            Some(n) if n.addr != 0 => {
                s.text_r(xl + w - 16, yb + 16, n.link_name(), 2, DIM, PANEL);
                let iw = s.text(
                    xl + 16,
                    yb + 64,
                    &format!("{}", Ipv4(n.addr)),
                    3,
                    GOOD,
                    PANEL,
                );
                s.text(
                    xl + 16 + iw,
                    yb + 80,
                    &format!("/{}", n.prefix),
                    2,
                    DIM,
                    PANEL,
                );
            }
            Some(n) => {
                s.text_r(xl + w - 16, yb + 16, n.link_name(), 2, DIM, PANEL);
                s.text(xl + 16, yb + 72, "waiting for DHCP", 2, WARN, PANEL);
            }
            None => {
                s.text(xl + 16, yb + 72, "offline", 2, DIM, PANEL);
            }
        }
        s.text(xl + 16, yb + 136, "Eth", 2, DIM, PANEL);
        let (et, ec) = eth_text(d, true);
        s.fill(xl + 128, yb + 146, 12, 12, ec);
        s.text(
            xl + 152,
            yb + 136,
            et,
            2,
            if ec == DIM { DIM } else { FG },
            PANEL,
        );
        s.text(xl + 16, yb + 184, "Wi-Fi", 2, DIM, PANEL);
        match (d.wifi, wifi_connected(d)) {
            (_, Some(wf)) => {
                signal_bars(s, xl + 128, yb + 212, wf.signal);
                let t = format!("{}  ch {}  -{} dBm", wf.ssid(), wf.channel, wf.signal);
                s.text(xl + 176, yb + 184, &t, 2, FG, PANEL);
            }
            (Seen::Value(wf), None) if wf.state == wifi::ST_CONNECTING => {
                s.text(
                    xl + 128,
                    yb + 184,
                    &format!("joining {}", wf.ssid()),
                    2,
                    WARN,
                    PANEL,
                );
            }
            _ => {
                let (t, c) = wifi_short(d);
                s.text(xl + 128, yb + 184, t, 2, c, PANEL);
            }
        }

        self.tile(s, Id::Tile(3, TERMINAL), xr, yb, w, h, "TERMINAL");
        let (ut, uc) = usb_short(d);
        s.text_r(xr + w - 16, yb + 16, ut, 2, uc, PANEL);
        let last = term.commands(3);
        if last.is_empty() {
            s.text(xr + 16, yb + 72, "no command yet", 2, DIM, PANEL);
        }
        for (i, c) in last.iter().enumerate() {
            let y = yb + 64 + i * 40;
            let x = xr + 16 + s.text(xr + 16, y, "% ", 2, DIM, PANEL);
            let line: String = if c.line.chars().count() > 20 {
                c.line
                    .chars()
                    .take(19)
                    .chain(core::iter::once('~'))
                    .collect()
            } else {
                c.line.clone()
            };
            s.text(x, y, &line, 2, FG, PANEL);
            match &c.error {
                Some(e) => s.text_r(xr + w - 16, y, e, 2, BAD, PANEL),
                None => s.text_r(xr + w - 16, y, &c.time(), 2, DIM, PANEL),
            };
        }
        if let Now::Value(u) = d.usb {
            let t = format!("in {}   out {}", Bytes(u.rx), Bytes(u.tx));
            s.text(xr + 16, yb + h - 48, &t, 2, DIM, PANEL);
        }
    }

    fn system(&mut self, s: &mut Screen, d: &Data) {
        let w = (W - 3 * M) / 2;
        let (xl, xr) = (M, 2 * M + w);
        card(s, xl, CY, w, CH, "PROCESSOR", false);
        s.text_r(
            xl + w - 16,
            CY + 16,
            &format!("up {}", Hms(d.up_s)),
            2,
            DIM,
            PANEL,
        );
        for (k, p) in d.load.iter().enumerate() {
            let y = CY + 64 + k * 44;
            s.text(
                xl + 16,
                y,
                &format!("CPU{}", k),
                2,
                if p.is_some() { FG } else { DIM },
                PANEL,
            );
            match p {
                Some(p) => {
                    bar(s, xl + 104, y + 4, 400, 24, *p, load_color(*p));
                    s.text_r(xl + w - 16, y, &format!("{}%", p), 2, FG, PANEL);
                }
                None => {
                    s.fill(xl + 104, y + 4, 400, 24, TRACK);
                    s.text_r(xl + w - 16, y, "off", 2, DIM, PANEL);
                }
            }
        }
        // The clock row opens the CPU panel.
        let fy = CY + 246;
        let bg = if self.pressed == Some(Id::CpuOpen) {
            PRESS
        } else {
            PANEL
        };
        s.fill(xl + 8, fy - 8, w - 16, 48, bg);
        s.text(xl + 16, fy, "clock", 2, DIM, bg);
        let (t, c) = match d.cpu {
            Seen::Value(c) => cpu_text(&c),
            Seen::Missing => ("no clock control".into(), DIM),
            Seen::Unknown => ("starting".into(), DIM),
        };
        s.text_r(xl + w - 48, fy, &t, 2, c, bg);
        s.text_r(xl + w - 16, fy, ">", 2, DIM, bg);
        self.zone(Id::CpuOpen, xl + 8, fy - 8, w - 16, 48);
        let (t, c) = gpu_text(&d.gpu);
        s.text(xl + 16, CY + 294, "gpu", 2, DIM, PANEL);
        s.text_r(xl + w - 48, CY + 294, &t, 2, c, PANEL);
        s.fill(xl + 16, CY + 334, w - 32, 2, EDGE);
        s.text(xl + 16, CY + 344, "ACTIVITY", 2, accent(), PANEL);
        let rows = [
            ("syscalls/s", d.rates[0]),
            ("interrupts/s", d.rates[1]),
            ("context switches/s", d.rates[2]),
            ("processes", d.procs),
        ];
        for (i, (n, v)) in rows.iter().enumerate() {
            kv(
                s,
                xl + 16,
                xl + w - 16,
                CY + 380 + i * 36,
                n,
                &format!("{}", v),
                None,
            );
        }

        card(s, xr, CY, w, 256, "MEMORY", false);
        if let Some(m) = d.mem {
            let b = |p: u64| format!("{}", Bytes(p * PAGE_SIZE));
            let used = m.total_pages - m.free_pages;
            let user = used.saturating_sub(m.kernel_pages);
            s.text_r(xr + w - 16, CY + 16, &b(m.total_pages), 2, DIM, PANEL);
            mem_bar(
                s,
                xr + 16,
                CY + 64,
                w - 32,
                32,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                xr + 16,
                xr + w - 16,
                CY + 120,
                "kernel",
                &b(m.kernel_pages),
                Some(palette(3)),
            );
            kv(
                s,
                xr + 16,
                xr + w - 16,
                CY + 160,
                "programs",
                &b(user),
                Some(palette(5)),
            );
            kv(
                s,
                xr + 16,
                xr + w - 16,
                CY + 200,
                "free",
                &b(m.free_pages),
                Some(TRACK),
            );
        }

        let py = CY + 256 + M;
        card(s, xr, py, w, 256, "POWER", false);
        match d.temps {
            Seen::Value((chip, package)) => {
                kv(
                    s,
                    xr + 16,
                    xr + w - 16,
                    py + 60,
                    "chip temperature",
                    &format!("{}", Celsius(chip)),
                    None,
                );
                kv(
                    s,
                    xr + 16,
                    xr + w - 16,
                    py + 98,
                    "package temperature",
                    &format!("{}", Celsius(package)),
                    None,
                );
            }
            Seen::Missing => {
                s.text(xr + 16, py + 60, "no temperature sensor", 2, DIM, PANEL);
            }
            Seen::Unknown => {
                s.text(xr + 16, py + 60, "starting", 2, DIM, PANEL);
            }
        }
        // The fan row opens the FAN panel.
        let fy = py + 136;
        let bg = if self.pressed == Some(Id::FanOpen) {
            PRESS
        } else {
            PANEL
        };
        s.fill(xr + 8, fy - 8, w - 16, 48, bg);
        s.text(xr + 16, fy, "fan", 2, DIM, bg);
        let (t, c) = match d.fan {
            Seen::Value(f) => fan_text(&f),
            Seen::Missing => ("no fan control".into(), DIM),
            Seen::Unknown => ("starting".into(), DIM),
        };
        s.text_r(xr + w - 48, fy, &t, 2, c, bg);
        s.text_r(xr + w - 16, fy, ">", 2, DIM, bg);
        self.zone(Id::FanOpen, xr + 8, fy - 8, w - 16, 48);
        let (bw, bh) = ((w - 48) / 2, 64);
        let by = py + 256 - 16 - bh;
        self.button(
            s,
            Id::Reboot,
            xr + 16,
            by,
            bw,
            bh,
            "REBOOT",
            Some(WARN),
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            xr + 32 + bw,
            by,
            bw,
            bh,
            "POWER OFF",
            Some(BAD),
            true,
        );
    }

    /// ACTIVE on the link net uses, STANDBY on another usable one; returns the width.
    fn link_badge(
        s: &mut Screen,
        d: &Data,
        xr: usize,
        y: usize,
        name: &str,
        usable: bool,
    ) -> usize {
        match d.active() {
            Some(n) if n.link_name() == name => badge(s, xr, y, "ACTIVE", GOOD),
            _ if usable => badge(s, xr, y, "STANDBY", DIM),
            _ => 0,
        }
    }

    fn network(&mut self, s: &mut Screen, d: &Data) {
        let w = (W - 3 * M) / 2;
        let (xl, xr) = (M, 2 * M + w);
        card(s, M, CY, W - 2 * M, 176, "INTERNET", false);
        match d.active() {
            Some(n) => {
                let t = format!(
                    "rx {}   tx {}   {} sockets",
                    Bytes(n.rx),
                    Bytes(n.tx),
                    n.sockets
                );
                s.text_r(W - 2 * M, CY + 16, &t, 2, DIM, PANEL);
                let dash = |a: u32| {
                    if a == 0 {
                        "-".into()
                    } else {
                        format!("{}", Ipv4(a))
                    }
                };
                if n.addr == 0 {
                    s.text(2 * M, CY + 64, "waiting for DHCP", 2, WARN, PANEL);
                } else {
                    let iw = s.text(2 * M, CY + 56, &format!("{}", Ipv4(n.addr)), 3, GOOD, PANEL);
                    s.text(
                        2 * M + iw,
                        CY + 72,
                        &format!("/{}", n.prefix),
                        2,
                        DIM,
                        PANEL,
                    );
                    s.text(2 * M, CY + 120, "online", 2, GOOD, PANEL);
                }
                let via = format!("{}, {}", n.link_name(), n.link_label());
                kv(s, xr + 16, W - 2 * M, CY + 56, "via", &via, None);
                kv(
                    s,
                    xr + 16,
                    W - 2 * M,
                    CY + 92,
                    "gateway",
                    &dash(n.gateway),
                    None,
                );
                kv(s, xr + 16, W - 2 * M, CY + 128, "DNS", &dash(n.dns), None);
            }
            None => {
                let t = match d.net {
                    Now::Busy => "busy",
                    _ => "no sharing on USB-C, no Wi-Fi network",
                };
                s.text(2 * M, CY + 56, "offline", 3, DIM, PANEL);
                s.text(2 * M, CY + 120, t, 2, DIM, PANEL);
            }
        }

        let (y0, h) = (CY + 176 + M, CH - 176 - M);
        card(s, xl, y0, w, h, "USB-C ETHERNET", false);
        let name = if d.eth_name.is_empty() {
            "usb0"
        } else {
            d.eth_name
        };
        Self::link_badge(s, d, xl + w - 16, y0 + 12, name, d.eth == Eth::Up);
        let (et, ec) = eth_text(d, false);
        s.text(xl + 16, y0 + 64, et, 2, ec, PANEL);
        if d.eth != Eth::Missing {
            let mac = d.eth_mac.to_le_bytes();
            let mac = if d.eth_mac == 0 {
                "-".into()
            } else {
                format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                )
            };
            let speed = match d.usb {
                Now::Value(u) if d.eth != Eth::NoPc && name == "usb0" => {
                    if u.state & usb::ST_HIGH_SPEED != 0 {
                        "high speed, 480 Mb/s"
                    } else {
                        "full speed, 12 Mb/s"
                    }
                }
                _ => "-",
            };
            kv(s, xl + 16, xl + w - 16, y0 + 112, "interface", name, None);
            kv(s, xl + 16, xl + w - 16, y0 + 150, "MAC", &mac, None);
            kv(s, xl + 16, xl + w - 16, y0 + 188, "USB", speed, None);
        }
        let hint = match d.eth {
            Eth::NoSharing => Some([
                "on the PC: share its connection",
                "(nmcli: ipv4.method shared)",
            ]),
            Eth::Off => Some(["the PC has no CDC-ECM driver", "(Linux and macOS have one)"]),
            Eth::NoPc => Some(["plug the Switch into a PC that", "shares its connection"]),
            _ => None,
        };
        for (i, t) in hint.iter().flatten().enumerate() {
            s.text(xl + 16, y0 + 240 + i * 36, t, 2, DIM, PANEL);
        }

        card(s, xr, y0, w, h, "WI-FI", false);
        let connected = wifi_connected(d);
        let bw = Self::link_badge(s, d, xr + w - 16, y0 + 12, "wlan0", connected.is_some());
        let (by, btw) = (y0 + h - 16 - 64, (w - 64) / 3);
        let state = match d.wifi {
            Seen::Value(wf) => wf.state,
            _ => wifi::ST_DOWN,
        };
        let adapter = matches!(d.wifi, Seen::Value(_)) && state != wifi::ST_DOWN;
        if let Some(nets) = &self.scan {
            let t = format!("{} networks", nets.len());
            s.text_r(xr + w - 32 - bw, y0 + 16, &t, 2, DIM, PANEL);
            for (i, n) in nets.iter().take(5).enumerate() {
                let y = y0 + 60 + i * 36;
                let cur = connected.is_some_and(|c| c.ssid() == n.ssid);
                let ssid: String = n.ssid.chars().take(14).collect();
                s.text(xr + 16, y, &ssid, 2, if cur { GOOD } else { FG }, PANEL);
                let sec = match n.security {
                    wifi::SEC_OPEN => "open",
                    wifi::SEC_WEP => "WEP",
                    wifi::SEC_WPA => "WPA",
                    wifi::SEC_WPA2 => "WPA2",
                    _ => "WPA3",
                };
                let t = format!("ch {:>2}  -{} dBm  {:>4}", n.channel, n.signal, sec);
                s.text_r(xr + w - 16, y, &t, 2, DIM, PANEL);
            }
            let busy = self.busy;
            self.button(s, Id::Back, xr + 16, by, btw, 64, "BACK", None, true);
            self.button(s, Id::Scan, xr + 32 + btw, by, btw, 64, "SCAN", None, !busy);
        } else {
            if let Some(wf) = connected {
                signal_bars(s, xr + w - 32 - bw - 36, y0 + 44, wf.signal);
            }
            let (t, c) = wifi_short(d);
            s.text(xr + 16, y0 + 64, t, 2, c, PANEL);
            if let Seen::Value(wf) = d.wifi {
                if matches!(wf.state, wifi::ST_CONNECTED | wifi::ST_CONNECTING) {
                    s.text(xr + 16, y0 + 100, wf.ssid(), 3, FG, PANEL);
                    self.ssid = wf.ssid().into();
                }
                if wf.state == wifi::ST_CONNECTED {
                    kv(
                        s,
                        xr + 16,
                        xr + w - 16,
                        y0 + 164,
                        "channel",
                        &format!("{}", wf.channel),
                        None,
                    );
                    kv(
                        s,
                        xr + 16,
                        xr + w - 16,
                        y0 + 202,
                        "signal",
                        &format!("-{} dBm", wf.signal),
                        None,
                    );
                } else if wf.state == wifi::ST_IDLE {
                    s.text(
                        xr + 16,
                        y0 + 112,
                        "join: wifi connect SSID (shell)",
                        2,
                        DIM,
                        PANEL,
                    );
                }
            }
            let (busy, joined) = (
                self.busy,
                matches!(state, wifi::ST_CONNECTED | wifi::ST_CONNECTING),
            );
            let scan_ok = adapter && state != wifi::ST_SCANNING && !busy;
            let retry_ok = !matches!(d.wifi, Seen::Missing | Seen::Unknown) && !busy;
            self.button(s, Id::Scan, xr + 16, by, btw, 64, "SCAN", None, scan_ok);
            self.button(
                s,
                Id::Retry,
                xr + 32 + btw,
                by,
                btw,
                64,
                "RETRY",
                None,
                retry_ok,
            );
            self.button(
                s,
                Id::Leave,
                xr + 48 + 2 * btw,
                by,
                btw,
                64,
                "LEAVE",
                Some(WARN),
                joined && !busy,
            );
        }
    }

    fn terminal(&mut self, s: &mut Screen, d: &Data, term: &Term, now: u64) {
        let (x, y, w, h) = (M, CY, W - 2 * M, CH);
        s.fill(x, y, w, h, TERM_BG);
        s.frame(x, y, w, h, 2, EDGE);
        s.fill(x + 2, y + 2, w - 4, 54, PANEL);
        s.fill(x + 2, y + 56, w - 4, 2, EDGE);
        let (ut, uc) = usb_status(d);
        s.fill(2 * M, y + 22, 12, 12, uc);
        s.text(
            2 * M + 24,
            y + 12,
            &ut,
            2,
            if uc == DIM { DIM } else { FG },
            PANEL,
        );
        let bx = W - 2 * M - 144;
        let label = if self.term_scale == 1 {
            "TEXT x2"
        } else {
            "TEXT x1"
        };
        self.button(s, Id::TermSize, bx, y + 8, 144, 40, label, None, true);
        if let Now::Value(u) = d.usb {
            let t = format!("in {}  out {}", Bytes(u.rx), Bytes(u.tx));
            let tw = s.text_r(bx - 16, y + 12, &t, 2, DIM, PANEL);
            if u.errors != 0 {
                s.text_r(
                    bx - 40 - tw,
                    y + 12,
                    &format!("{} errors", u.errors),
                    2,
                    BAD,
                    PANEL,
                );
            }
        }
        let sc = self.term_scale;
        let (x0, y0) = (2 * M, y + 72);
        let cols = (w - 32) / (8 * sc);
        let rows = (h - 72 - 12) / (16 * sc);
        let live = matches!(d.usb, Now::Value(u) if u.state & usb::ST_TERMINAL != 0);
        let notice = (!live).then(|| format!("-- {} --", ut));
        for (i, r) in term.rows(cols, rows, notice).iter().enumerate() {
            let ry = y0 + i * 16 * sc;
            let mut cx = x0;
            for (t, c) in &r.segs {
                cx += s.text(cx, ry, t, sc, *c, TERM_BG);
            }
            let mut rx = x0 + cols * 8 * sc;
            for (t, c) in r.right.iter().rev() {
                rx -= s.text_r(rx, ry, t, sc, *c, TERM_BG);
            }
            if r.cursor && (now / 500_000_000).is_multiple_of(2) {
                s.fill(cx, ry, 8 * sc, 16 * sc, FG);
            }
        }
    }

    fn logs(&mut self, s: &mut Screen, msgs: &Messages) {
        self.unread = 0;
        card(s, M, CY, W - 2 * M, CH, "MESSAGES", false);
        let cx = W - 2 * M - 112;
        let label = if self.log_scale == 1 {
            "TEXT x2"
        } else {
            "TEXT x1"
        };
        self.button(
            s,
            Id::LogClear,
            cx,
            CY + 8,
            112,
            48,
            "CLEAR",
            None,
            !msgs.is_empty(),
        );
        self.button(
            s,
            Id::LogSize,
            cx - 16 - 144,
            CY + 8,
            144,
            48,
            label,
            None,
            true,
        );
        let sc = self.log_scale;
        let cols = (W - 4 * M) / (8 * sc);
        let rows = (CH - 80) / (16 * sc);
        let mut wrapped: Vec<String> = Vec::with_capacity(rows + 8);
        for l in msgs.lines().iter().rev() {
            if wrapped.len() >= rows {
                break;
            }
            let mut parts: Vec<String> = Vec::new();
            let chars: Vec<char> = l.chars().collect();
            if chars.is_empty() {
                parts.push(String::new());
            }
            for chunk in chars.chunks(cols) {
                parts.push(chunk.iter().collect());
            }
            wrapped.extend(parts.into_iter().rev());
        }
        wrapped.reverse();
        if wrapped.is_empty() {
            s.text(2 * M, CY + 64, "no messages", 2, DIM, PANEL);
        }
        for (r, t) in wrapped[wrapped.len().saturating_sub(rows)..]
            .iter()
            .enumerate()
        {
            s.text(2 * M, CY + 64 + r * 16 * sc, t, sc, FG, PANEL);
        }
    }

    /// FAN panel: auto or by hand, the duty in `FAN_STEP` steps. The
    /// fan service still runs it full speed when the SoC is hot.
    fn fan_panel(&mut self, s: &mut Screen) {
        let (x, y, w, h) = (240, 96, 800, 528);
        s.fill(x - 8, y - 8, w + 16, h + 16, BG);
        s.fill(x, y, w, h, PANEL);
        s.frame(x, y, w, h, 4, accent());
        s.text(x + 32, y + 32, "FAN", 2, accent(), PANEL);
        let bh = 72;
        if let Seen::Value(f) = self.fan {
            let mut state = String::new();
            if let Some(t) = f.temp {
                state = format!("SoC {}", Celsius(t));
            }
            if let Some(rpm) = f.rpm {
                state = format!("{}   {} rpm", state, rpm);
            }
            s.text_r(x + w - 32, y + 32, &state, 2, FG, PANEL);

            let manual = self.fan_manual();
            let bw = (w - 96) / 2;
            for (k, (id, name, on)) in [
                (Id::FanAuto, "AUTO", manual.is_none()),
                (Id::FanManual, "MANUAL", manual.is_some()),
            ]
            .into_iter()
            .enumerate()
            {
                let bx = x + 32 + k * (bw + 32);
                let tone = on.then(accent);
                self.button(s, id, bx, y + 96, bw, bh, name, tone, true);
                if on {
                    s.fill(bx + 16, y + 96 + bh - 12, bw - 32, 4, accent());
                }
            }

            let forced = f.state & (fan::ST_HOT | fan::ST_NO_SENSOR) != 0;
            let (sw, sh, sy) = (160, 112, y + 200);
            let shown = manual.unwrap_or(f.percent);
            let can = |ok: bool| manual.is_some() && ok;
            self.button(
                s,
                Id::FanDown,
                x + 32,
                sy,
                sw,
                sh,
                "-10%",
                None,
                can(shown > 0),
            );
            self.button(
                s,
                Id::FanUp,
                x + w - 32 - sw,
                sy,
                sw,
                sh,
                "+10%",
                None,
                can(shown < 100),
            );
            let c = if forced { WARN } else { FG };
            s.text_c(x + w / 2, sy + 4, &format!("{}%", shown), 4, c, PANEL);
            let (bx, bw) = (x + 32 + sw + 24, w - 2 * (32 + sw + 24));
            bar(
                s,
                bx,
                sy + 88,
                bw,
                16,
                f.percent,
                if forced { WARN } else { GOOD },
            );

            let (note, c) = if f.state & fan::ST_HOT != 0 {
                ("SoC hot: full speed until below 57\u{b0}C", WARN)
            } else if f.state & fan::ST_NO_SENSOR != 0 {
                ("No temperature: at least 43%", WARN)
            } else if manual.is_some() {
                ("Full speed anyway from 60\u{b0}C", DIM)
            } else {
                ("Follows the SoC temperature", DIM)
            };
            s.text(x + 32, y + 352, note, 2, c, PANEL);
        } else {
            let t = match self.fan {
                Seen::Missing => "no fan control",
                _ => "starting",
            };
            s.text(x + 32, y + 96, t, 2, DIM, PANEL);
        }
        let cw = 240;
        self.button(
            s,
            Id::FanClose,
            x + w - 32 - cw,
            y + h - 32 - bh,
            cw,
            bh,
            "CLOSE",
            None,
            true,
        );
    }

    /// CPU panel: sys-clk's steps, those needing more than the rail's
    /// maximum on this chip greyed out, or the loader's clock (BOOT).
    fn cpu_panel(&mut self, s: &mut Screen) {
        let (x, y, w, h) = (240, 96, 800, 528);
        s.fill(x - 8, y - 8, w + 16, h + 16, BG);
        s.fill(x, y, w, h, PANEL);
        s.frame(x, y, w, h, 4, accent());
        s.text(x + 32, y + 32, "CPU CLOCK", 2, accent(), PANEL);
        let bh = 72;
        match self.cpu {
            Seen::Value(c) if c.state & cpufreq::ST_UNSUPPORTED == 0 => {
                let (t, tone) = cpu_text(&c);
                s.text_r(x + w - 32, y + 32, &t, 2, tone, PANEL);
                let (cols, gap) = (6, 16);
                let bw = (w - 64 - (cols - 1) * gap) / cols;
                let mut top = 0;
                for (i, &m) in cpufreq::STEPS_MHZ.iter().enumerate() {
                    let (bx, by) = (
                        x + 32 + (i % cols) * (bw + gap),
                        y + 96 + (i / cols) * (bh + gap),
                    );
                    let runs = c.allowed & 1 << i != 0;
                    if runs {
                        top = m;
                    }
                    let set = c.step == m;
                    let label = format!("{}", m);
                    let on = runs && !self.cpu_sent;
                    self.button(
                        s,
                        Id::CpuStep(i),
                        bx,
                        by,
                        bw,
                        bh,
                        &label,
                        set.then(accent),
                        on,
                    );
                    if set {
                        s.fill(bx + 16, by + bh - 12, bw - 32, 4, accent());
                    }
                }
                let by = y + 96 + 2 * (bh + gap);
                let boot = c.step == cpufreq::BOOT;
                self.button(
                    s,
                    Id::CpuBoot,
                    x + 32,
                    by,
                    240,
                    bh,
                    "BOOT",
                    boot.then(accent),
                    !self.cpu_sent,
                );
                if boot {
                    s.fill(x + 48, by + bh - 12, 208, 4, accent());
                }
                s.text(x + 296, by + 20, "the loader's clock", 2, DIM, PANEL);
                let (note, nc) = if c.state & cpufreq::ST_HOT != 0 {
                    ("SoC hot: back to BOOT", WARN)
                } else {
                    ("Back to BOOT at 80\u{b0}C", DIM)
                };
                s.text(x + 32, y + 368, note, 2, nc, PANEL);
                let limit = format!("speedo {}: {} MHz max", c.speedo, top);
                s.text(x + 32, y + 408, &limit, 2, DIM, PANEL);
            }
            Seen::Value(c) => {
                s.text(x + 32, y + 96, &format!("{} MHz", mhz(c.khz)), 2, FG, PANEL);
                s.text(
                    x + 32,
                    y + 144,
                    "Boot clock only on this console",
                    2,
                    DIM,
                    PANEL,
                );
            }
            Seen::Missing | Seen::Unknown => {
                let t = match self.cpu {
                    Seen::Missing => "no clock control",
                    _ => "starting",
                };
                s.text(x + 32, y + 96, t, 2, DIM, PANEL);
            }
        }
        let cw = 240;
        self.button(
            s,
            Id::CpuClose,
            x + w - 32 - cw,
            y + h - 32 - bh,
            cw,
            bh,
            "CLOSE",
            None,
            true,
        );
    }

    fn modal(&mut self, s: &mut Screen) {
        let Some(m) = self.modal.take() else { return };
        let (x, y, w, h) = (240, 180, 800, 320);
        s.fill(x - 8, y - 8, w + 16, h + 16, BG);
        s.fill(x, y, w, h, PANEL);
        s.frame(x, y, w, h, 4, m.tone);
        s.text(x + 32, y + 32, &m.title, 2, m.tone, PANEL);
        for (i, t) in m.lines.iter().enumerate() {
            s.text(x + 32, y + 88 + i * 40, t, 2, FG, PANEL);
        }
        let (bw, by) = ((w - 96) / 2, y + h - 32 - 72);
        self.button(s, Id::No, x + 32, by, bw, 72, "CANCEL", None, true);
        self.button(
            s,
            Id::Yes,
            x + 64 + bw,
            by,
            bw,
            72,
            m.ok,
            Some(m.tone),
            true,
        );
        self.modal = Some(m);
    }
}

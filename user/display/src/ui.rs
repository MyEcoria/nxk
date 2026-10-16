//! The touch UI (docs/ui/home.html is its mockup): a home with the game
//! tiles and a dock, a quick settings panel, settings, and the pages
//! (network, system, terminal, logs) opened from the dock. Dark or light.
//! Drawing records the touch zones of what it drew; a tap is a press and
//! a release on one zone, a slider follows the finger.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::input::{
    BTN_A, BTN_B, BTN_DOWN, BTN_HOME, BTN_L, BTN_LEFT, BTN_MINUS, BTN_PLUS, BTN_R, BTN_RIGHT,
    BTN_UP, BTN_ZL, BTN_ZR, TOUCH_DOWN, TOUCH_MOVE, TOUCH_UP,
};
use libnx::abi::proto::{cpufreq, fan, gpu, power, usbcon as usb, wifi};

use crate::backlight::MIN_PCT;
use crate::config::{Config, SLEEP_MIN};
use crate::font::{self, Face};
use crate::games::GameInfo;
use crate::screen::{self, mix, Celsius, Screen};
use crate::stats::{Cpu, Data, Eth, Fan, Gpu, Now, Seen};
use crate::term::Term;
use crate::theme::{self, pal};
use crate::widgets::{
    bars_for, card, fit, icon, press, signal_bars, slider, switch, tone_color, vc, Icon, Tone,
};
use crate::Messages;

/// The layout is made for the Switch's 1280x720 (landscape).
pub const W: usize = 1280;
pub const H: usize = 720;
/// Side margin, content top and bottom (above the hint bar).
pub const M: usize = 48;
pub const CY: usize = 112;
pub const CB: usize = H - 72;
pub const CH: usize = CB - CY;

pub const LIBRARY: usize = 0;
pub const SETTINGS: usize = 1;
pub const NETWORK: usize = 2;
pub const SYSTEM: usize = 3;
pub const TERMINAL: usize = 4;
pub const LOGS: usize = 5;
const TITLES: [&str; 6] = ["Games", "Settings", "Network", "System", "Terminal", "Logs"];
const TAGLINE: &str = "nxk \u{b7} AArch64 microkernel for Tegra X1";
/// Fan duty step of the FAN sheet's buttons (%).
const FAN_STEP: u64 = 10;
const TOAST_NS: u64 = 2_500_000_000;
/// Brightness step of the pad on a slider.
const PAD_STEP: u8 = 5;

/// Dock: icon, label, and what it opens.
const DOCK: [(Icon, &str); 7] = [
    (Icon::Gear, "Settings"),
    (Icon::Globe, "Network"),
    (Icon::Chip, "System"),
    (Icon::Terminal, "Terminal"),
    (Icon::Logs, "Logs"),
    (Icon::Moon, "Sleep"),
    (Icon::Power, "Power"),
];

/// Settings categories.
const CATS: [&str; 6] = [
    "Display",
    "Sound",
    "Network",
    "Performance",
    "Power",
    "About",
];

#[derive(Clone, Copy, PartialEq)]
pub enum Id {
    Back,
    /// A game's tile, by index.
    Game(usize),
    Dock(usize),
    /// Status bar: clock and power (quick settings), Wi-Fi, USB.
    Status(usize),
    /// Closes what is on top (the dimmed area).
    Scrim,
    // Quick settings.
    QSleep,
    QBright,
    QTheme,
    QFlight,
    QSettings,
    // Settings.
    Cat(usize),
    Theme(bool),
    Bright,
    AutoSleep(usize),
    Flight,
    OpenPage(usize),
    SleepNow,
    Reboot,
    PowerOff,
    // System.
    SysTab(usize),
    // Network.
    Scan,
    Retry,
    Leave,
    ScanBack,
    LogSize,
    LogClear,
    TermSize,
    // Sheets.
    FanOpen,
    FanAuto,
    FanManual,
    FanDown,
    FanUp,
    CpuOpen,
    /// A step of `cpufreq::STEPS_MHZ`, by index.
    CpuStep(usize),
    CpuBoot,
    GpuOpen,
    /// A step of `gpu::STEPS_KHZ`, by index.
    GpuStep(usize),
    SheetClose,
    No,
    Yes,
}

/// What a tap asks the service to do.
#[derive(Clone, Copy)]
pub enum Action {
    Power(u32),
    Scan,
    Retry,
    Leave,
    ClearLogs,
    /// Start game `n` of the library.
    Launch(usize),
    /// `fan::SET`: `fan::AUTO` or a duty (%).
    Fan(u64),
    /// `cpufreq::SET`: a step (MHz) or `cpufreq::BOOT`.
    Cpu(u64),
    /// `gpu::CLOCK`: a step (kHz), with `CLOCK_FORCE` past the cap.
    Gpu(u64, bool),
    /// Backlight, %.
    Brightness(u8),
    /// A setting changed: write the file soon.
    Save,
    /// Screen off.
    Sleep,
    /// Flight mode on or off (`wifi::RADIO`).
    Flight(bool),
}

#[derive(Clone, Copy, PartialEq)]
enum Sheet {
    Fan,
    Cpu,
    Gpu,
    Power,
}

struct Zone {
    id: Id,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    /// Corner radius, for the outline.
    r: usize,
}

pub(crate) struct Modal {
    title: String,
    lines: [String; 2],
    ok: &'static str,
    tone: Tone,
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
    pub(crate) pressed: Option<Id>,
    pub(crate) modal: Option<Modal>,
    toast: Option<(String, u64)>,
    /// Scan results, shown instead of the Wi-Fi status.
    pub scan: Option<Vec<ScanNet>>,
    /// A Wi-Fi request is running.
    pub busy: bool,
    pub(crate) log_scale: usize,
    pub(crate) term_scale: usize,
    /// Screen messages written since the LOGS page was last shown.
    pub unread: usize,
    /// Network of the Wi-Fi card, for the LEAVE confirmation.
    pub(crate) ssid: String,
    sheet: Option<Sheet>,
    quick: bool,
    /// Fan state as last drawn: taps count from it.
    fan: Seen<Fan>,
    /// Fan requests in flight, and the last one (`fan::AUTO` or %).
    fan_sent: u32,
    fan_last: u64,
    cpu: Seen<Cpu>,
    /// A clock change is running: the steps wait for it.
    cpu_sent: bool,
    gpu: Seen<Gpu>,
    gpu_sent: bool,
    zones: Vec<Zone>,
    /// Zone the gamepad's A would tap (drawn outlined), and its centre as
    /// last drawn: where a move starts once the zone is gone.
    focus: Option<Id>,
    focus_at: (usize, usize),
    /// The games on the card, the one chosen, and where the row of
    /// tiles is scrolled to (in tiles, eased toward the one chosen).
    games: Vec<GameInfo>,
    sel: usize,
    scroll: f32,
    /// What is remembered (`config`), and whether the backlight answers.
    pub cfg: Config,
    pub has_backlight: bool,
    cat: usize,
    pub(crate) sys_tab: usize,
    /// Slider followed by a finger: its track (x, width).
    drag: Option<(Id, usize, usize)>,
    /// Page and focus to come back to from the page a quick link opened.
    pub(crate) wall: u64,
}

fn mhz(khz: u64) -> String {
    match khz {
        0 => "?".into(),
        k => format!("{}", (k + 500) / 1000),
    }
}

/// GPU clock in MHz, one decimal (sys-clk's steps are 76.8 MHz apart).
pub fn gpu_mhz(khz: u64) -> String {
    format!("{}.{}", khz / 1000, khz % 1000 / 100)
}

pub fn usb_status(d: &Data) -> (String, u32) {
    let p = pal();
    match d.usb {
        Now::Missing => ("console not running".into(), p.dim),
        Now::Busy => ("busy".into(), p.dim),
        Now::Value(u) => {
            let speed = if u.state & usb::ST_HIGH_SPEED != 0 {
                "high speed"
            } else {
                "full speed"
            };
            if u.state & usb::ST_TERMINAL != 0 {
                (format!("terminal open, {}", speed), p.good)
            } else if u.state & usb::ST_CONFIGURED != 0 {
                (format!("connected, {}, terminal closed", speed), p.warn)
            } else if u.state & usb::ST_HOST != 0 {
                ("host seen, not configured".into(), p.warn)
            } else {
                ("waiting for a PC".into(), p.dim)
            }
        }
    }
}

pub fn usb_short(d: &Data) -> (&'static str, u32) {
    let p = pal();
    match d.usb {
        Now::Missing => ("not running", p.dim),
        Now::Busy => ("busy", p.dim),
        Now::Value(u) if u.state & usb::ST_TERMINAL != 0 => ("open", p.good),
        Now::Value(u) if u.state & usb::ST_CONFIGURED != 0 => ("closed", p.warn),
        Now::Value(u) if u.state & usb::ST_HOST != 0 => ("host seen", p.warn),
        Now::Value(_) => ("no PC", p.dim),
    }
}

pub fn wifi_short(d: &Data) -> (&'static str, u32) {
    let p = pal();
    match d.wifi {
        Seen::Unknown => ("starting", p.dim),
        Seen::Missing => ("no Wi-Fi driver", p.dim),
        Seen::Value(w) => match w.state {
            wifi::ST_CONNECTED => ("connected", p.good),
            wifi::ST_CONNECTING => ("joining", p.warn),
            wifi::ST_SCANNING => ("scanning", p.warn),
            wifi::ST_IDLE => ("not connected", p.dim),
            wifi::ST_RADIO_OFF => ("radio off", p.warn),
            _ => ("no adapter", p.bad),
        },
    }
}

pub fn eth_text(d: &Data, short: bool) -> (&'static str, u32) {
    let p = pal();
    match d.eth {
        Eth::Up if d.eth_name == "usb0" => ("shared by the PC", p.good),
        Eth::Up => ("connected", p.good),
        Eth::Dhcp if short => ("waiting for DHCP", p.warn),
        Eth::Dhcp => ("link up, waiting for DHCP", p.warn),
        Eth::NoSharing if short => ("no DHCP answer", p.warn),
        Eth::NoSharing => ("no DHCP: sharing off on the PC?", p.warn),
        Eth::Off if short => ("off on the PC", p.dim),
        Eth::Off => ("link not turned on by the PC", p.dim),
        Eth::NoPc if short => ("no PC", p.dim),
        Eth::NoPc => ("no PC on USB-C", p.dim),
        Eth::Missing => ("not running", p.dim),
    }
}

/// Fan state: its mode and value, and the color it shows in.
pub fn fan_text(f: &Fan) -> (String, u32) {
    let p = pal();
    let (mode, c) = if f.state & fan::ST_HOT != 0 {
        ("hot", p.warn)
    } else if f.state & fan::ST_NO_SENSOR != 0 {
        ("no sensor", p.warn)
    } else if f.state & fan::ST_MANUAL != 0 {
        ("manual", p.text)
    } else {
        ("auto", p.text)
    };
    let text = match (f.percent, f.rpm) {
        (0, _) => format!("{}: off", mode),
        (p, Some(rpm)) => format!("{}: {}%, {} rpm", mode, p, rpm),
        (p, None) => format!("{}: {}%", mode, p),
    };
    (text, c)
}

/// CPU clock and rail, and the color they show in.
pub fn cpu_text(c: &Cpu) -> (String, u32) {
    let p = pal();
    let rail = match c.uv {
        0 => String::new(),
        uv => format!(", {} mV", uv / 1000),
    };
    if c.state & cpufreq::ST_UNSUPPORTED != 0 {
        (format!("{} MHz, boot only", mhz(c.khz)), p.dim)
    } else if c.state & cpufreq::ST_HOT != 0 {
        (format!("hot: {} MHz{}", mhz(c.khz), rail), p.warn)
    } else if c.step == cpufreq::BOOT {
        (format!("boot: {} MHz{}", mhz(c.khz), rail), p.text)
    } else {
        (format!("{} MHz{}", mhz(c.khz), rail), p.text)
    }
}

/// The GPU row: off, powering up, its clock and rail (warning color when
/// hot or forced past the cap), or where power-up failed.
pub fn gpu_text(g: &Seen<Gpu>) -> (String, u32) {
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
    let p = pal();
    let stage = |g: &Gpu| *STAGES.get(g.stage as usize).unwrap_or(&"?");
    match g {
        Seen::Value(g) if g.state == gpu::ON => {
            let clock = match g.khz {
                0 => "bypass clock".into(),
                k => format!("{} MHz", gpu_mhz(k)),
            };
            let text = format!("{}, {} mV", clock, g.uv / 1000);
            if g.ck & gpu::CK_HOT != 0 {
                (format!("hot: {}", text), p.warn)
            } else if g.ck & gpu::CK_FORCED != 0 {
                (text, p.warn)
            } else {
                (text, p.good)
            }
        }
        Seen::Value(g) if g.state == gpu::FAILED => (format!("failed: {}", stage(g)), p.bad),
        Seen::Value(g) if g.stage != 0 => (format!("powering up: {}", stage(g)), p.dim),
        Seen::Value(_) => ("off".into(), p.dim),
        Seen::Missing => ("no GPU service".into(), p.dim),
        Seen::Unknown => ("starting".into(), p.dim),
    }
}

pub fn wifi_connected(d: &Data) -> Option<crate::stats::Wifi> {
    match d.wifi {
        Seen::Value(w) if w.state == wifi::ST_CONNECTED => Some(w),
        _ => None,
    }
}

/// `hh:mm` UTC from the wall clock, or dashes while it is not set.
fn clock_text(wall: u64) -> String {
    let s = wall / 1_000_000_000;
    // Before 2001: the RTC was never read.
    if s < 978_307_200 {
        return "--:--".into();
    }
    format!("{:02}:{:02}", s / 3600 % 24, s / 60 % 60)
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
            sheet: None,
            quick: false,
            fan: Seen::Unknown,
            fan_sent: 0,
            fan_last: fan::AUTO,
            cpu: Seen::Unknown,
            cpu_sent: false,
            gpu: Seen::Unknown,
            gpu_sent: false,
            zones: Vec::new(),
            focus: None,
            focus_at: (W / 2, H / 2),
            games: Vec::new(),
            sel: 0,
            scroll: 0.0,
            cfg: Config::new(),
            has_backlight: false,
            cat: 0,
            sys_tab: 0,
            drag: None,
            wall: 0,
        }
    }

    /// The settings came from the card (or changed): theme in force.
    pub fn apply_config(&mut self, c: Config) {
        self.cfg = c;
        theme::set(c.light);
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

    pub fn gpu_done(&mut self) {
        self.gpu_sent = false;
    }

    /// A GPU step: at once within the power source's cap, past it once
    /// confirmed (then without a cap until the next change).
    fn gpu_step(&mut self, i: usize) -> Option<Action> {
        let Seen::Value(g) = self.gpu else {
            return None;
        };
        let k = gpu::STEPS_KHZ[i];
        if g.step == k {
            return None;
        }
        if k <= g.cap {
            self.gpu_sent = true;
            return Some(Action::Gpu(k, false));
        }
        let lines = if g.supply == power::SUPPLY_EXTERNAL {
            [
                format!("Above the {} MHz cap on input power:", gpu_mhz(g.cap)),
                "the official charger or dock only.".into(),
            ]
        } else {
            [
                format!("Above the {} MHz cap on battery, which", gpu_mhz(g.cap)),
                "sys-clk keeps to spare the battery.".into(),
            ]
        };
        self.modal = Some(Modal {
            title: format!("Overclock the GPU to {} MHz?", gpu_mhz(k)),
            lines,
            ok: "OVERCLOCK",
            tone: Tone::Warn,
            action: Action::Gpu(k, true),
        });
        None
    }

    /// Duty set by hand (%) as the FAN sheet shows it, none in auto:
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

    pub fn set_games(&mut self, v: Vec<GameInfo>) {
        self.sel = self.sel.min(v.len().saturating_sub(1));
        self.games = v;
    }

    pub fn game(&self, i: usize) -> Option<&GameInfo> {
        self.games.get(i)
    }

    /// The row of tiles is still moving: draw again soon.
    pub fn animating(&self) -> bool {
        self.page == LIBRARY && (self.scroll - self.sel as f32).abs() > 0.004
    }

    /// Short notice above the hint bar.
    pub fn toast(&mut self, text: String, now: u64) {
        self.toast = Some((text, now + TOAST_NS));
    }

    /// When the screen changes by itself (a notice ends), if it will.
    pub fn deadline(&self) -> Option<u64> {
        self.toast.as_ref().map(|t| t.1)
    }

    pub(crate) fn zone(&mut self, id: Id, x: usize, y: usize, w: usize, h: usize, r: usize) {
        self.zones.push(Zone { id, x, y, w, h, r });
    }

    fn hit(&self, x: usize, y: usize) -> Option<Id> {
        self.zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x < z.x + z.w && y >= z.y && y < z.y + z.h)
            .map(|z| z.id)
    }

    fn overlay(&self) -> bool {
        self.modal.is_some() || self.sheet.is_some() || self.quick
    }

    /// The brightness a drag at pixel `px` of a track gives.
    fn slide(&mut self, id: Id, zx: usize, zw: usize, px: usize) -> Option<Action> {
        let (x0, w) = (zx + 14, zw.saturating_sub(28).max(1));
        let pct = (px.saturating_sub(x0) * 100 / w).min(100) as u8;
        match id {
            Id::QBright | Id::Bright => self.set_brightness(pct),
            _ => None,
        }
    }

    fn set_brightness(&mut self, pct: u8) -> Option<Action> {
        let v = pct.clamp(MIN_PCT, 100);
        if v == self.cfg.brightness {
            return None;
        }
        self.cfg.brightness = v;
        Some(Action::Brightness(v))
    }

    fn is_slider(id: Id) -> bool {
        matches!(id, Id::QBright | Id::Bright)
    }

    /// Touch event at landscape (x, y); a tap may ask for an action.
    pub fn touch(&mut self, code: u64, x: usize, y: usize) -> Option<Action> {
        match code {
            TOUCH_DOWN => {
                self.focus = None;
                self.pressed = self.hit(x, y);
                let id = self.pressed?;
                if !Self::is_slider(id) {
                    return None;
                }
                let z = self.zones.iter().rev().find(|z| z.id == id)?;
                let (zx, zw) = (z.x, z.w);
                self.drag = Some((id, zx, zw));
                self.slide(id, zx, zw, x)
            }
            TOUCH_MOVE => {
                let (id, zx, zw) = self.drag?;
                self.slide(id, zx, zw, x)
            }
            TOUCH_UP => {
                if self.drag.take().is_some() {
                    self.pressed = None;
                    return Some(Action::Save);
                }
                match (self.pressed.take(), self.hit(x, y)) {
                    (Some(a), Some(b)) if a == b => self.tap(a),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Zone nearest to the landscape point (x, y).
    fn nearest(&self, x: usize, y: usize) -> Option<Id> {
        let d = |z: &Zone| {
            let (cx, cy) = (z.x + z.w / 2, z.y + z.h / 2);
            cx.abs_diff(x).pow(2) + cy.abs_diff(y).pow(2)
        };
        self.zones
            .iter()
            .filter(|z| z.id != Id::Scrim)
            .min_by_key(|z| d(z))
            .map(|z| z.id)
    }

    /// The D-pad's step: the zone ahead that is closest, favouring the
    /// ones in line with the focused one.
    fn step_focus(&mut self, code: u64) {
        let Some(cur) = self
            .focus
            .and_then(|f| self.zones.iter().rev().find(|z| z.id == f))
        else {
            self.focus = self.nearest(self.focus_at.0, self.focus_at.1);
            return;
        };
        let (cx, cy) = ((cur.x + cur.w / 2) as i64, (cur.y + cur.h / 2) as i64);
        let best = self
            .zones
            .iter()
            .filter(|z| z.id != cur.id && z.id != Id::Scrim)
            .filter_map(|z| {
                let (dx, dy) = ((z.x + z.w / 2) as i64 - cx, (z.y + z.h / 2) as i64 - cy);
                let (ahead, aside) = match code {
                    BTN_RIGHT => (dx, dy),
                    BTN_LEFT => (-dx, dy),
                    BTN_DOWN => (dy, dx),
                    _ => (-dy, dx),
                };
                (ahead > 0).then_some((ahead + 2 * aside.abs(), z.id))
            })
            .min_by_key(|c| c.0);
        if let Some((_, id)) = best {
            self.focus = Some(id);
        }
    }

    /// A gamepad button was pressed. The D-pad moves the focus (or a
    /// slider), A taps the focused zone, B backs out, L and R (ZL and ZR)
    /// turn the page, - opens the quick settings, HOME and + go home.
    pub fn pad(&mut self, code: u64) -> Option<Action> {
        let open = self.overlay();
        // On the library the chosen tile is where the pad starts.
        let first = matches!(code, BTN_A | BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT);
        if self.page == LIBRARY && !open && self.focus.is_none() && first && !self.games.is_empty()
        {
            self.focus = Some(Id::Game(self.sel));
            if code != BTN_A {
                // The first press of an arrow only shows where it is.
                return None;
            }
        }
        // A slider takes left and right.
        if let (Some(f), BTN_LEFT | BTN_RIGHT) = (self.focus, code) {
            if Self::is_slider(f) && self.zones.iter().any(|z| z.id == f) {
                let v = self.cfg.brightness;
                let v = if code == BTN_LEFT {
                    v.saturating_sub(PAD_STEP)
                } else {
                    v.saturating_add(PAD_STEP)
                };
                return self.set_brightness(v).or(Some(Action::Save));
            }
        }
        match code {
            BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT => {
                self.step_focus(code);
                if let Some(Id::Game(i)) = self.focus {
                    self.sel = i;
                }
            }
            BTN_A => match self.focus {
                Some(id) if self.zones.iter().any(|z| z.id == id) => return self.tap(id),
                _ => self.step_focus(code),
            },
            BTN_B if self.modal.is_some() => return self.tap(Id::No),
            BTN_B if self.sheet.is_some() => self.sheet = None,
            BTN_B if self.quick => self.quick = false,
            BTN_B if self.scan.is_some() => self.scan = None,
            BTN_B | BTN_HOME | BTN_PLUS if self.page != LIBRARY || open => {
                self.quick = false;
                self.sheet = None;
                self.go(LIBRARY);
            }
            BTN_MINUS if self.modal.is_none() && self.sheet.is_none() => {
                self.quick = !self.quick;
                self.focus = self.quick.then_some(Id::QSleep);
            }
            BTN_L | BTN_ZL if !open => self.go((self.page + TITLES.len() - 1) % TITLES.len()),
            BTN_R | BTN_ZR if !open => self.go((self.page + 1) % TITLES.len()),
            _ => {}
        }
        None
    }

    fn go(&mut self, page: usize) {
        self.page = page;
        self.focus = None;
        self.scan = None;
        if page == LOGS {
            self.unread = 0;
        }
    }

    fn confirm(&mut self, title: &str, lines: [&str; 2], ok: &'static str, tone: Tone, a: Action) {
        self.modal = Some(Modal {
            title: title.into(),
            lines: [lines[0].into(), lines[1].into()],
            ok,
            tone,
            action: a,
        });
    }

    fn tap(&mut self, id: Id) -> Option<Action> {
        match id {
            Id::Back => self.go(LIBRARY),
            Id::Scrim => {
                self.quick = false;
                self.sheet = None;
            }
            Id::Status(0) => {
                self.quick = true;
                self.focus = None;
            }
            Id::Status(1) => self.go(NETWORK),
            Id::Status(_) => self.go(TERMINAL),
            Id::Dock(0) => self.go(SETTINGS),
            Id::Dock(1) => self.go(NETWORK),
            Id::Dock(2) => self.go(SYSTEM),
            Id::Dock(3) => self.go(TERMINAL),
            Id::Dock(4) => self.go(LOGS),
            Id::Dock(5) | Id::QSleep | Id::SleepNow => {
                self.quick = false;
                self.sheet = None;
                return Some(Action::Sleep);
            }
            Id::Dock(_) => self.sheet = Some(Sheet::Power),
            Id::OpenPage(p) => self.go(p),
            Id::Game(i) if self.sel == i => return Some(Action::Launch(i)),
            Id::Game(i) => self.sel = i,
            Id::QSettings => {
                self.quick = false;
                self.go(SETTINGS);
            }
            Id::QBright | Id::Bright => {}
            Id::QTheme => return self.set_theme(!self.cfg.light),
            Id::Theme(light) => return self.set_theme(light),
            Id::QFlight | Id::Flight => {
                self.cfg.flight = !self.cfg.flight;
                return Some(Action::Flight(self.cfg.flight));
            }
            Id::Cat(i) => self.cat = i,
            Id::AutoSleep(i) => {
                self.cfg.sleep = i as u8;
                return Some(Action::Save);
            }
            Id::SysTab(i) => self.sys_tab = i,
            Id::FanOpen => self.sheet = Some(Sheet::Fan),
            Id::CpuOpen => self.sheet = Some(Sheet::Cpu),
            Id::GpuOpen => self.sheet = Some(Sheet::Gpu),
            Id::SheetClose => self.sheet = None,
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
            Id::GpuStep(_) if self.gpu_sent => {}
            Id::GpuStep(i) => return self.gpu_step(i),
            Id::Reboot => {
                self.sheet = None;
                self.confirm(
                    "Reboot the Switch?",
                    ["It goes back to RCM: inject nxk", "again from the PC."],
                    "REBOOT",
                    Tone::Warn,
                    Action::Power(power::REBOOT),
                )
            }
            Id::PowerOff => {
                self.sheet = None;
                self.confirm(
                    "Power off the Switch?",
                    [
                        "Power on goes to RCM (AutoRCM):",
                        "inject nxk again from the PC.",
                    ],
                    "POWER OFF",
                    Tone::Bad,
                    Action::Power(power::OFF),
                )
            }
            Id::Leave => {
                let ssid = self.ssid.clone();
                self.modal = Some(Modal {
                    title: format!("Leave {}?", ssid),
                    lines: [
                        "Rejoining needs the shell:".into(),
                        format!("wifi connect {} PASSPHRASE", ssid),
                    ],
                    ok: "LEAVE",
                    tone: Tone::Warn,
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
            Id::ScanBack => self.scan = None,
            Id::No => self.modal = None,
            Id::Yes => {
                let a = self.modal.take().map(|m| m.action);
                if matches!(a, Some(Action::Gpu(..))) {
                    self.gpu_sent = true;
                }
                return a;
            }
            Id::LogSize => self.log_scale = 3 - self.log_scale,
            Id::LogClear => return Some(Action::ClearLogs),
            Id::TermSize => self.term_scale = 3 - self.term_scale,
        }
        None
    }

    fn set_theme(&mut self, light: bool) -> Option<Action> {
        if self.cfg.light == light {
            return None;
        }
        self.cfg.light = light;
        theme::set(light);
        Some(Action::Save)
    }

    /// Whole frame into `s` (not yet on screen: `Screen::present`).
    pub fn draw(&mut self, s: &mut Screen, d: &Data, msgs: &Messages, term: &Term, now: u64) {
        let p = pal();
        self.zones.clear();
        self.wall = libnx::time::realtime_ns();
        s.clear(p.bg);
        match self.page {
            LIBRARY => self.home(s, d),
            SETTINGS => self.settings(s, d),
            NETWORK => self.network(s, d),
            SYSTEM => self.system(s, d),
            TERMINAL => self.terminal(s, d, term, now),
            _ => self.logs(s, msgs),
        }
        self.status_bar(s, d);
        if matches!(&self.toast, Some((_, end)) if now >= *end) {
            self.toast = None;
        }
        if let Some((t, _)) = &self.toast {
            let w = font::width(Face::Body, t) + 56;
            let (x, y) = ((W - w) / 2, CB - 56);
            s.shadow(x, y, w, 48, 24, 0xFF00_0000);
            s.fill_round(x, y, w, 48, 24, p.text);
            s.text_c(W / 2, vc(Face::Body, y, 48), t, Face::Body, p.bg);
        }
        self.fan = d.fan;
        self.cpu = d.cpu;
        self.gpu = d.gpu;
        if self.quick {
            self.zones.clear();
            self.quick_panel(s, d);
        }
        if let Some(sh) = self.sheet {
            self.zones.clear();
            match sh {
                Sheet::Fan => self.fan_sheet(s),
                Sheet::Cpu => self.cpu_sheet(s),
                Sheet::Gpu => self.gpu_sheet(s),
                Sheet::Power => self.power_sheet(s),
            }
        }
        if self.modal.is_some() {
            self.zones.clear();
            self.modal(s);
        }
        // The focus outline comes last; a zone that is gone passes it on
        // to the one nearest to where it was.
        if self
            .focus
            .is_some_and(|f| self.zones.iter().all(|z| z.id != f))
        {
            self.focus = self.nearest(self.focus_at.0, self.focus_at.1);
        }
        let ring = self
            .focus
            .and_then(|f| self.zones.iter().rev().find(|z| z.id == f))
            .map(|z| (z.x, z.y, z.w, z.h, z.r));
        if let Some((x, y, w, h, r)) = ring {
            self.focus_at = (x + w / 2, y + h / 2);
            // A game's tile shows its own selection.
            if !matches!(self.focus, Some(Id::Game(_))) {
                s.ring_round(x - 3, y - 3, w + 6, h + 6, r + 3, 3, p.focus);
            }
        }
    }

    /// Hints of the bottom bar, right to left: (button, what it does).
    fn hints(&self, s: &mut Screen, items: &[(&str, &str)]) {
        let p = pal();
        let y = H - 40;
        s.text(M, vc(Face::Cap, y, 28), TAGLINE, Face::Cap, p.dim);
        let mut x = W - M;
        for (b, t) in items.iter().rev() {
            x -= font::width(Face::Body, t);
            s.text(x, vc(Face::Body, y, 28), t, Face::Body, p.text);
            x -= 12 + 26;
            s.disc(x + 13, y + 14, 13, p.text);
            s.text_c(x + 13, vc(Face::Cap, y, 28), b, Face::Cap, p.bg);
            x -= 28;
        }
    }

    /// Top right: temperature, USB, Wi-Fi, power and the clock.
    fn status_bar(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let (yc, home) = (50usize, self.page == LIBRARY);
        let mut x = W - M;
        // Clock and power source open the quick settings.
        let utc = font::width(Face::Cap, "UTC");
        s.text_r(x, yc + 2, "UTC", Face::Cap, p.dim);
        let clock = clock_text(self.wall);
        let cw = s.text_r(
            x - utc - 6,
            vc(Face::BodyB, yc - 20, 40),
            &clock,
            Face::BodyB,
            p.text,
        );
        x -= utc + 6 + cw + 24;
        let (sup_text, sup_c) = match d.supply {
            Seen::Value(power::SUPPLY_EXTERNAL) => ("USB power", p.good),
            Seen::Value(_) => ("Battery", p.text),
            _ => ("", p.dim),
        };
        if !sup_text.is_empty() {
            let tw = s.text_r(x, vc(Face::Cap, yc - 20, 40), sup_text, Face::Cap, sup_c);
            x -= tw + 8;
            icon(s, Icon::Bolt, x - 8, yc, sup_c, p.bg);
            x -= 40;
        }
        self.zone(Id::Status(0), x, yc - 28, W - M - x, 56, 14);
        // Wi-Fi.
        let (wt, wc) = wifi_short(d);
        let n = wifi_connected(d).map_or(0, |w| bars_for(w.signal));
        let wx = x - 40;
        signal_bars(s, wx, yc + 14, n, if n > 0 { p.text } else { p.dim });
        if wt == "radio off" {
            icon(s, Icon::Plane, wx + 14, yc, wc, p.bg);
        }
        self.zone(Id::Status(1), wx - 8, yc - 28, 56, 56, 14);
        x = wx - 32;
        // USB.
        let (_, uc) = usb_short(d);
        let uw = font::width(Face::Cap, "USB") + 28;
        x -= uw;
        s.ring_round(x, yc - 14, uw, 28, 14, 2, uc);
        s.text(x + 14, vc(Face::Cap, yc - 14, 28), "USB", Face::Cap, uc);
        self.zone(Id::Status(2), x - 8, yc - 28, uw + 16, 56, 14);
        x -= 28;
        // Temperature.
        if let Seen::Value((chip, _)) = d.temps {
            let t = format!("{}", Celsius(chip));
            s.text_r(x, vc(Face::Cap, yc - 20, 40), &t, Face::Cap, p.dim);
        }
        let _ = home;
    }

    /// Back button and title of a page.
    pub(crate) fn page_head(&mut self, s: &mut Screen, title: &str) {
        let p = pal();
        let down = self.pressed == Some(Id::Back);
        s.disc(M + 28, 50, 28, press(p.surface, down));
        s.ring(M + 28, 50, 28, 1, p.line);
        icon(s, Icon::Left, M + 26, 50, p.text, p.surface);
        s.text(
            M + 76,
            vc(Face::TitleB, 20, 60),
            title,
            Face::TitleB,
            p.text,
        );
        self.zone(Id::Back, M, 22, 56, 56, 28);
    }

    /// A button; `on` false: drawn dimmed, no zone.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn button(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        text: &str,
        tone: Tone,
        on: bool,
    ) {
        let p = pal();
        let bg = press(p.raised, on && self.pressed == Some(id));
        s.fill_round(x, y, w, h, 14, bg);
        let fg = if on {
            tone_color(tone)
        } else {
            mix(p.dim, p.raised, 140)
        };
        s.text_c(x + w / 2, vc(Face::BodyB, y, h), text, Face::BodyB, fg);
        if on {
            self.zone(id, x, y, w, h, 14);
        }
    }

    /// An option of a group: the chosen one is filled with the accent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn chip(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        text: &str,
        chosen: bool,
        on: bool,
    ) {
        let p = pal();
        let down = on && self.pressed == Some(id);
        let (bg, fg) = if chosen {
            (p.accent, p.on_accent)
        } else if on {
            (press(p.raised, down), p.text)
        } else {
            (p.raised, mix(p.dim, p.raised, 140))
        };
        s.fill_round(x, y, w, h, h / 2, bg);
        s.text_c(x + w / 2, vc(Face::Body, y, h), text, Face::Body, fg);
        if on {
            self.zone(id, x, y, w, h, h / 2);
        }
    }

    /// A card row that opens a sheet: label, value, chevron.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_row(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        w: usize,
        y: usize,
        label: &str,
        text: &str,
        c: u32,
    ) {
        let p = pal();
        let bg = press(p.raised, self.pressed == Some(id));
        s.fill_round(x + 16, y, w - 32, 48, 12, bg);
        s.text(x + 32, vc(Face::Body, y, 48), label, Face::Body, p.dim);
        s.text_r(x + w - 64, vc(Face::Body, y, 48), text, Face::Body, c);
        icon(s, Icon::Chevron, x + w - 40, y + 24, p.dim, bg);
        self.zone(id, x + 16, y, w - 32, 48, 12);
    }

    /// A card that opens a page (the system overview's).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tile_card(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        title: &str,
    ) {
        let p = pal();
        crate::widgets::titled(s, x, y, w, h, title);
        if self.pressed == Some(id) {
            s.ring_round(x, y, w, h, 16, 2, p.accent);
        }
        self.zone(id, x, y, w, h, 16);
    }

    /// The game library, the home page.
    fn home(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        self.scroll += (self.sel as f32 - self.scroll) * 0.3;
        if (self.scroll - self.sel as f32).abs() < 0.004 {
            self.scroll = self.sel as f32;
        }
        let (lw, lh) = screen::logo_size();
        s.logo(M, 50 - lh, 2);
        let tx = M + lw * 2 + 36;
        let n = self.games.len();
        let tw = s.text(tx, vc(Face::TitleB, 20, 60), "Games", Face::TitleB, p.text);
        let count = match n {
            1 => "1 installed".into(),
            n => format!("{} installed", n),
        };
        s.text(
            tx + tw + 20,
            vc(Face::Body, 20, 60) + 2,
            &count,
            Face::Body,
            p.dim,
        );

        const T: usize = 256;
        const PITCH: f32 = 280.0;
        const ANCHOR: f32 = 96.0;
        let y0 = 156usize;
        if n == 0 {
            card(s, W / 2 - 330, 170, 660, 200);
            s.text_c(W / 2, 214, "No games yet", Face::Title, p.text);
            s.text_c(W / 2, 270, "Copy a game to /games/NAME", Face::Body, p.dim);
            s.text_c(
                W / 2,
                300,
                "on the SD card, with its game.txt.",
                Face::Body,
                p.dim,
            );
        }
        for i in 0..n.max(4) {
            let left = (ANCHOR + (i as f32 - self.scroll) * PITCH) as i32;
            if left + T as i32 <= 0 || left >= W as i32 {
                continue;
            }
            if i >= n {
                s.dx = left;
                s.card_round(0, y0, T, T, 20, 2, p.line, p.surface);
                s.dx = 0;
                continue;
            }
            let on = i == self.sel;
            let ty = if on { y0 - 14 } else { y0 };
            let g = &self.games[i];
            let (color, light) = (g.color, mix(g.color, 0xFFFF_FFFF, 150));
            s.dx = left;
            if on {
                s.shadow(0, ty, T, T, 20, 0xFF00_0000);
            }
            s.fill_round(0, ty, T, T, 20, mix(color, 0xFF00_0000, 60));
            s.round_a(12, ty + 12, T - 24, 96, 14, light, 32);
            s.round_a(T / 2 - 52, ty + 34, 104, 104, 52, light, 52);
            let initial: String = g.title.chars().take(1).collect();
            s.text_c(
                T / 2,
                vc(Face::Big, ty + 34, 104),
                &initial.to_uppercase(),
                Face::Big,
                0xFFFF_FFFF,
            );
            s.round_a(12, ty + T - 62, T - 24, 50, 12, 0xFF00_0000, 120);
            let name = fit(Face::BodyB, &g.title, T - 56);
            s.text(
                26,
                vc(Face::BodyB, ty + T - 62, 50),
                &name,
                Face::BodyB,
                0xFFFF_FFFF,
            );
            if !on {
                s.round_a(0, ty, T, T, 20, p.bg, 80);
                s.ring_round(0, ty, T, T, 20, 1, p.line);
            } else {
                s.ring_round(0, ty, T, T, 20, 4, p.focus);
            }
            s.dx = 0;
            let (zx, zr) = (
                left.max(0) as usize,
                (left + T as i32).min(W as i32) as usize,
            );
            self.zone(Id::Game(i), zx, ty, zr - zx, T, 20);
        }
        if n > 0 {
            let g = &self.games[self.sel];
            s.text(
                M,
                436,
                &fit(Face::Head, &g.title, W - 2 * M),
                Face::Head,
                p.accent,
            );
            s.text(
                M,
                492,
                &fit(Face::Body, &g.desc, W - 2 * M),
                Face::Body,
                p.dim,
            );
        }
        self.dock(s);
        let _ = d;
        self.hints(s, &[("A", "Play"), ("-", "Quick settings"), ("R", "Pages")]);
    }

    /// Round icons: settings, pages, sleep and power.
    fn dock(&mut self, s: &mut Screen) {
        let p = pal();
        let (r, pitch, cy) = (34usize, 108usize, 580usize);
        let total = (DOCK.len() - 1) * pitch + 2 * r;
        let x0 = (W - total) / 2;
        for (i, (ic, label)) in DOCK.iter().enumerate() {
            let cx = x0 + r + i * pitch;
            let down = self.pressed == Some(Id::Dock(i));
            let on = self.focus == Some(Id::Dock(i));
            s.disc(cx, cy, r, press(p.surface, down));
            s.ring(cx, cy, r, 1, p.line);
            icon(s, *ic, cx, cy, p.text, press(p.surface, down));
            let c = if on || down { p.text } else { p.dim };
            s.text_c(cx, cy + r + 10, label, Face::Cap, c);
            if i == 4 && self.unread > 0 {
                s.disc(cx + r - 6, cy - r + 6, 12, p.accent);
                s.text_c(
                    cx + r - 6,
                    vc(Face::Cap, cy - r - 6, 24),
                    &format!("{}", self.unread.min(99)),
                    Face::Cap,
                    p.on_accent,
                );
            }
            self.zone(Id::Dock(i), cx - r, cy - r, 2 * r, 2 * r, r);
        }
    }

    /// A label and its value, then the slider of a brightness.
    fn bright_slider(&mut self, s: &mut Screen, id: Id, x: usize, y: usize, w: usize) {
        let p = pal();
        let en = self.has_backlight;
        icon(
            s,
            Icon::Sun,
            x + 14,
            y + 4,
            if en { p.text } else { p.dim },
            p.surface,
        );
        let pct = if en {
            self.cfg.brightness.max(MIN_PCT)
        } else {
            0
        };
        slider(s, x + 14 + 44, y, w - 28 - 44, pct, en);
        if en {
            self.zone(id, x + 44, y - 20, w - 44, 48, 12);
        }
    }

    /// Quick settings: sleep, brightness, volume (off), theme, flight
    /// mode and the way to the settings.
    fn quick_panel(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        s.fill_alpha(0, 0, W, H, p.scrim, 140);
        let (pw, px) = (456usize, W - 456);
        s.fill(px, 0, pw, H, p.surface);
        s.fill(px, 0, 1, H, p.line);
        self.zone(Id::Scrim, 0, 0, px, H, 0);
        let (x, w) = (px + 32, pw - 64);
        // Status line.
        s.text(x, 28, &clock_text(self.wall), Face::BodyB, p.text);
        s.text(x + 66, 32, "UTC", Face::Cap, p.dim);
        let (wt, wc) = wifi_short(d);
        let sup = match d.supply {
            Seen::Value(power::SUPPLY_EXTERNAL) => "USB power",
            Seen::Value(_) => "Battery",
            _ => "",
        };
        let rx = s.text_r(x + w, 32, sup, Face::Cap, p.dim);
        s.text_r(x + w - rx - 20, 32, wt, Face::Cap, wc);
        // Sleep.
        let down = self.pressed == Some(Id::QSleep);
        s.fill_round(x, 84, w, 64, 14, press(p.raised, down));
        icon(s, Icon::Moon, x + 40, 116, p.text, press(p.raised, down));
        s.text(
            x + 76,
            vc(Face::BodyB, 84, 64),
            "Sleep Mode",
            Face::BodyB,
            p.text,
        );
        self.zone(Id::QSleep, x, 84, w, 64, 14);
        // Brightness.
        s.text(x, 176, "Brightness", Face::Body, p.dim);
        let b = if self.has_backlight {
            format!("{}%", self.cfg.brightness.max(MIN_PCT))
        } else {
            "not available".into()
        };
        s.text_r(x + w, 176, &b, Face::Body, p.text);
        self.bright_slider(s, Id::QBright, x, 232, w);
        // Volume: the audio path is off for now.
        s.text(x, 284, "Volume", Face::Body, p.dim);
        s.text_r(x + w, 284, "audio off", Face::Body, p.dim);
        icon(s, Icon::Speaker, x + 14, 336, p.dim, p.surface);
        slider(s, x + 14 + 44, 332, w - 28 - 44, 0, false);
        // Theme and flight mode.
        s.fill(x, 372, w, 1, p.line);
        let light = self.cfg.light;
        s.text(
            x,
            vc(Face::Body, 388, 56),
            "Light theme",
            Face::Body,
            p.text,
        );
        switch(s, x + w - 56, 400, light, true);
        self.zone(Id::QTheme, x, 388, w, 56, 12);
        s.fill(x, 452, w, 1, p.line);
        s.text(x, 462, "Flight Mode", Face::Body, p.text);
        s.text(x, 492, "Wi-Fi radio off", Face::Cap, p.dim);
        switch(s, x + w - 56, 470, self.cfg.flight, true);
        self.zone(Id::QFlight, x, 460, w, 60, 12);
        s.fill(x, 532, w, 1, p.line);
        // Settings.
        let down = self.pressed == Some(Id::QSettings);
        s.fill_round(x, 548, w, 56, 14, press(p.surface, down));
        icon(s, Icon::Gear, x + 28, 576, p.text, press(p.surface, down));
        s.text(
            x + 64,
            vc(Face::Body, 548, 56),
            "System Settings",
            Face::Body,
            p.text,
        );
        icon(s, Icon::Chevron, x + w - 24, 576, p.dim, p.surface);
        self.zone(Id::QSettings, x, 548, w, 56, 14);
        s.text_r(x + w, H - 40, "B  Back", Face::Cap, p.dim);
    }

    /// Settings: categories on the left, the chosen one on the right.
    fn settings(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        self.page_head(s, "Settings");
        let lw = 320usize;
        for (i, name) in CATS.iter().enumerate() {
            let y = CY + 8 + i * 72;
            let (on, down) = (self.cat == i, self.pressed == Some(Id::Cat(i)));
            let bg = if on { p.surface } else { press(p.bg, down) };
            s.fill_round(M, y, lw, 60, 14, bg);
            if on {
                s.ring_round(M, y, lw, 60, 14, 1, p.line);
                s.fill_round(M, y + 14, 5, 32, 2, p.accent);
            }
            s.text(
                M + 28,
                vc(Face::Body, y, 60),
                name,
                Face::Body,
                if on { p.text } else { p.dim },
            );
            self.zone(Id::Cat(i), M, y, lw, 60, 14);
        }
        let (x, y, w, h) = (M + lw + 24, CY, W - 2 * M - lw - 24, CH);
        card(s, x, y, w, h);
        s.text(x + 32, y + 22, CATS[self.cat], Face::TitleB, p.text);
        let (cx, cw) = (x + 32, w - 64);
        match self.cat {
            0 => self.cat_display(s, cx, y + 88, cw),
            1 => {
                s.text(cx, y + 96, "Volume", Face::Body, p.dim);
                s.text_r(cx + cw, y + 96, "off", Face::Body, p.dim);
                icon(s, Icon::Speaker, cx + 14, y + 156, p.dim, p.surface);
                slider(s, cx + 58, y + 152, cw - 58, 0, false);
                s.text(
                    cx,
                    y + 208,
                    "The audio path is not switched on yet.",
                    Face::Body,
                    p.dim,
                );
                s.text(
                    cx,
                    y + 238,
                    "The speakers come back once it plays reliably.",
                    Face::Body,
                    p.dim,
                );
            }
            2 => self.cat_network(s, d, cx, y + 88, cw),
            3 => self.cat_performance(s, d, x, y + 88, w),
            4 => self.cat_power(s, d, cx, y + 88, cw),
            _ => self.cat_about(s, d, cx, y + 88, cw),
        }
        self.hints(s, &[("B", "Back"), ("A", "OK")]);
    }

    fn cat_display(&mut self, s: &mut Screen, x: usize, y: usize, w: usize) {
        let p = pal();
        s.text(x, y + 6, "Theme", Face::Body, p.dim);
        let light = self.cfg.light;
        let cw = 150;
        self.chip(
            s,
            Id::Theme(false),
            x + w - 2 * cw - 12,
            y,
            cw,
            44,
            "Dark",
            !light,
            true,
        );
        self.chip(
            s,
            Id::Theme(true),
            x + w - cw,
            y,
            cw,
            44,
            "Light",
            light,
            true,
        );
        s.fill(x, y + 68, w, 1, p.line);
        s.text(x, y + 92, "Brightness", Face::Body, p.dim);
        let b = if self.has_backlight {
            format!("{}%", self.cfg.brightness.max(MIN_PCT))
        } else {
            "no backlight control".into()
        };
        s.text_r(x + w, y + 92, &b, Face::Body, p.text);
        self.bright_slider(s, Id::Bright, x, y + 156, w);
        s.fill(x, y + 196, w, 1, p.line);
        s.text(x, y + 220, "Sleep after", Face::Body, p.dim);
        s.text(
            x,
            y + 252,
            "Screen off when nothing is touched",
            Face::Cap,
            p.dim,
        );
        let bw = 92;
        for (i, m) in SLEEP_MIN.iter().enumerate() {
            let label = if *m == 0 {
                "Never".into()
            } else {
                format!("{} min", m)
            };
            let bx = x + w - (SLEEP_MIN.len() - i) * (bw + 8) + 8;
            self.chip(
                s,
                Id::AutoSleep(i),
                bx,
                y + 224,
                bw,
                40,
                &label,
                self.cfg.sleep as usize == i,
                true,
            );
        }
    }

    fn cat_network(&mut self, s: &mut Screen, d: &Data, x: usize, y: usize, w: usize) {
        let p = pal();
        s.text(x, y, "Flight Mode", Face::Body, p.text);
        s.text(
            x,
            y + 30,
            "Turns the Wi-Fi radio off: no scan, no join",
            Face::Cap,
            p.dim,
        );
        switch(s, x + w - 56, y + 4, self.cfg.flight, true);
        self.zone(Id::Flight, x, y - 8, w, 64, 12);
        s.fill(x, y + 66, w, 1, p.line);
        let (wt, wc) = wifi_short(d);
        s.text(x, y + 88, "Wi-Fi", Face::Body, p.dim);
        s.text_r(x + w, y + 88, wt, Face::Body, wc);
        let (et, ec) = eth_text(d, true);
        s.text(x, y + 124, "USB-C Ethernet", Face::Body, p.dim);
        s.text_r(x + w, y + 124, et, Face::Body, ec);
        let ip = match d.active() {
            Some(n) if n.addr != 0 => format!("{}/{}", screen::Ipv4(n.addr), n.prefix),
            Some(_) => "waiting for DHCP".into(),
            None => "offline".into(),
        };
        s.text(x, y + 160, "Address", Face::Body, p.dim);
        s.text_r(x + w, y + 160, &ip, Face::Body, p.text);
        self.button(
            s,
            Id::OpenPage(NETWORK),
            x,
            y + 216,
            280,
            56,
            "Open network page",
            Tone::Accent,
            true,
        );
    }

    fn cat_performance(&mut self, s: &mut Screen, d: &Data, x: usize, y: usize, w: usize) {
        let (t, c) = match d.cpu {
            Seen::Value(c) => cpu_text(&c),
            Seen::Missing => ("no clock control".into(), pal().dim),
            Seen::Unknown => ("starting".into(), pal().dim),
        };
        self.open_row(s, Id::CpuOpen, x, w, y, "CPU clock", &t, c);
        let (t, c) = gpu_text(&d.gpu);
        self.open_row(s, Id::GpuOpen, x, w, y + 64, "GPU clock", &t, c);
        let (t, c) = match d.fan {
            Seen::Value(f) => fan_text(&f),
            Seen::Missing => ("no fan control".into(), pal().dim),
            Seen::Unknown => ("starting".into(), pal().dim),
        };
        self.open_row(s, Id::FanOpen, x, w, y + 128, "Fan", &t, c);
        s.text(
            x + 32,
            y + 208,
            "Clocks and the fan fall back to safe values when the SoC is hot.",
            Face::Cap,
            pal().dim,
        );
    }

    fn cat_power(&mut self, s: &mut Screen, d: &Data, x: usize, y: usize, w: usize) {
        let p = pal();
        let src = match d.supply {
            Seen::Value(power::SUPPLY_EXTERNAL) => "input power on the USB-C port",
            Seen::Value(_) => "battery",
            _ => "unknown",
        };
        s.text(x, y, "Power source", Face::Body, p.dim);
        s.text_r(x + w, y, src, Face::Body, p.text);
        s.fill(x, y + 44, w, 1, p.line);
        let bw = (w - 32) / 3;
        self.button(
            s,
            Id::SleepNow,
            x,
            y + 72,
            bw,
            64,
            "Sleep",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::Reboot,
            x + bw + 16,
            y + 72,
            bw,
            64,
            "Restart",
            Tone::Warn,
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            x + 2 * (bw + 16),
            y + 72,
            bw,
            64,
            "Power off",
            Tone::Bad,
            true,
        );
        s.text(
            x,
            y + 168,
            "Sleep turns the screen off and slows the CPU; touch or a button wakes it.",
            Face::Cap,
            p.dim,
        );
        s.text(
            x,
            y + 194,
            "Power off and restart go back to RCM: inject nxk again from the PC.",
            Face::Cap,
            p.dim,
        );
    }

    fn cat_about(&mut self, s: &mut Screen, d: &Data, x: usize, y: usize, w: usize) {
        let p = pal();
        let (sw, sh) = s.size();
        let rows: [(&str, String); 6] = [
            ("System", "nxk, AArch64 microkernel".into()),
            ("SoC", "NVIDIA Tegra X1 (4x Cortex-A57)".into()),
            (
                "Screen",
                format!(
                    "{}x{}{}",
                    sw,
                    sh,
                    if s.turned() { ", panel turned" } else { "" }
                ),
            ),
            ("Uptime", format!("{}", screen::Hms(d.up_s))),
            (
                "Memory",
                d.mem.map_or("-".into(), |m| {
                    format!("{}", screen::Bytes(m.total_pages * PAGE_SIZE))
                }),
            ),
            (
                "SD card",
                match d.disk {
                    Seen::Value((_, total)) => format!("{}", screen::Bytes(total)),
                    _ => "none".into(),
                },
            ),
        ];
        for (i, (k, v)) in rows.iter().enumerate() {
            let ry = y + i * 44;
            s.text(x, ry, k, Face::Body, p.dim);
            s.text_r(x + w, ry, v, Face::Body, p.text);
            s.fill(x, ry + 34, w, 1, p.line);
        }
    }

    /// A centered sheet over the dimmed page, with its title.
    fn sheet_frame(&mut self, s: &mut Screen, title: &str) -> (usize, usize, usize, usize) {
        let p = pal();
        s.fill_alpha(0, 0, W, H, p.scrim, 150);
        let (x, y, w, h) = (160, 64, 960, 592);
        self.zone(Id::Scrim, 0, 0, W, H, 0);
        s.shadow(x, y, w, h, 24, 0xFF00_0000);
        s.card_round(x, y, w, h, 24, 1, p.line, p.surface);
        s.text(x + 40, y + 30, title, Face::TitleB, p.text);
        // The card itself is no way out.
        self.zone(Id::Yes, x, y, w, h, 24);
        self.zones.pop();
        self.zone(Id::No, x, y, w, h, 24);
        self.zones.pop();
        (x, y, w, h)
    }

    fn close_button(&mut self, s: &mut Screen, x: usize, y: usize, w: usize, h: usize) {
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

    /// Fan: auto or by hand, the duty in `FAN_STEP` steps. The fan service
    /// still runs it full speed when the SoC is hot.
    fn fan_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "Fan");
        if let Seen::Value(f) = self.fan {
            let mut state = String::new();
            if let Some(t) = f.temp {
                state = format!("SoC {}", Celsius(t));
            }
            if let Some(rpm) = f.rpm {
                state = format!("{}   {} rpm", state, rpm);
            }
            s.text_r(x + w - 40, y + 38, &state, Face::Body, p.text);
            let manual = self.fan_manual();
            let bw = (w - 104) / 2;
            self.chip(
                s,
                Id::FanAuto,
                x + 40,
                y + 108,
                bw,
                56,
                "Auto",
                manual.is_none(),
                true,
            );
            self.chip(
                s,
                Id::FanManual,
                x + 64 + bw,
                y + 108,
                bw,
                56,
                "Manual",
                manual.is_some(),
                true,
            );
            let forced = f.state & (fan::ST_HOT | fan::ST_NO_SENSOR) != 0;
            let shown = manual.unwrap_or(f.percent);
            let can = |ok: bool| manual.is_some() && ok;
            self.button(
                s,
                Id::FanDown,
                x + 40,
                y + 208,
                160,
                120,
                "-10%",
                Tone::Plain,
                can(shown > 0),
            );
            self.button(
                s,
                Id::FanUp,
                x + w - 40 - 160,
                y + 208,
                160,
                120,
                "+10%",
                Tone::Plain,
                can(shown < 100),
            );
            let c = if forced { p.warn } else { p.text };
            s.text_c(x + w / 2, y + 220, &format!("{}%", shown), Face::Big, c);
            crate::widgets::bar(
                s,
                x + 240,
                y + 316,
                w - 480,
                12,
                f.percent,
                if forced { p.warn } else { p.good },
            );
            let (note, nc) = if f.state & fan::ST_HOT != 0 {
                ("SoC hot: full speed until below 57\u{b0}C", p.warn)
            } else if f.state & fan::ST_NO_SENSOR != 0 {
                ("No temperature: at least 43%", p.warn)
            } else if manual.is_some() {
                ("Full speed anyway from 60\u{b0}C", p.dim)
            } else {
                ("Follows the SoC temperature", p.dim)
            };
            s.text(x + 40, y + 372, note, Face::Body, nc);
        } else {
            let t = match self.fan {
                Seen::Missing => "no fan control",
                _ => "starting",
            };
            s.text(x + 40, y + 108, t, Face::Body, p.dim);
        }
        self.close_button(s, x, y, w, h);
    }

    /// CPU: sys-clk's steps, those needing more than the rail's maximum
    /// on this chip greyed out, or the loader's clock (BOOT).
    fn cpu_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "CPU clock");
        match self.cpu {
            Seen::Value(c) if c.state & cpufreq::ST_UNSUPPORTED == 0 => {
                let (t, tone) = cpu_text(&c);
                s.text_r(x + w - 40, y + 38, &t, Face::Body, tone);
                let (cols, gap) = (6, 14);
                let (bw, bh) = ((w - 80 - (cols - 1) * gap) / cols, 64);
                let mut top = 0;
                for (i, &m) in cpufreq::STEPS_MHZ.iter().enumerate() {
                    let (bx, by) = (
                        x + 40 + (i % cols) * (bw + gap),
                        y + 108 + (i / cols) * (bh + gap),
                    );
                    let runs = c.allowed & 1 << i != 0;
                    if runs {
                        top = m;
                    }
                    let on = runs && !self.cpu_sent;
                    self.chip(
                        s,
                        Id::CpuStep(i),
                        bx,
                        by,
                        bw,
                        bh,
                        &format!("{}", m),
                        c.step == m,
                        on,
                    );
                }
                let by = y + 108 + 2 * (64 + gap) + 8;
                self.chip(
                    s,
                    Id::CpuBoot,
                    x + 40,
                    by,
                    200,
                    56,
                    "Boot",
                    c.step == cpufreq::BOOT,
                    !self.cpu_sent,
                );
                s.text(
                    x + 264,
                    vc(Face::Body, by, 56),
                    "the loader's clock",
                    Face::Body,
                    p.dim,
                );
                let (note, nc) = if c.state & cpufreq::ST_HOT != 0 {
                    ("SoC hot: back to BOOT", p.warn)
                } else {
                    ("Back to BOOT at 80\u{b0}C", p.dim)
                };
                s.text(x + 40, y + 400, note, Face::Body, nc);
                s.text(
                    x + 40,
                    y + 436,
                    &format!("speedo {}: {} MHz max", c.speedo, top),
                    Face::Body,
                    p.dim,
                );
            }
            Seen::Value(c) => {
                s.text(
                    x + 40,
                    y + 108,
                    &format!("{} MHz", mhz(c.khz)),
                    Face::Head,
                    p.text,
                );
                s.text(
                    x + 40,
                    y + 176,
                    "Boot clock only on this console",
                    Face::Body,
                    p.dim,
                );
            }
            Seen::Missing | Seen::Unknown => {
                let t = match self.cpu {
                    Seen::Missing => "no clock control",
                    _ => "starting",
                };
                s.text(x + 40, y + 108, t, Face::Body, p.dim);
            }
        }
        self.close_button(s, x, y, w, h);
    }

    /// GPU: sys-clk's steps, those needing more than the rail's maximum
    /// on this chip greyed out, those past the power source's cap in the
    /// warning color (confirmed first).
    fn gpu_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "GPU clock");
        match self.gpu {
            // Fuses unread (0): the service still checks each request.
            Seen::Value(g) if g.steps != 1 => {
                let (t, tone) = gpu_text(&self.gpu);
                s.text_r(x + w - 40, y + 38, &t, Face::Body, tone);
                let (cols, gap) = (6, 14);
                let (bw, bh) = ((w - 80 - (cols - 1) * gap) / cols, 64);
                let mut top = 0;
                for (i, &k) in gpu::STEPS_KHZ.iter().enumerate() {
                    let (bx, by) = (
                        x + 40 + (i % cols) * (bw + gap),
                        y + 108 + (i / cols) * (bh + gap),
                    );
                    if g.steps & 1 << i != 0 {
                        top = k;
                    }
                    let runs = g.steps == 0 || g.steps & 1 << i != 0;
                    let set = g.step == k;
                    let on = runs && !self.gpu_sent;
                    self.chip(s, Id::GpuStep(i), bx, by, bw, bh, &gpu_mhz(k), set, on);
                    if k > g.cap && !set && on {
                        s.ring_round(bx, by, bw, bh, bh / 2, 2, p.warn);
                    }
                }
                let source = match g.supply {
                    power::SUPPLY_EXTERNAL => "on input power",
                    gpu::NO_SUPPLY => "(charger unread)",
                    _ => "on battery",
                };
                let mut cap = format!("cap {} MHz {}", gpu_mhz(g.cap), source);
                if g.ck & gpu::CK_FORCED != 0 {
                    cap.push_str(", forced past it");
                }
                let tone = if g.ck & gpu::CK_FORCED != 0 {
                    p.warn
                } else {
                    p.text
                };
                s.text(x + 40, y + 280, &cap, Face::Body, tone);
                let clock: String = match (g.state, g.measured) {
                    (gpu::ON, 0) => "not measured yet".into(),
                    (gpu::ON, m) => format!("measured {} MHz after the last change", gpu_mhz(m)),
                    _ => "off: the step applies at power-up".into(),
                };
                s.text(x + 40, y + 316, &clock, Face::Body, p.dim);
                let (note, nc) = if g.ck & gpu::CK_HOT != 0 {
                    ("SoC hot: back to 76.8 MHz", p.warn)
                } else if g.ck & gpu::CK_CAPPED != 0 {
                    ("Power source changed: down to its cap", p.warn)
                } else {
                    ("Back to 76.8 MHz at 80\u{b0}C", p.dim)
                };
                s.text(x + 40, y + 352, note, Face::Body, nc);
                if top != 0 {
                    s.text(
                        x + 40,
                        y + h - 40 - 60 + 16,
                        &format!("{} MHz max on this chip", gpu_mhz(top)),
                        Face::Body,
                        p.dim,
                    );
                }
            }
            Seen::Value(_) => {
                s.text(x + 40, y + 108, "76.8 MHz", Face::Head, p.text);
                s.text(
                    x + 40,
                    y + 176,
                    "Lowest step only on this console",
                    Face::Body,
                    p.dim,
                );
            }
            Seen::Missing | Seen::Unknown => {
                let t = match self.gpu {
                    Seen::Missing => "no GPU service",
                    _ => "starting",
                };
                s.text(x + 40, y + 108, t, Face::Body, p.dim);
            }
        }
        self.close_button(s, x, y, w, h);
    }

    /// Sleep, restart or power off.
    fn power_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = (360, 150, 560, 420);
        s.fill_alpha(0, 0, W, H, p.scrim, 150);
        self.zone(Id::Scrim, 0, 0, W, H, 0);
        s.shadow(x, y, w, h, 24, 0xFF00_0000);
        s.card_round(x, y, w, h, 24, 1, p.line, p.surface);
        s.text(x + 40, y + 30, "Power", Face::TitleB, p.text);
        self.button(
            s,
            Id::SleepNow,
            x + 40,
            y + 100,
            w - 80,
            64,
            "Sleep",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::Reboot,
            x + 40,
            y + 180,
            w - 80,
            64,
            "Restart",
            Tone::Warn,
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            x + 40,
            y + 260,
            w - 80,
            64,
            "Power off",
            Tone::Bad,
            true,
        );
        self.button(
            s,
            Id::SheetClose,
            x + 40,
            y + 340,
            w - 80,
            56,
            "Cancel",
            Tone::Plain,
            true,
        );
    }

    fn modal(&mut self, s: &mut Screen) {
        let Some(m) = self.modal.take() else { return };
        let p = pal();
        s.fill_alpha(0, 0, W, H, p.scrim, 170);
        let (x, y, w, h) = (240, 190, 800, 340);
        s.shadow(x, y, w, h, 24, 0xFF00_0000);
        s.card_round(x, y, w, h, 24, 1, p.line, p.surface);
        let tc = match m.tone {
            Tone::Plain => p.text,
            t => tone_color(t),
        };
        s.text(x + 40, y + 34, &m.title, Face::TitleB, tc);
        for (i, t) in m.lines.iter().enumerate() {
            s.text(x + 40, y + 100 + i * 36, t, Face::Body, p.text);
        }
        let (bw, by) = ((w - 104) / 2, y + h - 40 - 64);
        self.button(s, Id::No, x + 40, by, bw, 64, "Cancel", Tone::Plain, true);
        self.button(s, Id::Yes, x + 64 + bw, by, bw, 64, m.ok, m.tone, true);
        self.modal = Some(m);
    }

    pub(crate) fn hint_bar(&self, s: &mut Screen, items: &[(&str, &str)]) {
        self.hints(s, items);
    }

    pub(crate) fn sleep_minutes(&self) -> u8 {
        SLEEP_MIN[self.cfg.sleep as usize % SLEEP_MIN.len()]
    }
}

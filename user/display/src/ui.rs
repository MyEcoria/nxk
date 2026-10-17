//! Switch-style touch and gamepad UI: top navigation bar (user/theme
//! avatars, L/R tabs for Games, System, Network, Terminal, Logs,
//! Settings, and top-right status cluster), portrait game card carousel
//! with cover art and 2x2 app grid, right-side Quick Settings drawer,
//! 6-category Settings page, and hardware control sheets.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{cpufreq, fan, gpu, input, power, usbcon as usb, wifi};

use crate::backlight::MIN_PCT;
use crate::config::{Config, SLEEP_MIN};
use crate::font::{self, Face};
use crate::games::GameInfo;
use crate::screen::{mix, Celsius, Screen};
use crate::stats::{Cpu, Data, Eth, Fan, Gpu, Now, Seen};
use crate::term::Term;
use crate::theme::{self, pal, rgb as rgb_hex};
use crate::widgets::{card, fit, icon, press, slider, switch, tone_color, vc, Icon, Tone};
use crate::Messages;

/// The layout is made for the Switch's 1280x720 (landscape).
pub const W: usize = 1280;
pub const H: usize = 720;
/// Side margin, content top and bottom (above the hint bar).
pub const M: usize = 56;
pub const CY: usize = 104;
pub const CB: usize = 628;
pub const CH: usize = CB - CY;

// Pages (top navigation tabs).
pub const LIBRARY: usize = 0;
pub const SYSTEM: usize = 1;
pub const NETWORK: usize = 2;
pub const TERMINAL: usize = 3;
pub const LOGS: usize = 4;
pub const SETTINGS: usize = 5;

const TABS: [&str; 6] = ["Games", "System", "Network", "Terminal", "Logs", "Settings"];

const CATS: [&str; 6] = [
    "Display",
    "Sound",
    "Network",
    "Performance",
    "Power",
    "About",
];

const SLEEP_LABELS: [&str; 5] = ["Never", "1 min", "5 min", "10 min", "30 min"];

const TOAST_NS: u64 = 4_000_000_000;
const FAN_STEP: u64 = 10;
const PAD_STEP: u8 = 5;

#[derive(Clone, Copy, PartialEq)]
pub enum Id {
    AvatarHome,
    AvatarTheme,
    Tab(usize),
    BumperL,
    BumperR,
    /// A game's card, by index.
    Game(usize),
    /// Left peek card.
    PeekLeft,
    /// Middle stacked card.
    StackCard,
    /// 2x2 + 2 square app tiles on the home screen.
    AppTile(usize),
    /// Status bar top-right cluster (opens quick settings).
    StatusQuick,
    /// Closes what is on top (the dimmed area).
    Scrim,
    // Quick settings drawer.
    QSleep,
    QAutoBright,
    QBright,
    QVol,
    QFlight,
    QSettings,
    QTheme,
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
    r: usize,
}

struct Modal {
    title: String,
    lines: [String; 2],
    ok: &'static str,
    tone: Tone,
    action: Action,
}

pub struct ScanNet {
    pub ssid: String,
    pub channel: u8,
    pub signal: u8,
    pub security: u8,
}

pub struct Ui {
    pub page: usize,
    pub(crate) pressed: Option<Id>,
    modal: Option<Modal>,
    toast: Option<(String, u64)>,
    pub scan: Option<Vec<ScanNet>>,
    pub busy: bool,
    pub(crate) log_scale: usize,
    pub(crate) term_scale: usize,
    pub unread: usize,
    pub(crate) ssid: String,
    sheet: Option<Sheet>,
    /// Quick settings open on the right (opened by `-` or the status bar).
    pub quick: bool,
    pub(crate) fan: Seen<Fan>,
    fan_sent: usize,
    fan_last: u64,
    pub(crate) cpu: Seen<Cpu>,
    pub(crate) cpu_sent: bool,
    pub(crate) gpu: Seen<Gpu>,
    pub(crate) gpu_sent: bool,
    zones: Vec<Zone>,
    pub(crate) focus: Option<Id>,
    focus_at: (usize, usize),
    games: Vec<GameInfo>,
    sel: usize,
    scroll: f32,
    pub cfg: Config,
    pub has_backlight: bool,
    pub(crate) cat: usize,
    pub(crate) sys_tab: usize,
    drag: Option<(Id, usize, usize)>,
    wall: u64,
    vol_pct: u8,
    auto_bright: bool,
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

/// `10:25 PM` from the wall clock, or `10:25 PM` default when RTC is unset.
fn clock_ampm(wall: u64) -> String {
    let s = wall / 1_000_000_000;
    if s < 978_307_200 {
        return "10:25 PM".into();
    }
    let h24 = (s / 3600 % 24) as u32;
    let min = (s / 60 % 60) as u32;
    let (h12, ampm) = match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
    };
    format!("{}:{:02} {}", h12, min, ampm)
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
            vol_pct: 72,
            auto_bright: true,
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

    pub fn animating(&self) -> bool {
        self.page == LIBRARY && (self.scroll - self.sel as f32).abs() > 0.004
    }

    pub fn toast(&mut self, text: String, now: u64) {
        self.toast = Some((text, now + TOAST_NS));
    }

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

    fn nearest(&self, cx: usize, cy: usize) -> Option<Id> {
        self.zones
            .iter()
            .filter(|z| z.id != Id::Scrim)
            .min_by_key(|z| {
                let dx = (z.x + z.w / 2) as i64 - cx as i64;
                let dy = (z.y + z.h / 2) as i64 - cy as i64;
                dx * dx + dy * dy
            })
            .map(|z| z.id)
    }

    fn step_focus(&mut self, code: u64) {
        let (dx, dy): (i64, i64) = match code {
            input::BTN_LEFT => (-1, 0),
            input::BTN_RIGHT => (1, 0),
            input::BTN_UP => (0, -1),
            input::BTN_DOWN => (0, 1),
            _ => return,
        };
        let cur = self
            .focus
            .and_then(|f| self.zones.iter().find(|z| z.id == f));
        let Some(cur) = cur else {
            self.focus = self.zones.iter().find(|z| z.id != Id::Scrim).map(|z| z.id);
            return;
        };
        let (cx, cy) = ((cur.x + cur.w / 2) as i64, (cur.y + cur.h / 2) as i64);
        let next = self
            .zones
            .iter()
            .filter(|z| z.id != cur.id && z.id != Id::Scrim)
            .filter_map(|z| {
                let (zx, zy) = ((z.x + z.w / 2) as i64 - cx, (z.y + z.h / 2) as i64 - cy);
                let along = zx * dx + zy * dy;
                let perp = (zx * dy - zy * dx).abs();
                (along > 0 && perp <= along * 2).then_some((along + perp * 3, z.id))
            })
            .min_by_key(|t| t.0)
            .map(|t| t.1);
        if let Some(id) = next {
            self.focus = Some(id);
        }
    }

    fn is_slider(id: Id) -> bool {
        matches!(id, Id::QBright | Id::Bright | Id::QVol)
    }

    fn set_brightness(&mut self, pct: u8) -> Option<Action> {
        let pct = pct.clamp(MIN_PCT, 100);
        if self.cfg.brightness == pct {
            return None;
        }
        self.cfg.brightness = pct;
        Some(Action::Brightness(pct))
    }

    fn slide(&mut self, id: Id, x: usize, sx: usize, sw: usize) -> Option<Action> {
        let rel = x.saturating_sub(sx).min(sw);
        let pct = (MIN_PCT as usize + (100 - MIN_PCT as usize) * rel / sw.max(1)) as u8;
        if id == Id::QVol {
            self.vol_pct = (100 * rel / sw.max(1)) as u8;
            return None;
        }
        self.set_brightness(pct)
    }

    pub fn touch(&mut self, code: u64, x: usize, y: usize) -> Option<Action> {
        self.focus = None;
        let hit = self.hit(x, y);
        match code {
            input::TOUCH_DOWN => {
                self.pressed = hit;
                if let Some(id) = hit.filter(|&id| Self::is_slider(id)) {
                    let z = self.zones.iter().find(|z| z.id == id)?;
                    let (sx, sw) = (z.x + 14, z.w.saturating_sub(28));
                    self.drag = Some((id, sx, sw));
                    return self.slide(id, x, sx, sw);
                }
                None
            }
            input::TOUCH_MOVE => {
                if let Some((id, sx, sw)) = self.drag {
                    return self.slide(id, x, sx, sw);
                }
                if self.pressed != hit {
                    self.pressed = None;
                }
                None
            }
            input::TOUCH_UP => {
                if let Some((id, _, _)) = self.drag.take() {
                    self.pressed = None;
                    return (id != Id::QVol).then_some(Action::Save);
                }
                let p = self.pressed.take()?;
                if Some(p) == hit {
                    self.tap(p)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn pad(&mut self, code: u64) -> Option<Action> {
        use input::*;
        let open = self.modal.is_some() || self.sheet.is_some() || self.quick;
        let first = matches!(code, BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT | BTN_A);
        if self.page == LIBRARY && !open && self.focus.is_none() && first && !self.games.is_empty()
        {
            self.focus = Some(Id::Game(self.sel));
            if code != BTN_A {
                return None;
            }
        }
        if let (Some(f), BTN_LEFT | BTN_RIGHT) = (self.focus, code) {
            if Self::is_slider(f) && self.zones.iter().any(|z| z.id == f) {
                if f == Id::QVol {
                    self.vol_pct = if code == BTN_LEFT {
                        self.vol_pct.saturating_sub(PAD_STEP)
                    } else {
                        (self.vol_pct + PAD_STEP).min(100)
                    };
                    return None;
                }
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
            BTN_Y => return self.set_theme(!self.cfg.light),
            BTN_B if self.modal.is_some() => return self.tap(Id::No),
            BTN_B if self.sheet.is_some() => self.sheet = None,
            BTN_B if self.quick => self.quick = false,
            BTN_B if self.scan.is_some() => self.scan = None,
            BTN_B | BTN_HOME if self.page != LIBRARY || open => {
                self.quick = false;
                self.sheet = None;
                self.go(LIBRARY);
            }
            BTN_MINUS | BTN_PLUS if self.modal.is_some() == false && self.sheet.is_none() => {
                self.quick = !self.quick;
                self.focus = self.quick.then_some(Id::QSleep);
            }
            BTN_L | BTN_ZL if !open => self.go((self.page + TABS.len() - 1) % TABS.len()),
            BTN_R | BTN_ZR if !open => self.go((self.page + 1) % TABS.len()),
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
            Id::AvatarHome => self.go(LIBRARY),
            Id::AvatarTheme => return self.set_theme(!self.cfg.light),
            Id::Tab(i) => self.go(i),
            Id::BumperL => self.go((self.page + TABS.len() - 1) % TABS.len()),
            Id::BumperR => self.go((self.page + 1) % TABS.len()),
            Id::Scrim => {
                self.quick = false;
                self.sheet = None;
            }
            Id::StatusQuick => {
                self.quick = !self.quick;
                self.focus = self.quick.then_some(Id::QSleep);
            }
            Id::PeekLeft | Id::StackCard => {
                if !self.games.is_empty() {
                    self.sel = (self.sel + 1) % self.games.len();
                }
            }
            Id::AppTile(0) => self.go(SYSTEM),
            Id::AppTile(1) => self.go(TERMINAL),
            Id::AppTile(2) => self.go(NETWORK),
            Id::AppTile(3) => self.go(LOGS),
            Id::AppTile(4) => self.go(SETTINGS),
            Id::AppTile(_) => self.sheet = Some(Sheet::Power),
            Id::OpenPage(p) => self.go(p),
            Id::Game(i) if self.sel == i => return Some(Action::Launch(i)),
            Id::Game(i) => self.sel = i,
            Id::QSleep | Id::SleepNow => {
                self.quick = false;
                self.sheet = None;
                return Some(Action::Sleep);
            }
            Id::QAutoBright => {
                self.auto_bright = !self.auto_bright;
            }
            Id::QSettings => {
                self.quick = false;
                self.go(SETTINGS);
            }
            Id::QBright | Id::Bright | Id::QVol => {}
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
            Id::FanManual if self.fan_manual().is_none() => {
                let Seen::Value(f) = self.fan else {
                    return None;
                };
                return self.fan_set(f.percent);
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
        self.top_bar(s, d);
        if matches!(&self.toast, Some((_, end)) if now >= *end) {
            self.toast = None;
        }
        if let Some((t, _)) = &self.toast {
            let w = font::width(Face::Body, t) + 56;
            let (x, y) = ((W - w) / 2, CB - 52);
            s.shadow(x, y, w, 44, 22, 0xFF00_0000);
            s.fill_round(x, y, w, 44, 22, p.text);
            s.text_c(W / 2, vc(Face::Body, y, 44), t, Face::Body, p.bg);
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
            if !matches!(self.focus, Some(Id::Game(_) | Id::QSleep)) {
                s.ring_round(x - 3, y - 3, w + 6, h + 6, r + 3, 2, p.focus);
            }
        }
    }

    /// Draw a red circular player/nxk avatar centered on (cx, cy) with radius `r`.
    fn draw_red_avatar(&self, s: &mut Screen, cx: usize, cy: usize, r: usize) {
        let red = rgb_hex(0xE60012);
        s.disc(cx, cy, r, red);
        s.ring(cx, cy, r, 2, 0xFFFF_FFFF);
        // Stylized character face / cap inside the red circle.
        s.fill_round(cx - r / 2, cy - r / 2, r, r / 2, r / 4, rgb_hex(0xB8000E));
        s.disc(cx, cy + 2, r * 9 / 20, rgb_hex(0xFFD2B4));
        s.fill_round(cx - r / 2, cy - r / 3, r, r / 4, 3, red);
        s.disc(cx, cy - r / 3, 3, 0xFFFF_FFFF);
        s.disc(cx - 4, cy + 1, 2, rgb_hex(0x18181C));
        s.disc(cx + 4, cy + 1, 2, rgb_hex(0x18181C));
    }

    /// Top status cluster (`10:25 PM  [wifi]  100% [battery]`) ending at `xr`, centered on `yc`.
    fn status_cluster(&self, s: &mut Screen, d: &Data, xr: usize, yc: usize) -> usize {
        let p = pal();
        let mut x = xr;
        // Battery icon at right.
        icon(s, Icon::Battery, x - 14, yc, p.text, p.bg);
        x -= 34;
        // 100% text.
        let pct_w = s.text_r(x, vc(Face::Cap, yc - 14, 28), "100%", Face::Cap, p.text);
        x -= pct_w + 14;
        // Wi-Fi wave or plane icon.
        let (wt, _) = wifi_short(d);
        if wt == "radio off" || self.cfg.flight {
            icon(s, Icon::Plane, x - 8, yc, p.dim, p.bg);
        } else {
            icon(s, Icon::WifiWave, x - 8, yc, p.text, p.bg);
        }
        x -= 28;
        // Time `10:25 PM`.
        let clk = clock_ampm(self.wall);
        let cw = s.text_r(x, vc(Face::Cap, yc - 14, 28), &clk, Face::Cap, p.text);
        x - cw
    }

    /// Top navigation bar: avatars on the left, `[L] Games System Network Terminal Logs Settings [R]` in the center, status cluster on the right.
    fn top_bar(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let yc = 52usize;

        // Left: two circular avatars (Home red avatar + Theme avatar).
        self.draw_red_avatar(s, M + 22, yc, 22);
        self.zone(Id::AvatarHome, M, yc - 24, 48, 48, 24);

        let ax2 = M + 62;
        s.disc(ax2 + 22, yc, 22, p.raised);
        s.ring(ax2 + 22, yc, 22, 1, p.line);
        icon(
            s,
            if self.cfg.light {
                Icon::Sun
            } else {
                Icon::Moon
            },
            ax2 + 22,
            yc,
            p.text,
            p.raised,
        );
        self.zone(Id::AvatarTheme, ax2, yc - 24, 48, 48, 24);

        // Center: `[ L ]` bumper + tabs + `[ R ]` bumper.
        let mut tx = 296usize;
        s.fill_round(tx, yc - 10, 34, 20, 6, p.dim);
        s.text_c(tx + 17, vc(Face::Cap, yc - 10, 20), "L", Face::Cap, p.bg);
        self.zone(Id::BumperL, tx - 4, yc - 18, 42, 36, 8);
        tx += 52;

        for (i, &name) in TABS.iter().enumerate() {
            let active = self.page == i;
            let f = if active { Face::BodyB } else { Face::Body };
            let col = if active { p.text } else { p.dim };
            let tw = s.text(tx, vc(f, yc - 18, 36), name, f, col);
            let has_dot = match i {
                NETWORK => wifi_connected(d).is_some(),
                TERMINAL => matches!(d.usb, Now::Value(u) if u.state & usb::ST_TERMINAL != 0),
                LOGS => self.unread > 0,
                _ => false,
            };
            if has_dot {
                s.disc(tx + tw + 6, yc - 6, 3, p.good);
            }
            self.zone(Id::Tab(i), tx - 10, yc - 22, tw + 20, 44, 8);
            tx += tw + 34;
        }

        s.fill_round(tx - 6, yc - 10, 34, 20, 6, p.dim);
        s.text_c(tx + 11, vc(Face::Cap, yc - 10, 20), "R", Face::Cap, p.bg);
        self.zone(Id::BumperR, tx - 10, yc - 18, 42, 36, 8);

        // Right: clock, Wi-Fi, battery (opens Quick Settings).
        let left_status = self.status_cluster(s, d, W - M, yc);
        self.zone(
            Id::StatusQuick,
            left_status.saturating_sub(12),
            yc - 24,
            W - M - left_status + 24,
            48,
            10,
        );
    }

    /// Bottom bar: handheld Switch icon on the left, button hints on the right.
    fn hints(&self, s: &mut Screen, items: &[(&str, &str)]) {
        let p = pal();
        let y = H - 46;
        icon(s, Icon::SwitchHandheld, M + 24, y + 14, p.text, p.bg);
        // When Quick Settings is open, place the main screen hints just left of the drawer (x = 700), exactly like the reference photo!
        let mut x = if self.quick { 696 } else { W - M };
        for (b, t) in items.iter().rev() {
            x -= font::width(Face::Cap, t);
            s.text(x, vc(Face::Cap, y, 28), t, Face::Cap, p.text);
            x -= 8 + 20;
            s.disc(x + 10, y + 14, 10, p.text);
            s.text_c(x + 10, vc(Face::Cap, y, 28), b, Face::Cap, p.bg);
            x -= 24;
        }
    }

    /// Back button and title of a sub-view (kept minimal since the top bar has tabs).
    pub(crate) fn page_head(&mut self, _s: &mut Screen, _title: &str) {}

    /// Button: filled pill in `tone`, or raised surface when `Plain`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn button(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        label: &str,
        tone: Tone,
        enabled: bool,
    ) {
        let p = pal();
        let r = (h / 2).min(10);
        let down = enabled && self.pressed == Some(id);
        let (bg, fg) = if !enabled {
            (p.raised, p.dim)
        } else if tone == Tone::Plain {
            (press(p.raised, down), p.text)
        } else {
            let c = tone_color(tone);
            let bg = if down { mix(c, 0xFFFF_FFFF, 50) } else { c };
            (bg, p.on_accent)
        };
        s.fill_round(x, y, w, h, r, bg);
        if tone == Tone::Plain {
            s.ring_round(x, y, w, h, r, 1, p.line);
        }
        s.text_c(x + w / 2, vc(Face::BodyB, y, h), label, Face::BodyB, fg);
        if enabled {
            self.zone(id, x, y, w, h, r);
        }
    }

    /// Pill choice (tabs, sleep delay, theme).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pill(
        &mut self,
        s: &mut Screen,
        id: Id,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        label: &str,
        on: bool,
    ) {
        self.chip(s, id, x, y, w, h, label, on, true);
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
        s.fill_round(x, y, w, h, 8, bg);
        if !chosen {
            s.ring_round(x, y, w, h, 8, 1, p.line);
        }
        s.text_c(x + w / 2, vc(Face::Body, y, h), text, Face::Body, fg);
        if on {
            self.zone(id, x, y, w, h, 8);
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
        s.fill_round(x + 16, y, w - 32, 48, 8, bg);
        s.text(x + 32, vc(Face::Body, y, 48), label, Face::Body, p.dim);
        s.text_r(x + w - 64, vc(Face::Body, y, 48), text, Face::Body, c);
        icon(s, Icon::Chevron, x + w - 40, y + 24, p.dim, bg);
        self.zone(id, x + 16, y, w - 32, 48, 8);
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
            s.ring_round(x, y, w, h, 10, 2, p.accent);
        }
        self.zone(id, x, y, w, h, 10);
    }

    /// Home screen (`Games` tab) matching the reference Switch UI:
    /// - Left peek card (`0..64`)
    /// - Selected tall portrait game card (`82, 148, 272x388`) with cover art, silver-white border, and red avatar + `Playing` badge
    /// - `(Y) Change User` below selected card
    /// - Middle stacked `Retro Time  2` card (`374, 164, 228x352`)
    /// - Right 2x2 square app tiles (`System`, `Terminal`, `Network`, `Logs`, plus `Settings` & `Power`)
    /// - Selected game title in cyan with blue game-card icon below the right cards
    fn home(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        self.scroll += (self.sel as f32 - self.scroll) * 0.35;
        if (self.scroll - self.sel as f32).abs() < 0.004 {
            self.scroll = self.sel as f32;
        }
        let n = self.games.len();
        let sel_cov = self.games.get(self.sel).map_or(0, |g| g.cover);
        let subtitle: String = self
            .games
            .get(self.sel)
            .map_or_else(|| "THIS WEEK: 7 HOURS".into(), |g| g.subtitle.clone());
        let title_str: String = self.games.get(self.sel).map_or_else(
            || "Pokémon Legends: Arceus / Cube".into(),
            |g| g.title.clone(),
        );

        // Uppercase play-time header above the selected card.
        s.text(84, 114, &subtitle, Face::Cap, p.dim);

        // 1. Left-edge peeking card (x = 0..64, y = 164, w = 64, h = 352).
        s.cover(3, 0, 164, 64, 352, 4);
        self.zone(Id::PeekLeft, 0, 164, 64, 352, 4);

        // 2. Selected Tall Portrait Game Card (x = 82, y = 148, w = 272, h = 388).
        s.shadow(82, 148, 272, 388, 8, 0xFF00_0000);
        s.fill_round(82, 148, 272, 388, 6, p.bg);
        s.cover(sel_cov, 88, 154, 260, 376, 4);
        s.ring_round(82, 148, 272, 388, 6, 2, p.focus);

        // Bottom-left "Playing" badge inside the selected card.
        s.round_a(98, 474, 142, 42, 21, 0xFF00_0000, 175);
        self.draw_red_avatar(s, 119, 495, 19);
        s.text(
            146,
            vc(Face::Cap, 474, 42),
            "Playing",
            Face::Cap,
            0xFFFF_FFFF,
        );
        self.zone(Id::Game(self.sel), 82, 148, 272, 388, 6);

        // Below the selected card: `(Y) Change User`.
        s.disc(92, 558, 9, p.text);
        s.text_c(92, vc(Face::Cap, 548, 20), "Y", Face::Cap, p.bg);
        s.text(
            108,
            vc(Face::Cap, 548, 20),
            "Change User",
            Face::Cap,
            p.text,
        );
        self.zone(Id::AvatarTheme, 82, 544, 160, 28, 6);

        // 3. Middle Stacked Card: "Retro Time  2" + stacked cover art (x = 374, y = 164, w = 228, h = 352).
        let (sx, sy, sw, sh) = (374, 164, 228, 352);
        s.fill_round(sx, sy, sw, sh, 4, p.surface);
        s.text(
            sx + 10,
            vc(Face::Cap, sy, 34),
            "Retro Time",
            Face::Cap,
            p.text,
        );
        s.text_r(
            sx + sw - 10,
            vc(Face::Cap, sy, 34),
            &format!("{}", n.max(2)),
            Face::Cap,
            p.text,
        );
        let stack_cov = if sel_cov == 0 { 2 } else { 0 };
        s.cover(stack_cov, sx, sy + 34, sw, sh - 34, 4);
        self.zone(Id::StackCard, sx, sy, sw, sh, 4);

        // 4. Right 2x2 Grid of Square App Tiles (164 x 168 each, gap 16).
        let (gx, gy, tw, th, gap) = (622, 164, 164, 168, 16);

        // Tile 0 (Top-left): System (Netflix-black & red 'N' style).
        s.fill_round(gx, gy, tw, th, 4, rgb_hex(0x050507));
        s.fill_round(gx + 60, gy + 44, 12, 80, 2, rgb_hex(0xB80710));
        s.fill_round(gx + 92, gy + 44, 12, 80, 2, rgb_hex(0xB80710));
        s.line(
            (gx + 66) as i32,
            (gy + 46) as i32,
            (gx + 98) as i32,
            (gy + 122) as i32,
            12,
            rgb_hex(0xE50914),
        );
        self.zone(Id::AppTile(0), gx, gy, tw, th, 4);

        // Tile 1 (Top-right): Terminal (Twitch-purple style).
        let tx1 = gx + tw + gap;
        s.fill_round(tx1, gy, tw, th, 4, rgb_hex(0x772CE8));
        s.fill_round(tx1 + 48, gy + 44, 68, 64, 6, 0xFFFF_FFFF);
        s.fill_round(tx1 + 56, gy + 52, 52, 46, 4, rgb_hex(0x0A0A0C));
        s.fill_round(tx1 + 72, gy + 64, 6, 18, 1, 0xFFFF_FFFF);
        s.fill_round(tx1 + 90, gy + 64, 6, 18, 1, 0xFFFF_FFFF);
        self.zone(Id::AppTile(1), tx1, gy, tw, th, 4);

        // Tile 2 (Bottom-left): Network (Disney+ royal blue style).
        let ty2 = gy + th + gap;
        s.fill_round(gx, ty2, tw, th, 4, rgb_hex(0x0A1E5E));
        s.round_a(
            gx + 18,
            ty2 + 24,
            tw - 36,
            th - 48,
            48,
            rgb_hex(0x1E56C8),
            110,
        );
        s.ring_round(gx + 34, ty2 + 48, 96, 46, 23, 2, rgb_hex(0x74C0FC));
        s.fill_round(gx + 28, ty2 + 72, 108, 28, 4, rgb_hex(0x0A1E5E));
        s.text_c(
            gx + tw / 2,
            vc(Face::TitleB, ty2 + 62, 48),
            "net+",
            Face::TitleB,
            0xFFFF_FFFF,
        );
        self.zone(Id::AppTile(2), gx, ty2, tw, th, 4);

        // Tile 3 (Bottom-right): Logs (Spotify emerald circle style).
        s.fill_round(tx1, ty2, tw, th, 4, rgb_hex(0x121316));
        let (scx, scy) = (tx1 + tw / 2, ty2 + th / 2);
        s.disc(scx, scy, 46, rgb_hex(0x1ED760));
        for (dy, hw) in [(-12i32, 24i32), (0, 20), (12, 15)] {
            s.line(
                scx as i32 - hw,
                scy as i32 + dy,
                scx as i32 + hw,
                scy as i32 + dy + 2,
                5,
                rgb_hex(0x121316),
            );
        }
        self.zone(Id::AppTile(3), tx1, ty2, tw, th, 4);

        // Column 3 of tiles (Settings & Power, visible when Quick Settings is closed).
        let tx2 = tx1 + tw + gap;
        s.fill_round(tx2, gy, tw, th, 4, p.surface);
        s.ring_round(tx2, gy, tw, th, 4, 1, p.line);
        icon(
            s,
            Icon::Sliders,
            tx2 + tw / 2,
            gy + th / 2 - 12,
            p.good,
            p.surface,
        );
        s.text_c(tx2 + tw / 2, gy + th - 44, "Settings", Face::Cap, p.text);
        self.zone(Id::AppTile(4), tx2, gy, tw, th, 4);

        s.fill_round(tx2, ty2, tw, th, 4, p.surface);
        s.ring_round(tx2, ty2, tw, th, 4, 1, p.line);
        icon(
            s,
            Icon::Power,
            tx2 + tw / 2,
            ty2 + th / 2 - 12,
            p.bad,
            p.surface,
        );
        s.text_c(tx2 + tw / 2, ty2 + th - 44, "Power", Face::Cap, p.text);
        self.zone(Id::AppTile(5), tx2, ty2, tw, th, 4);

        // Selected game title in electric cyan-blue below the middle & right cards.
        icon(s, Icon::GameCard, sx + 10, 548, p.accent, p.bg);
        s.text(
            sx + 28,
            vc(Face::Title, 530, 36),
            &fit(Face::Title, &title_str, W - sx - 80),
            Face::Title,
            p.accent,
        );

        let _ = d;
        self.hints(s, &[("-", "Explore"), ("+", "Options")]);
    }

    /// A brightness slider row in Quick Settings or Settings.
    fn bright_slider(&mut self, s: &mut Screen, id: Id, x: usize, y: usize, w: usize) {
        let p = pal();
        icon(s, Icon::Sun, x + 10, y + 4, p.text, p.surface);
        let pct = self.cfg.brightness.max(MIN_PCT);
        slider(s, x + 42, y, w - 46, pct, true);
        self.zone(id, x + 28, y - 18, w - 28, 44, 8);
    }

    /// Right-side Quick Settings overlay matching the reference Switch photo:
    /// - Top status cluster (`10:25 PM  [wifi]  100% [battery]`)
    /// - Outlined `Sleep Mode` button (`r = 6`, gradient sheen + 2px border)
    /// - `Automatic Brightness` (`On` in mint green)
    /// - Brightness slider with white rounded-square knob
    /// - Volume slider with white rounded-square knob
    /// - `Flight Mode` (`Off`) + 2-line explanation caption
    /// - `System Settings` with 3-slider icon
    /// - Bottom bar with handheld Switch icon + `(B) Back   (A) OK`
    fn quick_panel(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let (px, pw) = (720, 560);
        // Lightly dim the left side so the home screen stays clearly visible underneath.
        s.fill_alpha(0, 0, px, H, p.scrim, 75);
        self.zone(Id::Scrim, 0, 0, px, H, 0);

        s.fill(px, 0, pw, H, p.surface);
        s.fill(px, 0, 1, H, p.line);

        let (x, w) = (px + 36, pw - 72);

        // Top-right status bar inside the drawer.
        self.status_cluster(s, d, W - 36, 44);

        // 1. Sleep Mode button (y = 84, h = 68, r = 6) with crisp focus border.
        let sy = 84;
        let down = self.pressed == Some(Id::QSleep);
        s.fill_round(x, sy, w, 68, 6, press(p.raised, down));
        s.ring_round(x, sy, w, 68, 6, 2, p.focus);
        icon(
            s,
            Icon::Power,
            x + 30,
            sy + 34,
            p.text,
            press(p.raised, down),
        );
        s.text(
            x + 52,
            vc(Face::Body, sy, 68),
            "Sleep Mode",
            Face::Body,
            p.text,
        );
        self.zone(Id::QSleep, x, sy, w, 68, 6);

        // 2. Automatic Brightness row (y = 174).
        let ay = 174;
        s.text(
            x + 6,
            vc(Face::Body, ay, 36),
            "Automatic Brightness",
            Face::Body,
            p.text,
        );
        s.text_r(
            x + w - 6,
            vc(Face::Body, ay, 36),
            if self.auto_bright { "On" } else { "Off" },
            Face::Body,
            if self.auto_bright { p.good } else { p.dim },
        );
        self.zone(Id::QAutoBright, x, ay, w, 36, 6);

        // 3. Brightness slider (y = 236).
        self.bright_slider(s, Id::QBright, x + 4, 236, w - 8);

        // 4. Volume slider (y = 298, visual-only as audio service is disabled).
        icon(s, Icon::Speaker, x + 14, 302, p.text, p.surface);
        slider(s, x + 46, 298, w - 54, self.vol_pct, false);
        self.zone(Id::QVol, x + 32, 280, w - 32, 44, 8);

        // Divider.
        s.fill(x + 4, 348, w - 8, 1, p.line);

        // 5. Flight Mode row + 2-line caption (y = 366).
        let fy = 366;
        s.text(
            x + 6,
            vc(Face::Body, fy, 36),
            "Flight Mode",
            Face::Body,
            p.text,
        );
        s.text_r(
            x + w - 6,
            vc(Face::Body, fy, 36),
            if self.cfg.flight { "On" } else { "Off" },
            Face::Body,
            if self.cfg.flight { p.good } else { p.dim },
        );
        s.text(
            x + 6,
            fy + 42,
            "If you turn flight mode on, you will no longer",
            Face::Cap,
            p.dim,
        );
        s.text(
            x + 6,
            fy + 64,
            "be able to use wireless controllers.",
            Face::Cap,
            p.dim,
        );
        self.zone(Id::QFlight, x, fy, w, 88, 6);

        // Divider.
        s.fill(x + 4, 468, w - 8, 1, p.line);

        // 6. System Settings row (y = 486).
        let ssy = 486;
        if self.pressed == Some(Id::QSettings) {
            s.fill_round(x, ssy, w, 46, 6, p.raised);
        }
        icon(s, Icon::Sliders, x + 16, ssy + 23, p.text, p.surface);
        s.text(
            x + 40,
            vc(Face::Body, ssy, 46),
            "System Settings",
            Face::Body,
            p.text,
        );
        self.zone(Id::QSettings, x, ssy, w, 46, 6);

        // 7. Theme row (y = 540, quick Dark / Light toggle).
        let ty = 540;
        if self.pressed == Some(Id::QTheme) {
            s.fill_round(x, ty, w, 44, 6, p.raised);
        }
        icon(
            s,
            if self.cfg.light {
                Icon::Sun
            } else {
                Icon::Moon
            },
            x + 16,
            ty + 22,
            p.text,
            p.surface,
        );
        s.text(x + 40, vc(Face::Body, ty, 44), "Theme", Face::Body, p.text);
        s.text_r(
            x + w - 6,
            vc(Face::Body, ty, 44),
            if self.cfg.light { "Light" } else { "Dark" },
            Face::Body,
            p.good,
        );
        self.zone(Id::QTheme, x, ty, w, 44, 6);

        // Bottom bar inside the Quick Settings drawer.
        let by = H - 46;
        icon(s, Icon::SwitchHandheld, x + 24, by + 14, p.text, p.surface);
        let mut bx = W - 36;
        for (b, t) in [("B", "Back"), ("A", "OK")].iter().rev() {
            bx -= font::width(Face::Cap, t);
            s.text(bx, vc(Face::Cap, by, 28), t, Face::Cap, p.text);
            bx -= 8 + 20;
            s.disc(bx + 10, by + 14, 10, p.text);
            s.text_c(bx + 10, vc(Face::Cap, by, 28), b, Face::Cap, p.surface);
            bx -= 24;
        }
    }

    /// Settings: categories on the left, the chosen one on the right.
    fn settings(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let lw = 300usize;
        for (i, name) in CATS.iter().enumerate() {
            let y = CY + 8 + i * 72;
            let (on, down) = (self.cat == i, self.pressed == Some(Id::Cat(i)));
            let bg = if on { p.surface } else { press(p.bg, down) };
            s.fill_round(M, y, lw, 60, 8, bg);
            if on {
                s.ring_round(M, y, lw, 60, 8, 1, p.line);
                s.fill_round(M + 2, y + 14, 4, 32, 2, p.accent);
            }
            s.text(
                M + 28,
                vc(Face::Body, y, 60),
                name,
                if on { Face::BodyB } else { Face::Body },
                if on { p.text } else { p.dim },
            );
            self.zone(Id::Cat(i), M, y, lw, 60, 8);
        }
        let (x, y, w, h) = (M + lw + 36, CY, W - 2 * M - lw - 36, CH);
        card(s, x, y, w, h);
        s.text(x + 32, y + 22, CATS[self.cat], Face::TitleB, p.text);
        let (cx, cw) = (x + 32, w - 64);
        match self.cat {
            0 => self.cat_display(s, cx, y + 88, cw),
            1 => {
                s.text(cx, y + 96, "Volume", Face::Body, p.dim);
                s.text_r(cx + cw, y + 96, "audio off", Face::Body, p.dim);
                icon(s, Icon::Speaker, cx + 14, y + 156, p.dim, p.surface);
                slider(s, cx + 58, y + 152, cw - 58, self.vol_pct, false);
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
        let b = format!("{}%", self.cfg.brightness.max(MIN_PCT));
        s.text_r(x + w, y + 92, &b, Face::Body, p.good);
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
        for (i, &label) in SLEEP_LABELS.iter().enumerate() {
            let bx = x + w - (SLEEP_LABELS.len() - i) * (bw + 8) + 8;
            self.chip(
                s,
                Id::AutoSleep(i),
                bx,
                y + 224,
                bw,
                40,
                label,
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
            Some(n) if n.addr != 0 => format!("{}/{}", crate::screen::Ipv4(n.addr), n.prefix),
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
        let (lw, lh) = crate::screen::logo_size();
        s.logo(
            x + w - lw * 2,
            y - 66 + (48usize.saturating_sub(lh * 2)) / 2,
            2,
        );
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
            ("Uptime", format!("{}", crate::screen::Hms(d.up_s))),
            (
                "Memory",
                d.mem.map_or("-".into(), |m| {
                    format!(
                        "{}",
                        crate::screen::Bytes(m.total_pages * libnx::abi::layout::PAGE_SIZE)
                    )
                }),
            ),
            (
                "SD card",
                match d.disk {
                    Seen::Value((_, total)) => format!("{}", crate::screen::Bytes(total)),
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
        s.shadow(x, y, w, h, 14, 0xFF00_0000);
        s.card_round(x, y, w, h, 14, 1, p.line, p.surface);
        s.text(x + 40, y + 30, title, Face::TitleB, p.text);
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

    fn gpu_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "GPU clock");
        match self.gpu {
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
                        s.ring_round(bx, by, bw, bh, 8, 2, p.warn);
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

    fn power_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = (360, 150, 560, 420);
        s.fill_alpha(0, 0, W, H, p.scrim, 150);
        self.zone(Id::Scrim, 0, 0, W, H, 0);
        s.shadow(x, y, w, h, 14, 0xFF00_0000);
        s.card_round(x, y, w, h, 14, 1, p.line, p.surface);
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
        s.shadow(x, y, w, h, 14, 0xFF00_0000);
        s.card_round(x, y, w, h, 14, 1, p.line, p.surface);
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

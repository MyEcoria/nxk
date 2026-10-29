//! The Wi-Fi sheet over the Network page: the networks in range as a list
//! (touch drag, D-pad), a tap joins one; a secured one first asks for its
//! passphrase on the on-screen keyboard. The join itself runs in a thread
//! of `main` (`wlan` holds the call for seconds); the sheet only keeps
//! what it shows and hands over the request (`take_join`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::proto::{input, wifi};

use crate::avatar::padlock;
use crate::font::{self, Face};
use crate::keyboard::Keys;
use crate::screen::Screen;
use crate::stats::Data;
use crate::status::wifi_connected;
use crate::theme::pal;
use crate::ui::{Action, Id, ScanNet, Sheet, Ui};
use crate::widgets::{bars_for, fit, press, signal_bars, Tone};

/// A row of the list, and the distance between two.
const ROW: usize = 58;
const PITCH: usize = 66;
/// The list's window: from `LIST_TOP` below the sheet's top, this tall.
const LIST_TOP: usize = 96;
const VIEW: usize = 392;
/// Longest passphrase: 63 characters, or 64 hex digits for the raw key.
const PSK_MAX: usize = 64;
const PSK_MIN: usize = 8;
/// A finger moves this far before a touch is a drag, not a tap.
const SLOP: i64 = crate::pan::TOUCH_SLOP;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Stage {
    List,
    Password,
}

/// A touch on the list: where it began, the offset then, whether it moved.
#[derive(Clone, Copy)]
pub(crate) struct Drag {
    y0: usize,
    off0: usize,
    moved: bool,
}

pub(crate) struct WifiUi {
    /// What the last scan found: one row per name, the strongest each.
    pub nets: Vec<ScanNet>,
    pub scanning: bool,
    pub scanned: bool,
    /// Why the last scan failed.
    pub scan_err: Option<String>,
    /// Pixels the list is scrolled by.
    off: usize,
    drag: Option<Drag>,
    pub stage: Stage,
    /// The network being joined or asked for: its name as bytes and shown.
    target: Option<(Vec<u8>, String, u8)>,
    pass: String,
    show: bool,
    symbols: bool,
    shift: bool,
    /// A join in progress (its name), and why the last one failed.
    pub joining: Option<String>,
    error: Option<String>,
    /// The request waiting for `main`: name and passphrase.
    request: Option<(Vec<u8>, Vec<u8>)>,
}

impl WifiUi {
    pub fn new() -> Self {
        Self {
            nets: Vec::new(),
            scanning: false,
            scanned: false,
            scan_err: None,
            off: 0,
            drag: None,
            stage: Stage::List,
            target: None,
            pass: String::new(),
            show: false,
            symbols: false,
            shift: false,
            joining: None,
            error: None,
            request: None,
        }
    }

    fn max_off(&self) -> usize {
        (self.nets.len() * PITCH).saturating_sub(PITCH - ROW + VIEW)
    }

    fn scroll_to(&mut self, i: usize) {
        let (top, bottom) = (i * PITCH, i * PITCH + ROW);
        if top < self.off {
            self.off = top;
        } else if bottom > self.off + VIEW {
            self.off = bottom - VIEW;
        }
        self.off = self.off.min(self.max_off());
    }

    /// Something moves on the sheet (the joining marker).
    pub fn busy(&self) -> bool {
        self.joining.is_some() || self.scanning
    }

    /// The passphrase as typed is one the chip may be given: 8 to 63
    /// characters, or the 64 hex digits of the key itself.
    fn pass_ok(&self) -> Result<(), &'static str> {
        match self.pass.len() {
            PSK_MIN..PSK_MAX => Ok(()),
            PSK_MAX if self.pass.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(()),
            PSK_MAX => Err("64 characters must be hexadecimal digits"),
            _ => Err("The passphrase has 8 to 63 characters"),
        }
    }
}

/// One row per name, none without a name, the strongest first; the one
/// joined comes first of all.
fn rows(mut nets: Vec<ScanNet>, joined: Option<&[u8]>) -> Vec<ScanNet> {
    nets.retain(|n| !n.raw.is_empty());
    nets.sort_by_key(|n| n.signal);
    let mut out: Vec<ScanNet> = Vec::new();
    for n in nets {
        if out.iter().all(|o| o.raw != n.raw) {
            out.push(n);
        }
    }
    out.sort_by_key(|n| Some(n.raw.as_slice()) != joined);
    out
}

/// Words for a join that failed.
pub(crate) fn join_error(e: i64) -> String {
    match e.abs() {
        err::EACCES => "The access point refused: wrong passphrase?".into(),
        err::EINVAL => "The passphrase has 8 to 63 characters".into(),
        err::EOPNOTSUPP => "Not supported: WPA2 with AES only (no TKIP, no WPA3)".into(),
        err::ENOENT => "Network not found: scan again".into(),
        err::ETIMEDOUT => "No answer from the access point".into(),
        err::ENETDOWN => "The radio is off (Flight Mode)".into(),
        err::EIO => "The Wi-Fi chip failed: try Retry".into(),
        _ => format!("wifi: {}", err::name(e)),
    }
}

fn security_text(sec: u8) -> &'static str {
    match sec {
        wifi::SEC_OPEN => "Open",
        wifi::SEC_WEP => "WEP",
        wifi::SEC_WPA => "WPA",
        wifi::SEC_WPA2 => "WPA2",
        _ => "WPA3 only",
    }
}

/// Open and WPA2 (AES) are what `wlan` joins.
fn joinable(sec: u8) -> bool {
    matches!(sec, wifi::SEC_OPEN | wifi::SEC_WPA2)
}

impl Ui {
    /// The Wi-Fi sheet opens and looks for networks.
    pub(crate) fn wifi_open(&mut self) -> Option<Action> {
        self.sheet = Some(Sheet::Wifi);
        self.wifi.stage = Stage::List;
        self.wifi.off = 0;
        self.wifi.error = None;
        self.wifi_scan()
    }

    fn wifi_scan(&mut self) -> Option<Action> {
        if self.wifi.scanning {
            return None;
        }
        self.wifi.scanning = true;
        Some(Action::Scan)
    }

    /// A scan came back.
    pub fn wifi_scanned(&mut self, r: Result<Vec<ScanNet>, i64>, joined: Option<&[u8]>) {
        self.wifi.scanning = false;
        match r {
            Ok(nets) => {
                self.wifi.nets = rows(nets, joined);
                self.wifi.scan_err = None;
                self.wifi.off = self.wifi.off.min(self.wifi.max_off());
            }
            Err(e) => self.wifi.scan_err = Some(join_error(e)),
        }
        self.wifi.scanned = true;
    }

    /// The join `main` has to run: the network's name and its passphrase.
    pub fn take_join(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
        self.wifi.request.take()
    }

    fn wifi_begin(&mut self, raw: Vec<u8>, name: String, psk: Vec<u8>) -> Option<Action> {
        self.wifi.joining = Some(name);
        self.wifi.error = None;
        self.wifi.request = Some((raw, psk));
        Some(Action::Join)
    }

    /// A join ended; the words for a toast when the sheet is not the one
    /// to show them.
    pub fn wifi_joined(&mut self, name: &str, r: &Result<(), i64>, quiet: bool) -> Option<String> {
        if self.wifi.joining.as_deref() == Some(name) {
            self.wifi.joining = None;
        }
        let asked = !quiet && self.sheet == Some(Sheet::Wifi) && self.wifi.stage == Stage::Password;
        match r {
            Ok(()) => {
                // The boot-time join leaves a passphrase half typed alone.
                if !quiet {
                    if self.sheet == Some(Sheet::Wifi) {
                        self.sheet = None;
                    }
                    self.wifi.stage = Stage::List;
                    self.wifi.pass.clear();
                }
                Some(format!("Wi-Fi: joined {}", name))
            }
            Err(e) if asked => {
                self.wifi.error = Some(join_error(*e));
                None
            }
            Err(_) if quiet => None,
            Err(e) => Some(format!("{}: {}", name, join_error(*e))),
        }
    }

    fn wifi_back(&mut self) {
        self.wifi.stage = Stage::List;
        self.wifi.pass.clear();
        self.wifi.error = None;
        self.wifi.show = false;
        self.focus = None;
    }

    fn wifi_submit(&mut self) -> Option<Action> {
        if self.wifi.joining.is_some() {
            return None;
        }
        if let Err(why) = self.wifi.pass_ok() {
            self.wifi.error = Some(why.into());
            return None;
        }
        let (raw, name, _) = self.wifi.target.clone()?;
        let psk = self.wifi.pass.clone().into_bytes();
        self.wifi_begin(raw, name, psk)
    }

    /// A tap or A on `id` while the sheet is up: `None` when it is not
    /// the sheet's.
    pub(crate) fn wifi_tap(&mut self, id: Id) -> Option<Option<Action>> {
        if self.sheet != Some(Sheet::Wifi) {
            return None;
        }
        let busy = self.wifi.joining.is_some();
        let typing = self.wifi.stage == Stage::Password && !busy;
        Some(match id {
            // A stray touch outside must not lose a passphrase half typed.
            Id::Scrim if self.wifi.stage == Stage::Password => None,
            Id::WifiNet(i) if self.wifi.stage == Stage::List && !busy => {
                let n = self.wifi.nets.get(i)?;
                let (raw, name, sec) = (n.raw.clone(), n.ssid.clone(), n.security);
                if !joinable(sec) {
                    let t = format!("{}: {} is not supported", name, security_text(sec));
                    self.toast(t, libnx::time::now_ns());
                    return Some(None);
                }
                if sec == wifi::SEC_OPEN {
                    self.wifi_begin(raw, name, Vec::new())
                } else {
                    self.wifi.target = Some((raw, name, sec));
                    self.wifi.stage = Stage::Password;
                    self.wifi.pass.clear();
                    self.wifi.error = None;
                    self.wifi.symbols = false;
                    self.wifi.shift = false;
                    self.focus = Some(Id::Key(b'a'));
                    None
                }
            }
            Id::WifiRescan if self.wifi.stage == Stage::List && !busy => self.wifi_scan(),
            Id::WifiBack => {
                self.wifi_back();
                None
            }
            Id::WifiJoin => self.wifi_submit(),
            Id::WifiShow => {
                self.wifi.show = !self.wifi.show;
                None
            }
            Id::KeyLayer if typing => {
                self.wifi.symbols = !self.wifi.symbols;
                None
            }
            Id::KeyShift if typing => {
                self.wifi.shift = !self.wifi.shift;
                None
            }
            Id::Key(c) if typing => {
                if self.wifi.pass.len() < PSK_MAX {
                    let c = if self.wifi.shift && !self.wifi.symbols {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    };
                    self.wifi.pass.push(c as char);
                }
                self.wifi.shift = false;
                self.wifi.error = None;
                None
            }
            Id::KeySpace if typing => {
                if self.wifi.pass.len() < PSK_MAX {
                    self.wifi.pass.push(' ');
                }
                self.wifi.error = None;
                None
            }
            Id::KeyBack if typing => {
                self.wifi.pass.pop();
                self.wifi.error = None;
                None
            }
            Id::WifiNet(_)
            | Id::WifiRescan
            | Id::Key(_)
            | Id::KeySpace
            | Id::KeyBack
            | Id::KeyShift
            | Id::KeyLayer => None,
            _ => return None,
        })
    }

    /// A button while the sheet is up: `None` when the generic handling
    /// is to run.
    pub(crate) fn wifi_pad(&mut self, code: u64) -> Option<Option<Action>> {
        use input::*;
        if self.sheet != Some(Sheet::Wifi) || self.modal.is_some() {
            return None;
        }
        let busy = self.wifi.joining.is_some();
        if self.wifi.stage == Stage::Password {
            return match code {
                BTN_B if !busy => {
                    self.wifi_back();
                    Some(None)
                }
                BTN_X if !busy => {
                    self.wifi.pass.pop();
                    Some(None)
                }
                BTN_Y => {
                    self.wifi.show = !self.wifi.show;
                    Some(None)
                }
                BTN_PLUS => Some(self.wifi_submit()),
                _ => None,
            };
        }
        let n = self.wifi.nets.len();
        let at = match self.focus {
            Some(Id::WifiNet(i)) => Some(i),
            _ => None,
        };
        let to = match (code, at) {
            (BTN_DOWN, Some(i)) if i + 1 < n => Some(Id::WifiNet(i + 1)),
            (BTN_DOWN, Some(_)) => Some(Id::WifiRescan),
            (BTN_UP, Some(i)) => Some(Id::WifiNet(i.saturating_sub(1))),
            (BTN_UP, None)
                if n > 0 && matches!(self.focus, Some(Id::WifiRescan | Id::SheetClose)) =>
            {
                Some(Id::WifiNet(n - 1))
            }
            (BTN_UP | BTN_DOWN | BTN_A, None) if n > 0 && self.focus.is_none() => {
                if code == BTN_A {
                    None
                } else {
                    Some(Id::WifiNet(0))
                }
            }
            _ => None,
        }?;
        if let Id::WifiNet(i) = to {
            self.wifi.scroll_to(i);
        }
        self.focus = Some(to);
        Some(None)
    }

    /// Touch begins on the list: a drag may follow.
    pub(crate) fn wifi_touch_down(&mut self, x: usize, y: usize) {
        self.wifi.drag = None;
        if self.sheet != Some(Sheet::Wifi) || self.wifi.stage != Stage::List {
            return;
        }
        let (sx, sy) = self.wifi_origin();
        let inside = x >= sx && x < sx + 960 && y >= sy + LIST_TOP && y < sy + LIST_TOP + VIEW;
        if inside {
            self.wifi.drag = Some(Drag {
                y0: y,
                off0: self.wifi.off,
                moved: false,
            });
        }
    }

    /// The finger moves: true once it scrolls (the tap is off).
    pub(crate) fn wifi_touch_move(&mut self, y: usize) -> bool {
        let max = self.wifi.max_off() as i64;
        let Some(d) = &mut self.wifi.drag else {
            return false;
        };
        let dy = y as i64 - d.y0 as i64;
        if !d.moved && dy.abs() > SLOP {
            d.moved = true;
        }
        if d.moved {
            self.wifi.off = (d.off0 as i64 - dy).clamp(0, max) as usize;
        }
        d.moved
    }

    /// The finger lifts: true if it was a drag, not a tap.
    pub(crate) fn wifi_touch_up(&mut self) -> bool {
        self.wifi.drag.take().is_some_and(|d| d.moved)
    }

    fn wifi_origin(&self) -> (usize, usize) {
        ((crate::ui::W - 960) / 2, (crate::ui::H - 592) / 2)
    }

    pub(crate) fn wifi_sheet(&mut self, s: &mut Screen, d: &Data) {
        let (x, y, w, h) = self.frame(s, "", 960, 592);
        match self.wifi.stage {
            Stage::List => self.wifi_list(s, d, (x, y, w, h)),
            Stage::Password => self.wifi_password(s, (x, y, w, h)),
        }
    }

    fn wifi_list(&mut self, s: &mut Screen, d: &Data, (x, y, w, h): (usize, usize, usize, usize)) {
        let p = pal();
        let joined = wifi_connected(d);
        let (lx, lw) = (x + 40, w - 80);
        let (top, bottom) = (y + LIST_TOP, y + LIST_TOP + VIEW);
        let n = self.wifi.nets.len();
        self.wifi.off = self.wifi.off.min(self.wifi.max_off());
        for i in 0..n {
            let ry = (top + i * PITCH) as i64 - self.wifi.off as i64;
            if ry + ROW as i64 <= top as i64 || ry >= bottom as i64 {
                continue;
            }
            let ry = ry.max(0) as usize;
            let net = &self.wifi.nets[i];
            let id = Id::WifiNet(i);
            let cur = joined.is_some_and(|c| c.raw() == net.raw.as_slice());
            let trying = self.wifi.joining.as_deref() == Some(net.ssid.as_str());
            let ok = joinable(net.security);
            let bg = press(p.raised, self.pressed == Some(id));
            s.fill_round(lx, ry, lw - 16, ROW, 8, bg);
            let cy = ry + ROW / 2;
            let ink = if ok { p.text } else { p.faint };
            if net.security != wifi::SEC_OPEN {
                padlock(s, lx + 28, cy, if ok { p.dim } else { p.faint });
            }
            let bars_x = lx + lw - 16 - 24 - 29;
            let (note, nc): (String, u32) = match () {
                _ if cur => ("Connected".into(), p.good),
                _ if trying => ("Joining...".into(), p.warn),
                _ => (
                    format!("ch {}   {}", net.channel, security_text(net.security)),
                    if ok { p.dim } else { p.faint },
                ),
            };
            let tw = font::width(Face::Small, &note);
            let name = fit(Face::Body, &net.ssid, bars_x - 20 - tw - (lx + 60) - 12);
            s.label(lx + 60, cy, &name, Face::Body, ink);
            s.label_r(bars_x - 20, cy, &note, Face::Small, nc);
            signal_bars(
                s,
                bars_x,
                cy + 12,
                bars_for(net.signal as u64),
                if ok { p.good } else { p.faint },
            );
            let (zy, zb) = (ry.max(top), (ry + ROW).min(bottom));
            if zb > zy {
                self.zone(id, lx, zy, lw - 16, zb - zy, 8);
            }
        }
        // The rows the window cut leave the header and the footer.
        s.fill(x + 20, y + 30, w - 40, top - y - 30, p.surface);
        s.fill(x + 20, bottom, w - 40, y + h - 20 - bottom, p.surface);
        if self.wifi.max_off() > 0 {
            let track = VIEW - 8;
            let thumb = (track * VIEW / (VIEW + self.wifi.max_off())).max(32);
            let at = (track - thumb) * self.wifi.off / self.wifi.max_off();
            s.fill_round(x + w - 36, top + 4, 6, track, 3, p.track);
            s.fill_round(x + w - 36, top + 4 + at, 6, thumb, 3, p.dim);
        }
        s.label(x + 40, y + 50, "Wi-Fi networks", Face::TitleB, p.text);
        let status = if self.wifi.scanning {
            ("Scanning...".into(), p.warn)
        } else if let Some(e) = &self.wifi.scan_err {
            (fit(Face::Small, e, 460), p.bad)
        } else if self.wifi.scanned {
            (format!("{} networks", n), p.dim)
        } else {
            (String::new(), p.dim)
        };
        s.label_r(x + w - 40, y + 50, &status.0, Face::Small, status.1);
        if n == 0 {
            let t = match (self.wifi.scanning, self.wifi.scanned) {
                (true, _) | (_, false) => "Looking for networks...",
                _ => "No network found",
            };
            s.label_c(x + w / 2, top + VIEW / 2, t, Face::Body, p.dim);
        }
        let by = y + h - 40 - 60;
        let idle = !self.wifi.scanning && self.wifi.joining.is_none();
        self.button(
            s,
            Id::WifiRescan,
            x + 40,
            by,
            220,
            60,
            "Rescan",
            Tone::Plain,
            idle,
        );
        self.button(
            s,
            Id::SheetClose,
            x + w - 40 - 220,
            by,
            220,
            60,
            "Close",
            Tone::Plain,
            true,
        );
    }

    fn wifi_password(&mut self, s: &mut Screen, (x, y, w, h): (usize, usize, usize, usize)) {
        let p = pal();
        let Some((_, name, sec)) = self.wifi.target.clone() else {
            return;
        };
        let busy = self.wifi.joining.is_some();
        s.label(
            x + 40,
            y + 50,
            &fit(Face::TitleB, &name, 640),
            Face::TitleB,
            p.text,
        );
        s.label_r(x + w - 40, y + 50, security_text(sec), Face::Small, p.dim);

        let (fx, fy, fh) = (x + 40, y + 88, 60);
        let fw = w - 80 - 140;
        s.fill_round(fx, fy, fw, fh, 8, p.raised);
        s.ring_round(fx, fy, fw, fh, 8, 2, p.accent);
        let cy = fy + fh / 2;
        let len = self.wifi.pass.len();
        let end = if len == 0 {
            s.label(fx + 20, cy, "Passphrase", Face::Body, p.faint);
            fx + 20
        } else if self.wifi.show {
            // The end of what is typed, as much as fits.
            let mut from = 0;
            while from < len && font::width(Face::Body, &self.wifi.pass[from..]) > fw - 48 {
                from += 1;
            }
            fx + 20 + s.label(fx + 20, cy, &self.wifi.pass[from..], Face::Body, p.text)
        } else {
            let pitch = (fw - 56).checked_div(len).unwrap_or(18).clamp(6, 20);
            for i in 0..len {
                s.disc(fx + 28 + i * pitch, cy, pitch.clamp(6, 10) / 2 + 1, p.text);
            }
            fx + 28 + len * pitch
        };
        if !busy && (libnx::time::now_ns() / 500_000_000).is_multiple_of(2) {
            s.fill(end.min(fx + fw - 12), cy - 18, 3, 36, p.accent);
        }
        let label = if self.wifi.show { "Hide" } else { "Show" };
        self.chip(
            s,
            Id::WifiShow,
            x + w - 40 - 120,
            fy,
            120,
            fh,
            label,
            self.wifi.show,
            true,
        );

        let msg = if let Some(e) = &self.wifi.error {
            Some((e.clone(), p.bad))
        } else if let Some(j) = &self.wifi.joining {
            Some((format!("Joining {}...", j), p.warn))
        } else {
            Some((format!("{} characters", len), p.dim))
        };
        if let Some((t, c)) = msg {
            s.label(fx, y + 174, &fit(Face::Small, &t, w - 80), Face::Small, c);
        }

        let keys = Keys {
            x: x + 44,
            y: y + 198,
            w: 872,
            kh: 48,
            gx: 8,
            gy: 8,
            layers: true,
            symbols: self.wifi.symbols,
            shift: self.wifi.shift,
            on: !busy,
        };
        self.keyboard(s, &keys);

        let by = y + h - 40 - 60;
        self.button(
            s,
            Id::WifiBack,
            x + 40,
            by,
            220,
            60,
            "Back",
            Tone::Plain,
            !busy,
        );
        let ready = len >= PSK_MIN && !busy;
        let join = if busy { "Joining..." } else { "Join" };
        self.button(
            s,
            Id::WifiJoin,
            x + w - 40 - 220,
            by,
            220,
            60,
            join,
            Tone::Accent,
            ready,
        );
    }
}

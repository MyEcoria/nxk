//! CONTROLLERS: the controllers connected (the rails' Joy-Cons and the
//! Bluetooth ones), pairing, and a Test tab drawing each controller's
//! buttons and sticks live from the events it sends (`input` source
//! word). Also the left stick moving through the menus like the D-pad.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{bt, input, joycon};

use crate::font::Face;
use crate::screen::Screen;
use crate::stats::{Data, Seen};
use crate::theme::pal;
use crate::ui::{Id, Ui, CB, CY, HINT_Y, M, W};
use crate::widgets::{card, Tone};

/// Stick past this share of its travel: a direction; back under the
/// other: released (hysteresis against a stick resting off centre).
const NAV_ON: i64 = input::STICK_MAX * 55 / 100;
const NAV_OFF: i64 = input::STICK_MAX * 35 / 100;
/// A held direction repeats like a key: first after this, then often.
const NAV_FIRST_NS: u64 = 400_000_000;
const NAV_REPEAT_NS: u64 = 130_000_000;
const GAP: usize = 16;

/// One controller's state as its events left it.
pub struct PadView {
    pub src: u64,
    /// `1 << BTN_*` for each button held.
    pub buttons: u32,
    pub left: (i64, i64),
    pub right: (i64, i64),
}

/// A connected controller, for the list and the Test tab.
struct Entry {
    src: u64,
    name: String,
    how: &'static str,
    state: &'static str,
    ok: bool,
    battery: u64,
    left: bool,
    right: bool,
    /// Index in `bt_pads` (Forget), for a Bluetooth one.
    bt: Option<usize>,
}

fn kind_halves(kind: u8) -> (bool, bool) {
    match kind {
        bt::KIND_JOYCON_L => (true, false),
        bt::KIND_JOYCON_R => (false, true),
        _ => (true, true),
    }
}

/// A button cap: a disc lit while held, its label inside.
fn cap(s: &mut Screen, cx: usize, cy: usize, r: usize, label: &str, on: bool) {
    let p = pal();
    if on {
        s.disc(cx, cy, r, p.accent);
    } else {
        s.disc(cx, cy, r, p.raised);
        s.ring(cx, cy, r, 1, p.rule);
    }
    s.label_c(cx, cy, label, Face::Cap, if on { p.bg } else { p.text });
}

/// A shoulder or system button: a pill lit while held.
fn pill(s: &mut Screen, cx: usize, cy: usize, w: usize, label: &str, on: bool) {
    let p = pal();
    let (bg, fg) = if on { (p.accent, p.bg) } else { (p.raised, p.text) };
    s.fill_round(cx - w / 2, cy - 14, w, 28, 14, bg);
    s.label_c(cx, cy, label, Face::Cap, fg);
}

/// A stick: its ring (lit while clicked) and a dot where it points.
fn stick(s: &mut Screen, cx: usize, cy: usize, pos: (i64, i64), clicked: bool) {
    let p = pal();
    let r = 34i64;
    s.disc(cx, cy, r as usize, if clicked { p.accent } else { p.raised });
    s.ring(cx, cy, r as usize, 2, p.rule);
    let reach = r - 10;
    let dx = pos.0 * reach / input::STICK_MAX;
    let dy = -pos.1 * reach / input::STICK_MAX;
    let moved = pos != (0, 0);
    s.disc(
        (cx as i64 + dx) as usize,
        (cy as i64 + dy) as usize,
        9,
        if moved { p.good } else { p.text },
    );
}

impl Ui {
    /// Record a controller's event; true if the Test tab shows it.
    pub fn track(&mut self, kind: u64, code: u64, value: u64, x: u64, y: u64, src: u64) -> bool {
        if src == input::SRC_NONE || !(kind == input::KIND_BUTTON || kind == input::KIND_STICK) {
            return false;
        }
        let k = match self.pads.iter().position(|v| v.src == src) {
            Some(k) => k,
            None => {
                self.pads.push(PadView {
                    src,
                    buttons: 0,
                    left: (0, 0),
                    right: (0, 0),
                });
                self.pads.len() - 1
            }
        };
        let v = &mut self.pads[k];
        if kind == input::KIND_BUTTON && code < 32 {
            if value != 0 {
                v.buttons |= 1 << code;
            } else {
                v.buttons &= !(1 << code);
            }
        } else if kind == input::KIND_STICK {
            let pos = (x as i64, y as i64);
            match code {
                input::STICK_L => v.left = pos,
                input::STICK_R => v.right = pos,
                _ => {}
            }
        }
        self.page == crate::ui::CONTROLLERS
    }

    /// The left stick as a direction: the D-pad code to act on now.
    pub fn stick_nav(&mut self, code: u64, x: u64, y: u64, now: u64) -> Option<u64> {
        if code != input::STICK_L {
            return None;
        }
        let (x, y) = (x as i64, y as i64);
        let mag = x.abs().max(y.abs());
        let dir = if x.abs() > y.abs() {
            if x > 0 {
                input::BTN_RIGHT
            } else {
                input::BTN_LEFT
            }
        } else if y > 0 {
            input::BTN_UP
        } else {
            input::BTN_DOWN
        };
        match self.nav {
            Some(_) if mag < NAV_OFF => {
                self.nav = None;
                None
            }
            Some(d) if d == dir || mag <= NAV_ON => None,
            _ if mag > NAV_ON => {
                self.nav = Some(dir);
                self.nav_at = now + NAV_FIRST_NS;
                Some(dir)
            }
            _ => None,
        }
    }

    /// A held direction due again.
    pub fn stick_repeat(&mut self, now: u64) -> Option<u64> {
        let d = self.nav?;
        if now < self.nav_at {
            return None;
        }
        self.nav_at = now + NAV_REPEAT_NS;
        Some(d)
    }

    pub fn stick_due(&self) -> Option<u64> {
        self.nav.map(|_| self.nav_at)
    }

    /// The Test tab has the controllers' buttons to itself.
    pub fn testing(&self) -> bool {
        self.page == crate::ui::CONTROLLERS && self.ctl_tab == 1 && !self.overlay()
    }

    fn entries(&mut self, d: &Data) -> Vec<Entry> {
        let mut v = Vec::new();
        if let Seen::Value(r) = d.rails {
            for (i, &(st, battery)) in r.iter().enumerate() {
                if st == joycon::ST_ABSENT {
                    continue;
                }
                let left = i == 0;
                v.push(Entry {
                    src: if left {
                        input::SRC_RAIL_L
                    } else {
                        input::SRC_RAIL_R
                    },
                    name: String::from(if left { "Joy-Con (L)" } else { "Joy-Con (R)" }),
                    how: "on the rail",
                    state: if st == joycon::ST_READY {
                        "connected"
                    } else {
                        "attached, not answering"
                    },
                    ok: st == joycon::ST_READY,
                    battery,
                    left,
                    right: !left,
                    bt: None,
                });
            }
        }
        self.bt_pads.clear();
        if let Seen::Value(b) = &d.bt {
            self.bt_pairing = b.state == bt::ST_PAIRING;
            for pad in &b.pads {
                let (left, right) = kind_halves(pad.kind);
                self.bt_pads.push((pad.addr, pad.name.clone()));
                v.push(Entry {
                    src: input::SRC_BT | pad.addr,
                    name: pad.name.clone(),
                    how: "Bluetooth",
                    state: if pad.connected {
                        "connected"
                    } else {
                        "paired, press a button"
                    },
                    ok: pad.connected,
                    battery: if pad.battery == bt::BATTERY_UNKNOWN {
                        joycon::BATTERY_UNKNOWN
                    } else {
                        pad.battery as u64
                    },
                    left,
                    right,
                    bt: Some(self.bt_pads.len() - 1),
                });
            }
        }
        v
    }

    pub(crate) fn controllers(&mut self, s: &mut Screen, d: &Data) {
        let list = self.entries(d);
        // Tracked state of controllers gone is dropped.
        self.pads.retain(|v| list.iter().any(|e| e.ok && e.src == v.src));
        if self.ctl_tab == 0 {
            self.ctl_list(s, d, &list);
            self.page_hints(s);
        } else {
            self.ctl_test(s, &list);
            self.hints(s, 30, W - 54, pal().bg, &[("HOME", "Leave the test")]);
        }
        let tx = W / 2 - 144;
        for (i, t) in ["Controllers", "Test"].iter().enumerate() {
            let on = self.ctl_tab == i;
            self.chip(s, Id::CtlTab(i), tx + i * 148, HINT_Y - 18, 136, 36, t, on, true);
        }
    }

    fn ctl_list(&mut self, s: &mut Screen, d: &Data, list: &[Entry]) {
        let p = pal();
        let bt_up = matches!(&d.bt, Seen::Value(b) if b.state == bt::ST_READY || b.state == bt::ST_PAIRING);
        let hint = if self.bt_pairing {
            "Hold the sync button of the controller to pair."
        } else if bt_up {
            "Joy-Cons on the rails connect by themselves."
        } else {
            "Bluetooth is off: no adapter answered."
        };
        s.label(M, CY + 28, hint, Face::Body, p.dim);
        self.button(
            s,
            Id::BtPair,
            W - M - 300,
            CY,
            300,
            56,
            if self.bt_pairing {
                "Stop Pairing"
            } else {
                "Pair a Controller"
            },
            if self.bt_pairing {
                Tone::Warn
            } else {
                Tone::Accent
            },
            bt_up,
        );
        if list.is_empty() {
            s.label(M, CY + 120, "No controller connected.", Face::Body, p.dim);
            return;
        }
        let row_h = 64;
        for (i, e) in list.iter().take(6).enumerate() {
            let y = CY + 76 + i * (row_h + 8);
            card(s, M, y, W - 2 * M, row_h);
            let cy = y + row_h / 2;
            s.disc(M + 28, cy, 7, if e.ok { p.good } else { p.faint });
            s.label(M + 50, cy, &e.name, Face::Body, p.text);
            s.label(M + 380, cy, e.how, Face::Small, p.dim);
            s.label(
                M + 560,
                cy,
                e.state,
                Face::Small,
                if e.ok { p.good } else { p.dim },
            );
            let battery = if e.battery == joycon::BATTERY_UNKNOWN {
                String::from("-")
            } else {
                format!("battery {}/4", e.battery)
            };
            let xr = W - M - 24;
            let bx = if e.bt.is_some() { xr - 140 } else { xr };
            s.label_r(bx, cy, &battery, Face::Small, p.dim);
            if let Some(k) = e.bt {
                self.chip(s, Id::BtForget(k), xr - 120, cy - 20, 120, 40, "Forget", false, true);
            }
        }
    }

    fn ctl_test(&mut self, s: &mut Screen, list: &[Entry]) {
        let p = pal();
        let live: Vec<&Entry> = list.iter().filter(|e| e.ok).collect();
        if live.is_empty() {
            s.label(M, CY + 40, "No controller connected to test.", Face::Body, p.dim);
            s.label(
                M,
                CY + 80,
                "Attach a Joy-Con to a rail, or pair one in the Controllers tab.",
                Face::Small,
                p.dim,
            );
            return;
        }
        let cols = if live.len() > 1 { 2 } else { 1 };
        let rows = live.len().div_ceil(cols).min(2);
        let w = (W - 2 * M - GAP * (cols - 1)) / cols;
        let h = ((CB - CY) - GAP * (rows - 1)) / rows;
        for (i, e) in live.iter().take(4).enumerate() {
            let (x, y) = (M + (i % cols) * (w + GAP), CY + (i / cols) * (h + GAP));
            let view = self.pads.iter().find(|v| v.src == e.src);
            self.pad_card(s, x, y, w, h, e, view);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pad_card(
        &self,
        s: &mut Screen,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        e: &Entry,
        v: Option<&PadView>,
    ) {
        let p = pal();
        card(s, x, y, w, h);
        s.label(x + 24, y + 30, &e.name, Face::BodyB, p.text);
        s.label_r(x + w - 24, y + 30, e.how, Face::Small, p.dim);
        let held = |b: u64| v.is_some_and(|v| v.buttons & (1 << b) != 0);
        // The drawing (about 200 px tall) centred under the title, the
        // shoulder row just above it.
        let my = y + h / 2 + 18;
        let sy = (my - 96).max(y + 64);
        let halves = e.left as usize + e.right as usize;
        let (lx, rx) = if halves == 2 {
            (x + w / 4, x + 3 * w / 4)
        } else {
            (x + w / 2, x + w / 2)
        };
        if e.left {
            pill(s, lx - 50, sy, 56, "ZL", held(input::BTN_ZL));
            pill(s, lx + 14, sy, 56, "L", held(input::BTN_L));
            pill(s, lx + 80, sy, 44, "-", held(input::BTN_MINUS));
            stick(s, lx - 40, my - 12, v.map_or((0, 0), |v| v.left), held(input::BTN_LSTICK));
            let (dx, dy) = (lx + 52, my + 30);
            cap(s, dx, dy - 26, 15, "^", held(input::BTN_UP));
            cap(s, dx, dy + 26, 15, "v", held(input::BTN_DOWN));
            cap(s, dx - 26, dy, 15, "<", held(input::BTN_LEFT));
            cap(s, dx + 26, dy, 15, ">", held(input::BTN_RIGHT));
            pill(s, lx - 40, my + 62, 44, "[o]", held(input::BTN_CAPTURE));
        }
        if e.right {
            pill(s, rx - 80, sy, 44, "+", held(input::BTN_PLUS));
            pill(s, rx - 14, sy, 56, "R", held(input::BTN_R));
            pill(s, rx + 50, sy, 56, "ZR", held(input::BTN_ZR));
            let (bx, by) = (rx - 40, my - 12);
            cap(s, bx, by - 28, 16, "X", held(input::BTN_X));
            cap(s, bx, by + 28, 16, "B", held(input::BTN_B));
            cap(s, bx - 28, by, 16, "Y", held(input::BTN_Y));
            cap(s, bx + 28, by, 16, "A", held(input::BTN_A));
            stick(s, rx + 48, my + 34, v.map_or((0, 0), |v| v.right), held(input::BTN_RSTICK));
            cap(s, rx - 40, my + 62, 14, "H", held(input::BTN_HOME));
        }
    }
}

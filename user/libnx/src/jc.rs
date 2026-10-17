//! Joy-Con and Pro Controller input reports, the same on the rail
//! (`joycon`) and over Bluetooth (`bluetooth`): buttons and sticks
//! turned into `input` events.

use alloc::vec::Vec;

use nxk_abi::proto::input;

use crate::println;

/// Standard full report (60 Hz Pro Controller, 15 ms Joy-Con) and the
/// reply to a subcommand; both start with buttons, sticks and battery.
pub const REPORT_FULL: u8 = 0x30;
pub const REPORT_SUBCMD_REPLY: u8 = 0x21;

/// Buttons in the report's 24 bits (right half in 0-7, shared in 8-15,
/// left in 16-23), for each side.
pub const MASK_L: u32 = 0xFF_2900;
pub const MASK_R: u32 = 0x00_56FF;
pub const MASK_PRO: u32 = MASK_L | MASK_R;
pub const BUTTONS: [(u32, u64); 18] = [
    (1 << 0, input::BTN_Y),
    (1 << 1, input::BTN_X),
    (1 << 2, input::BTN_B),
    (1 << 3, input::BTN_A),
    (1 << 6, input::BTN_R),
    (1 << 7, input::BTN_ZR),
    (1 << 8, input::BTN_MINUS),
    (1 << 9, input::BTN_PLUS),
    (1 << 10, input::BTN_RSTICK),
    (1 << 11, input::BTN_LSTICK),
    (1 << 12, input::BTN_HOME),
    (1 << 13, input::BTN_CAPTURE),
    (1 << 16, input::BTN_DOWN),
    (1 << 17, input::BTN_UP),
    (1 << 18, input::BTN_RIGHT),
    (1 << 19, input::BTN_LEFT),
    (1 << 22, input::BTN_L),
    (1 << 23, input::BTN_ZL),
];

/// Sticks: 12-bit positions around a centre near 2048. The range of a
/// Joy-Con's stick is about +-1100 from it (not read from the unit's
/// calibration); a dead zone of 170 swallows the rest position's jitter.
const STICK_CENTER: i64 = 2048;
const STICK_RANGE: i64 = 1150;
const STICK_DEAD: i64 = 170;
const STICK_STEP: i64 = 600;
const CENTER_SAMPLES: i64 = 8;
const CENTER_JITTER: i64 = 60;
/// Reports (about 2 s) before the rest position is assumed.
const CENTER_GIVE_UP: u32 = 140;
const TRACE_BUTTONS: u32 = 40;

/// One input report: buttons, sticks, battery (0..8, bit 0: charging).
pub struct Report {
    pub buttons: u32,
    pub left: (i64, i64),
    pub right: (i64, i64),
    pub battery: u8,
}

/// A full report or subcommand reply, report id first.
pub fn parse(p: &[u8]) -> Option<Report> {
    if p.len() < 12 || p[0] != REPORT_FULL && p[0] != REPORT_SUBCMD_REPLY {
        return None;
    }
    let pair = |i: usize| {
        (
            (p[i] as i64) | ((p[i + 1] as i64 & 0xF) << 8),
            (p[i + 1] as i64 >> 4) | ((p[i + 2] as i64) << 4),
        )
    };
    Some(Report {
        buttons: p[3] as u32 | (p[4] as u32) << 8 | (p[5] as u32) << 16,
        left: pair(6),
        right: pair(9),
        battery: p[2] >> 4,
    })
}

fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let (mut x, mut y) = (n, (n + 1) / 2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Position from the centre, with the dead zone taken off the length,
/// as -`STICK_MAX`..=`STICK_MAX`.
fn stick_value(raw: (i64, i64), center: (i64, i64)) -> (i64, i64) {
    let (dx, dy) = (raw.0 - center.0, raw.1 - center.1);
    let m = isqrt(dx * dx + dy * dy);
    if m <= STICK_DEAD {
        return (0, 0);
    }
    let out =
        ((m - STICK_DEAD) * input::STICK_MAX / (STICK_RANGE - STICK_DEAD)).min(input::STICK_MAX);
    (dx * out / m, dy * out / m)
}

struct Stick {
    code: u64,
    center: (i64, i64),
    sum: (i64, i64),
    lo: (i64, i64),
    hi: (i64, i64),
    samples: i64,
    waited: u32,
    sent: (i64, i64),
}

impl Stick {
    const fn new(code: u64) -> Self {
        Stick {
            code,
            center: (STICK_CENTER, STICK_CENTER),
            sum: (0, 0),
            lo: (0, 0),
            hi: (0, 0),
            samples: 0,
            waited: 0,
            sent: (0, 0),
        }
    }

    fn release(&mut self, out: &mut Vec<[u64; 5]>) {
        if self.sent != (0, 0) {
            out.push([input::KIND_STICK, self.code, 0, 0, 0]);
        }
        *self = Stick::new(self.code);
    }

    fn feed(&mut self, raw: (i64, i64), tag: &str, who: &str, out: &mut Vec<[u64; 5]>) {
        if self.samples < CENTER_SAMPLES {
            // The rest position (units differ by hundreds): a run of
            // reports that hardly move, else the middle after a while.
            let (lo, hi) = if self.samples == 0 {
                (raw, raw)
            } else {
                (
                    (self.lo.0.min(raw.0), self.lo.1.min(raw.1)),
                    (self.hi.0.max(raw.0), self.hi.1.max(raw.1)),
                )
            };
            let steady = hi.0 - lo.0 <= CENTER_JITTER && hi.1 - lo.1 <= CENTER_JITTER;
            self.waited += 1;
            if steady {
                (self.lo, self.hi) = (lo, hi);
                self.sum = (self.sum.0 + raw.0, self.sum.1 + raw.1);
                self.samples += 1;
            } else {
                self.samples = 0;
                self.sum = (0, 0);
            }
            if self.samples == CENTER_SAMPLES {
                let c = (self.sum.0 / CENTER_SAMPLES, self.sum.1 / CENTER_SAMPLES);
                self.center = c;
                println!("[{}] {}: stick centre {:?}", tag, who, c);
            } else if self.waited == CENTER_GIVE_UP {
                self.samples = CENTER_SAMPLES;
                println!("[{}] {}: stick never at rest, centre assumed", tag, who);
            }
            return;
        }
        let v = stick_value(raw, self.center);
        let moved = (v.0 - self.sent.0).abs() + (v.1 - self.sent.1).abs() >= STICK_STEP;
        let rest = v == (0, 0) && self.sent != (0, 0);
        if moved || rest {
            self.sent = v;
            out.push([input::KIND_STICK, self.code, 0, v.0 as u64, v.1 as u64]);
        }
    }
}

/// One controller's share of the gamepad: the buttons in `mask`, its
/// sticks, what was last sent for them.
pub struct Pad {
    mask: u32,
    left: Option<Stick>,
    right: Option<Stick>,
    buttons: u32,
    traced: u32,
}

impl Pad {
    pub const fn new(mask: u32, left: bool, right: bool) -> Self {
        Pad {
            mask,
            left: if left {
                Some(Stick::new(input::STICK_L))
            } else {
                None
            },
            right: if right {
                Some(Stick::new(input::STICK_R))
            } else {
                None
            },
            buttons: 0,
            traced: 0,
        }
    }

    /// Left Joy-Con, right Joy-Con, or both halves (Pro Controller).
    pub const fn left() -> Self {
        Pad::new(MASK_L, true, false)
    }
    pub const fn right() -> Self {
        Pad::new(MASK_R, false, true)
    }
    pub const fn pro() -> Self {
        Pad::new(MASK_PRO, true, true)
    }

    pub fn buttons(&self) -> u32 {
        self.buttons
    }

    /// Button changes are logged again (a new attach).
    pub fn retrace(&mut self) {
        self.traced = 0;
    }

    /// Back to nothing held and centred sticks (centres found again).
    pub fn release(&mut self, out: &mut Vec<[u64; 5]>) {
        for &(_, code) in BUTTONS.iter().filter(|b| b.0 & self.buttons != 0) {
            out.push([input::KIND_BUTTON, code, 0, 0, 0]);
        }
        self.buttons = 0;
        for s in [&mut self.left, &mut self.right].into_iter().flatten() {
            s.release(out);
        }
    }

    /// Events for what changed since the last report; `tag` and `who`
    /// name the controller in the log.
    pub fn apply(&mut self, r: &Report, tag: &str, who: &str, out: &mut Vec<[u64; 5]>) {
        let now = r.buttons & self.mask;
        let changed = now ^ self.buttons;
        for &(bit, code) in BUTTONS.iter().filter(|b| b.0 & changed != 0) {
            out.push([input::KIND_BUTTON, code, (now & bit != 0) as u64, 0, 0]);
        }
        if changed != 0 && self.traced < TRACE_BUTTONS {
            self.traced += 1;
            println!(
                "[{}] {}: buttons {:#08x} (was {:#08x})",
                tag, who, now, self.buttons
            );
        }
        self.buttons = now;
        if let Some(s) = self.left.as_mut() {
            s.feed(r.left, tag, who, out);
        }
        if let Some(s) = self.right.as_mut() {
            s.feed(r.right, tag, who, out);
        }
    }
}

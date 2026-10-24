//! The full-screen user flows: the first-start setup (splash, name, look,
//! theme, PIN, welcome), adding a user, editing one, and the PIN lock.
//! Each step slides in; the busy parts (stars, rings, confetti) are small
//! so a frame changes few pixels of the uncached image.

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::input;
use libnx::time;

use crate::avatar::{avatar, color, ease_back, ease_out, fade, padlock, prog, sin};
use crate::font::{self, Face};
use crate::keyboard::Keys;
use crate::screen::{logo_size, Screen};
use crate::theme::{self, pal, DARK, LIGHT};
use crate::ui::{Action, Id, Ui, H, M, W};
use crate::users::{self, Profile, COLORS, GLYPHS, NAME_MAX, PIN_LEN};
use crate::widgets::{fit, Tone};

/// A step slides in over this long.
const SLIDE_NS: u64 = 380_000_000;
/// The last digit stays on screen this long before the PIN is checked.
const PIN_HOLD_NS: u64 = 250_000_000;
const BTN_Y: usize = 536;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Step {
    Splash,
    Name,
    Look,
    Theme,
    PinSet,
    PinConfirm,
    Done,
    Verify,
}

/// What the flow ends in.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Purpose {
    /// The first user of the console.
    Setup,
    Create,
    Rename(usize),
    Look(usize),
    Pin(usize),
    /// The PIN of user `n` lets them play.
    Login(usize),
    /// The PIN of user `n` opens their page.
    Unlock(usize),
}

pub(crate) struct Flow {
    pub purpose: Purpose,
    pub step: Step,
    /// The step sliding out, and whether the way is back.
    from: Option<Step>,
    back: bool,
    since: u64,
    name: String,
    color: u8,
    glyph: u8,
    light: bool,
    shift: bool,
    pin: String,
    first: String,
    set_pin: Option<String>,
    /// When the last digit was typed, or the PIN was refused (0: no).
    full_at: u64,
    bad_at: u64,
    /// When the look last changed (the picture pops).
    pop: u64,
    msg: &'static str,
    cancelable: bool,
}

fn order(p: Purpose) -> &'static [Step] {
    use Step::*;
    match p {
        Purpose::Setup => &[Splash, Name, Look, Theme, PinSet, Done],
        Purpose::Create => &[Name, Look, Theme, PinSet, Done],
        Purpose::Rename(_) => &[Name],
        Purpose::Look(_) => &[Look],
        Purpose::Pin(_) => &[PinSet],
        Purpose::Login(_) | Purpose::Unlock(_) => &[Verify],
    }
}

fn position(p: Purpose, step: Step) -> Option<usize> {
    let step = if step == Step::PinConfirm {
        Step::PinSet
    } else {
        step
    };
    order(p).iter().position(|&s| s == step)
}

fn next_step(p: Purpose, step: Step) -> Option<Step> {
    order(p).get(position(p, step)? + 1).copied()
}

fn prev_step(p: Purpose, step: Step) -> Option<Step> {
    order(p).get(position(p, step)?.checked_sub(1)?).copied()
}

fn now() -> u64 {
    time::now_ns()
}

impl Ui {
    fn new_flow(&self, purpose: Purpose, step: Step) -> Flow {
        let n = self.users.list.len();
        Flow {
            purpose,
            step,
            from: None,
            back: false,
            since: now(),
            name: String::new(),
            color: (n % COLORS.len()) as u8,
            glyph: (n * 3 % GLYPHS) as u8,
            light: theme::is_light(),
            shift: true,
            pin: String::new(),
            first: String::new(),
            set_pin: None,
            full_at: 0,
            bad_at: 0,
            pop: 0,
            msg: "",
            cancelable: true,
        }
    }

    /// The first start: nobody has a profile yet.
    pub fn start_setup(&mut self) {
        let mut f = self.new_flow(Purpose::Setup, Step::Splash);
        f.cancelable = false;
        self.begin(f);
    }

    pub(crate) fn start_create(&mut self) {
        let f = self.new_flow(Purpose::Create, Step::Name);
        self.begin(f);
    }

    pub(crate) fn start_rename(&mut self, i: usize) {
        let Some(p) = self.users.list.get(i) else {
            return;
        };
        let mut f = self.new_flow(Purpose::Rename(i), Step::Name);
        f.name = p.name.clone();
        f.color = p.color;
        f.glyph = p.glyph;
        f.shift = false;
        self.begin(f);
    }

    pub(crate) fn start_look(&mut self, i: usize) {
        let Some(p) = self.users.list.get(i) else {
            return;
        };
        let mut f = self.new_flow(Purpose::Look(i), Step::Look);
        f.name = p.name.clone();
        f.color = p.color;
        f.glyph = p.glyph;
        self.begin(f);
    }

    pub(crate) fn start_pin(&mut self, i: usize) {
        let f = self.new_flow(Purpose::Pin(i), Step::PinSet);
        self.begin(f);
    }

    /// The PIN of user `i` is asked: to play (`login`) or to open their page.
    pub(crate) fn start_verify(&mut self, i: usize, login: bool) {
        let p = if login {
            Purpose::Login(i)
        } else {
            Purpose::Unlock(i)
        };
        let mut f = self.new_flow(p, Step::Verify);
        f.cancelable = self.users.current.is_some();
        self.begin(f);
    }

    fn begin(&mut self, f: Flow) {
        self.flow = Some(f);
        self.focus = None;
        self.quick = false;
        self.sheet = None;
    }

    fn go_step(&mut self, step: Step, back: bool) {
        if let Some(f) = self.flow.as_mut() {
            f.from = Some(f.step);
            f.back = back;
            f.step = step;
            f.since = now();
            f.msg = "";
            f.pin.clear();
            f.full_at = 0;
        }
        self.focus = None;
    }

    /// The user's theme is back after the flow tried others.
    fn restore_theme(&mut self) {
        let light = self.users.active().map_or(self.cfg.light, |p| p.light);
        theme::set(light);
    }

    fn cancel(&mut self) {
        self.flow = None;
        self.focus = None;
        self.restore_theme();
    }

    fn flow_next(&mut self) -> Option<Action> {
        let f = self.flow.as_ref()?;
        let (purpose, step) = (f.purpose, f.step);
        if step == Step::Name && users::clean(&f.name).is_empty() {
            return None;
        }
        match next_step(purpose, step) {
            Some(n) => {
                self.go_step(n, false);
                None
            }
            None => self.finish(),
        }
    }

    fn flow_back(&mut self) -> Option<Action> {
        let f = self.flow.as_ref()?;
        let (purpose, step, cancelable) = (f.purpose, f.step, f.cancelable);
        if step == Step::PinConfirm {
            self.go_step(Step::PinSet, true);
        } else if let Some(p) = prev_step(purpose, step) {
            self.go_step(p, true);
        } else if cancelable {
            self.cancel();
        }
        None
    }

    /// Ends the flow: what it gathered goes to the users.
    fn finish(&mut self) -> Option<Action> {
        let f = self.flow.take()?;
        self.focus = None;
        match f.purpose {
            Purpose::Setup | Purpose::Create => {
                let id = self.users.next_id();
                let mut p = Profile::new(id, users::clean(&f.name), f.color, f.glyph, f.light);
                p.pin = f.set_pin.as_deref().map(|pin| users::pin_hash(&p.id, pin));
                self.users.list.push(p);
                let i = self.users.list.len() - 1;
                self.usel = i;
                self.saves_req = true;
                let first = f.purpose == Purpose::Setup;
                if !first {
                    self.restore_theme();
                }
                Some(Action::UserSave(i, first))
            }
            Purpose::Rename(i) => {
                self.users.list.get_mut(i)?.name = users::clean(&f.name);
                Some(Action::UserSave(i, false))
            }
            Purpose::Look(i) => {
                let p = self.users.list.get_mut(i)?;
                p.color = f.color;
                p.glyph = f.glyph;
                Some(Action::UserSave(i, false))
            }
            Purpose::Pin(i) => {
                let p = self.users.list.get_mut(i)?;
                p.pin = f.set_pin.as_deref().map(|pin| users::pin_hash(&p.id, pin));
                Some(Action::UserSave(i, false))
            }
            Purpose::Login(i) => Some(Action::UserActivate(i)),
            Purpose::Unlock(i) => {
                self.users.unlock(i);
                None
            }
        }
    }

    fn default_focus(&self) -> Id {
        match self.flow.as_ref().map(|f| (f.step, f.glyph, f.light)) {
            Some((Step::Name, _, _)) => Id::Key(b'q'),
            Some((Step::Look, g, _)) => Id::Glyph(g as usize),
            Some((Step::Theme, _, l)) => Id::Theme(l),
            Some((Step::PinSet | Step::PinConfirm | Step::Verify, _, _)) => Id::Pin(5),
            _ => Id::FlowNext,
        }
    }

    fn sliding(&self) -> bool {
        self.flow
            .as_ref()
            .is_some_and(|f| f.from.is_some() && now().saturating_sub(f.since) < SLIDE_NS)
    }

    /// A tap or A on `id` while a flow is open; `None`: not the flow's.
    pub(crate) fn flow_tap(&mut self, id: Id) -> Option<Option<Action>> {
        if self.sliding() {
            return Some(None);
        }
        let n = now();
        let f = self.flow.as_mut()?;
        match id {
            Id::Key(c) if f.step == Step::Name => {
                if f.name.chars().count() < NAME_MAX {
                    f.name
                        .push(if f.shift { c.to_ascii_uppercase() } else { c } as char);
                    f.shift = false;
                }
            }
            Id::KeySpace if f.step == Step::Name => {
                if !f.name.is_empty() && !f.name.ends_with(' ') && f.name.chars().count() < NAME_MAX
                {
                    f.name.push(' ');
                }
            }
            Id::KeyBack => {
                f.name.pop();
                f.shift = f.name.is_empty();
            }
            Id::KeyShift => f.shift = !f.shift,
            Id::Glyph(g) => {
                f.glyph = (g % GLYPHS) as u8;
                f.pop = n;
            }
            Id::Color(c) => {
                f.color = (c % COLORS.len()) as u8;
                f.pop = n;
            }
            Id::Theme(l) => {
                f.light = l;
                theme::set(l);
            }
            Id::Pin(d) => {
                if f.pin.len() < PIN_LEN && f.full_at == 0 {
                    f.pin.push((b'0' + d as u8) as char);
                    if f.pin.len() == PIN_LEN {
                        f.full_at = n;
                    }
                }
            }
            Id::PinBack => {
                if f.full_at == 0 {
                    f.pin.pop();
                }
            }
            Id::FlowNext => return Some(self.flow_next()),
            Id::FlowBack => return Some(self.flow_back()),
            // "No PIN": the profile stays open.
            Id::FlowSkip => {
                f.set_pin = None;
                return Some(self.after_pin());
            }
            Id::LockOther => {
                if let Purpose::Login(i) = f.purpose {
                    let n = self.users.list.len().max(1);
                    f.purpose = Purpose::Login((i + 1) % n);
                    f.pin.clear();
                    f.msg = "";
                }
            }
            _ => return None,
        }
        Some(None)
    }

    /// The step after the PIN's.
    fn after_pin(&mut self) -> Option<Action> {
        let purpose = self.flow.as_ref()?.purpose;
        match next_step(purpose, Step::PinSet) {
            Some(n) => {
                self.go_step(n, false);
                None
            }
            None => self.finish(),
        }
    }

    /// The gamepad while a flow is open.
    pub(crate) fn flow_pad(&mut self, code: u64) -> Option<Action> {
        use input::*;
        let step = self.flow.as_ref()?.step;
        match code {
            BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT => {
                if self.focus.is_none() {
                    self.focus = Some(self.default_focus());
                } else {
                    self.step_focus(code);
                }
            }
            BTN_A => match self.focus {
                Some(id) if self.zones.iter().any(|z| z.id == id) => return self.tap(id),
                _ if matches!(step, Step::Splash | Step::Done) => return self.tap(Id::FlowNext),
                _ => self.focus = Some(self.default_focus()),
            },
            BTN_B => return self.tap(Id::FlowBack),
            BTN_X => {
                let id = if step == Step::Name {
                    Id::KeyBack
                } else {
                    Id::PinBack
                };
                return self.tap(id);
            }
            BTN_Y if step == Step::Name => return self.tap(Id::KeyShift),
            BTN_PLUS if !matches!(step, Step::PinSet | Step::PinConfirm | Step::Verify) => {
                return self.tap(Id::FlowNext)
            }
            _ => {}
        }
        None
    }

    /// A finished PIN is checked once its last digit has been seen.
    pub fn tick(&mut self, n: u64) -> Option<Action> {
        let f = self.flow.as_ref()?;
        if f.full_at == 0 || n < f.full_at + PIN_HOLD_NS || f.pin.len() < PIN_LEN {
            return None;
        }
        let (step, purpose, pin, first) = (f.step, f.purpose, f.pin.clone(), f.first.clone());
        match step {
            Step::PinSet => {
                self.go_step(Step::PinConfirm, false);
                if let Some(f) = self.flow.as_mut() {
                    f.first = pin;
                }
                None
            }
            Step::PinConfirm if pin == first => {
                if let Some(f) = self.flow.as_mut() {
                    f.set_pin = Some(pin);
                    f.full_at = 0;
                }
                self.after_pin()
            }
            Step::Verify => {
                let ok = match purpose {
                    Purpose::Login(i) | Purpose::Unlock(i) => {
                        self.users.list.get(i).is_some_and(|p| p.check(&pin))
                    }
                    _ => false,
                };
                if ok {
                    return self.finish();
                }
                self.refuse("Wrong PIN.", n);
                None
            }
            _ => {
                // The two PINs differ: start over.
                self.refuse("The PINs do not match. Try again.", n);
                if let Some(f) = self.flow.as_mut() {
                    f.step = Step::PinSet;
                    f.from = None;
                    f.first.clear();
                }
                None
            }
        }
    }

    fn refuse(&mut self, msg: &'static str, n: u64) {
        if let Some(f) = self.flow.as_mut() {
            f.msg = msg;
            f.bad_at = n;
            f.pin.clear();
            f.full_at = 0;
        }
    }

    /// The flow's frame: the step, or two steps sliding past each other.
    pub(crate) fn flow_screen(&mut self, s: &mut Screen, n: u64) {
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (step, from, back) = (f.step, f.from, f.back);
        let t = n.saturating_sub(f.since);
        let slide = from.filter(|_| t < SLIDE_NS);
        self.sliding = slide.is_some();
        match slide {
            Some(old) => {
                let off = (W as i64 * ease_out(prog(t, 0, SLIDE_NS / 1_000_000)) / 1000) as i32;
                let dir = if back { -1 } else { 1 };
                s.dx = -dir * off;
                self.draw_step(s, old, u64::MAX / 4, n);
                s.dx = dir * (W as i32 - off);
                self.draw_step(s, step, t, n);
                s.dx = 0;
                self.zones.clear();
            }
            None => self.draw_step(s, step, t, n),
        }
        self.flow_hints(s, step);
    }

    fn flow_hints(&self, s: &mut Screen, step: Step) {
        let p = pal();
        let back = self.flow.as_ref().is_some_and(|f| {
            prev_step(f.purpose, f.step).is_some() || f.step == Step::PinConfirm || f.cancelable
        });
        let mut items: Vec<(&str, &str)> = Vec::new();
        match step {
            Step::Name => items.extend([("Y", "Shift"), ("X", "Delete")]),
            Step::PinSet | Step::PinConfirm | Step::Verify => items.push(("X", "Delete")),
            _ => {}
        }
        if back && step != Step::Splash {
            items.push(("B", "Back"));
        }
        items.push(("A", if step == Step::Splash { "Begin" } else { "OK" }));
        self.hints(s, 30, W - 54, p.bg, &items);
    }

    fn draw_step(&mut self, s: &mut Screen, step: Step, t: u64, n: u64) {
        match step {
            Step::Splash => self.step_splash(s, t),
            Step::Name => self.step_name(s, t, n),
            Step::Look => self.step_look(s, t),
            Step::Theme => self.step_theme(s, t),
            Step::PinSet | Step::PinConfirm | Step::Verify => self.step_pin(s, step, t, n),
            Step::Done => self.step_done(s, t),
        }
    }

    fn flow_title(&self, s: &mut Screen, title: &str, sub: &str, t: u64) {
        let p = pal();
        let e = ease_out(prog(t, 0, 350));
        let dy = ((1000 - e) * 20 / 1000) as usize;
        s.label_c(W / 2, 70 + dy, title, Face::Head, fade(p.text, p.bg, e));
        s.label_c(W / 2, 118 + dy, sub, Face::Small, fade(p.dim, p.bg, e));
    }

    /// Back and forward buttons under a step, and the progress dots.
    fn flow_buttons(&mut self, s: &mut Screen, next: Option<(&str, bool)>, t: u64) {
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (purpose, step) = (f.purpose, f.step);
        let can_back =
            prev_step(purpose, step).is_some() || step == Step::PinConfirm || f.cancelable;
        if can_back {
            let label = if prev_step(purpose, step).is_some() || step == Step::PinConfirm {
                "Back"
            } else {
                "Cancel"
            };
            self.button(s, Id::FlowBack, M, BTN_Y, 200, 56, label, Tone::Plain, true);
        }
        if let Some((label, on)) = next {
            self.button(
                s,
                Id::FlowNext,
                W - M - 220,
                BTN_Y,
                220,
                56,
                label,
                Tone::Accent,
                on,
            );
        }
        // One dot per step the user fills in.
        let steps: &[Step] = match purpose {
            Purpose::Setup | Purpose::Create => {
                &[Step::Name, Step::Look, Step::Theme, Step::PinSet]
            }
            _ => &[],
        };
        let cur = steps
            .iter()
            .position(|&x| x == step || (x == Step::PinSet && step == Step::PinConfirm));
        let p = pal();
        let total = steps.len() * 20 + 20;
        let mut x = (W - total) / 2;
        for (k, _) in steps.iter().enumerate() {
            if Some(k) == cur {
                let e = ease_out(prog(t, 0, 300));
                let w = 8 + (20 * e / 1000) as usize;
                s.fill_round(x, 614, w, 8, 4, p.accent);
                x += w + 12;
            } else {
                s.fill_round(x, 614, 8, 8, 4, p.rule);
                x += 20;
            }
        }
    }

    fn step_splash(&mut self, s: &mut Screen, t: u64) {
        let p = pal();
        stars(s, t, p.bg, p.accent);
        let (lw, lh) = logo_size();
        let sc = 5;
        let (w, h) = (lw * sc, lh * sc);
        let e = ease_back(prog(t, 100, 900));
        let top = (150 - (1000 - e) * 200 / 1000).max(0) as usize;
        let (cx, cy) = (W / 2, 150 + h / 2);
        // Rings leave the logo once it has landed.
        let ms = (t / 1_000_000) as i64;
        if ms > 900 {
            for k in 0..2 {
                let ph = (ms + k * 900) % 1800;
                let e2 = ph * 1000 / 1800;
                let r = 95 + (70 * ease_out(e2) / 1000) as usize;
                s.ring(cx, cy, r, 3, fade(p.accent, p.bg, (1000 - e2) / 2));
            }
        }
        s.logo(cx - w / 2, top, sc);
        let word = "Welcome";
        let shown = ((ms - 1100).max(0) / 80) as usize;
        let tw = font::width(Face::Head, word);
        let part: String = word.chars().take(shown).collect();
        s.label(cx - tw / 2, 400, &part, Face::Head, p.text);
        let e = prog(t, 2000, 500);
        s.label_c(
            cx,
            456,
            "Let's get your console ready.",
            Face::Body,
            fade(p.dim, p.bg, e),
        );
        if ms > 2500 {
            let e = ease_out(prog(t, 2500, 400));
            let y = 520 + ((1000 - e) * 40 / 1000) as usize;
            self.button(
                s,
                Id::FlowNext,
                cx - 150,
                y,
                300,
                60,
                "Get started",
                Tone::Accent,
                true,
            );
        }
    }

    fn step_name(&mut self, s: &mut Screen, t: u64, n: u64) {
        let p = pal();
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (name, col, glyph, shift) = (f.name.clone(), f.color, f.glyph, f.shift);
        let title = if matches!(f.purpose, Purpose::Rename(_)) {
            "New name"
        } else {
            "What's your name?"
        };
        self.flow_title(s, title, "Letters and digits, up to 12.", t);
        let (fx, fy, fw, fh) = (W / 2 - 320, 142, 640, 68);
        s.fill_round(fx, fy, fw, fh, 12, p.surface);
        s.ring_round(fx, fy, fw, fh, 12, 2, p.accent);
        avatar(s, fx + 44, fy + fh / 2, 26, col, glyph);
        let (tx, cy) = (fx + 88, fy + fh / 2);
        if name.is_empty() {
            s.label(tx, cy, "Your name", Face::Body, p.faint);
        }
        let tw = s.label(tx, cy, &name, Face::Head, p.text);
        if (n / 500_000_000).is_multiple_of(2) {
            s.fill(tx + tw + 4, cy - 22, 3, 44, p.accent);
        }

        let keys = Keys {
            x: (W - 930) / 2,
            y: 236,
            w: 930,
            kh: 46,
            gx: 10,
            gy: 8,
            layers: false,
            symbols: false,
            shift,
            on: true,
        };
        self.keyboard(s, &keys);
        let last = matches!(
            self.flow.as_ref().map(|f| f.purpose),
            Some(Purpose::Rename(_))
        );
        let go = users::clean(&name).is_empty();
        self.flow_buttons(s, Some((if last { "Save" } else { "Next" }, !go)), t);
    }

    fn step_look(&mut self, s: &mut Screen, t: u64) {
        let p = pal();
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (name, col, glyph, pop) = (f.name.clone(), f.color, f.glyph, f.pop);
        let single = matches!(f.purpose, Purpose::Look(_));
        self.flow_title(s, "Pick your look", "A face and a color.", t);
        // The picture pops when it changes.
        let e = if pop == 0 {
            1000
        } else {
            ease_back(prog(now().saturating_sub(pop), 0, 450))
        };
        let r = 70 + (30 * e / 1000).max(0) as usize;
        let (pcx, pcy) = (300, 330);
        avatar(s, pcx, pcy, r, col, glyph);
        let shown = if name.is_empty() { "You" } else { &name };
        s.label_c(
            pcx,
            pcy + 140,
            &fit(Face::TitleB, shown, 340),
            Face::TitleB,
            p.text,
        );

        for i in 0..GLYPHS {
            let (cx, cy) = (640 + (i % 4) * 118, 240 + (i / 4) * 124);
            let e = ease_back(prog(t, 120 + 45 * i as u64, 380));
            let rr = (44 * e / 1000).max(0) as usize;
            if i == glyph as usize {
                s.disc(cx, cy, 50, p.accent);
            }
            s.disc(cx, cy, 44.min(rr + 2), p.surface);
            avatar(s, cx, cy, rr.min(44), col, i as u8);
            self.zone(Id::Glyph(i), cx - 44, cy - 44, 88, 88, 44);
        }
        for k in 0..COLORS.len() {
            let (cx, cy) = (600 + k * 62, 470);
            let e = ease_back(prog(t, 520 + 35 * k as u64, 320));
            let rr = (22 * e / 1000).max(0) as usize;
            if k == col as usize {
                s.ring(cx, cy, 29, 3, p.text);
            }
            if rr > 0 {
                s.disc(cx, cy, rr, color(k as u8));
            }
            self.zone(Id::Color(k), cx - 26, cy - 26, 52, 52, 26);
        }
        self.flow_buttons(s, Some((if single { "Save" } else { "Next" }, true)), t);
    }

    fn step_theme(&mut self, s: &mut Screen, t: u64) {
        let p = pal();
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let light = f.light;
        self.flow_title(s, "Choose a look", "You can change it any time.", t);
        for (k, (pal_k, name)) in [(&DARK, "Basic Black"), (&LIGHT, "Basic White")]
            .into_iter()
            .enumerate()
        {
            let on = light == (k == 1);
            let e = ease_out(prog(t, 100 + 150 * k as u64, 450));
            let (x, y, w, h) = (
                W / 2 - 380 + k * 400,
                190 + ((1000 - e) * 40 / 1000) as usize,
                360,
                250,
            );
            if on {
                s.ring_round(x - 8, y - 8, w + 16, h + 16, 20, 4, p.focus);
            }
            s.shadow(x, y, w, h, 14, crate::theme::BLACK);
            s.fill_round(x, y, w, h, 14, pal_k.bg);
            // A miniature home screen of the theme.
            for i in 0..3 {
                s.fill_round(x + 24 + i * 108, y + 28, 96, 96, 10, pal_k.raised);
                s.fill_round(
                    x + 34 + i * 108,
                    y + 38,
                    76,
                    76,
                    8,
                    color(((k * 3 + i) * 2 + 1) as u8),
                );
            }
            s.fill_round(x + 60, y + 160, w - 120, 44, 22, pal_k.surface);
            s.disc(x + w / 2, y + 182, 14, pal_k.accent);
            s.label_c(
                x + w / 2,
                y + h + 34,
                name,
                Face::Body,
                if on { p.accent } else { p.text },
            );
            self.zone(Id::Theme(k == 1), x, y, w, h, 14);
        }
        self.flow_buttons(s, Some(("Next", true)), t);
    }

    fn step_pin(&mut self, s: &mut Screen, step: Step, t: u64, n: u64) {
        let p = pal();
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (purpose, pin, msg, bad_at) = (f.purpose, f.pin.len(), f.msg, f.bad_at);
        let target = match purpose {
            Purpose::Login(i) | Purpose::Unlock(i) => self.users.list.get(i).cloned(),
            _ => None,
        };
        let cx = if step == Step::Verify { 830 } else { W / 2 };
        match (step, &target) {
            (Step::Verify, Some(u)) => {
                let t1 = alloc::format!("Hello, {}", u.name);
                self.flow_title(s, &fit(Face::Head, &t1, 900), "Enter your PIN.", t);
                let e = ease_back(prog(t, 100, 500));
                avatar(
                    s,
                    330,
                    300,
                    (90 * e / 1000).max(1) as usize,
                    u.color,
                    u.glyph,
                );
                padlock(s, 330, 300 + 140, p.dim);
            }
            (Step::PinConfirm, _) => {
                self.flow_title(s, "Type it again", "To be sure it is right.", t)
            }
            _ => self.flow_title(
                s,
                "Add a PIN?",
                "Optional. Four digits keep other players out of your profile.",
                t,
            ),
        }
        // The dots shake when the PIN was refused.
        let since = n.saturating_sub(bad_at);
        let shake = if bad_at != 0 && since < 450_000_000 {
            let e = 1000 - prog(since, 0, 450);
            (sin(since as i64 / 1_000_000 * 5) * e / 1000 * 18 / 1000) as i32
        } else {
            0
        };
        s.dx = shake;
        for k in 0..PIN_LEN {
            let x = cx - 84 + k * 56;
            if k < pin {
                s.disc(x, 176, 14, p.accent);
            } else {
                s.ring(x, 176, 14, 3, p.rule);
            }
        }
        s.dx = 0;
        if !msg.is_empty() {
            s.label_c(cx, 214, msg, Face::Small, p.bad);
        }
        let (kw, kh, gx, gy) = (112usize, 58usize, 14usize, 12usize);
        let x0 = cx - (3 * kw + 2 * gx) / 2;
        for i in 0..10usize {
            let digit = (i + 1) % 10;
            let (col, row) = if digit == 0 { (1, 3) } else { (i % 3, i / 3) };
            let l = [b'0' + digit as u8];
            let l = core::str::from_utf8(&l).unwrap_or("?");
            self.button(
                s,
                Id::Pin(digit),
                x0 + col * (kw + gx),
                236 + row * (kh + gy),
                kw,
                kh,
                l,
                Tone::Plain,
                true,
            );
        }
        self.button(
            s,
            Id::PinBack,
            x0,
            236 + 3 * (kh + gy),
            kw,
            kh,
            "Delete",
            Tone::Plain,
            true,
        );
        match step {
            Step::PinSet => {
                let label = if matches!(purpose, Purpose::Pin(_)) {
                    "Remove PIN"
                } else {
                    "No PIN"
                };
                self.flow_buttons(s, None, t);
                self.button(
                    s,
                    Id::FlowSkip,
                    W - M - 220,
                    BTN_Y,
                    220,
                    56,
                    label,
                    Tone::Plain,
                    true,
                );
            }
            Step::Verify => {
                self.flow_buttons(s, None, t);
                let other = matches!(purpose, Purpose::Login(_)) && self.users.list.len() > 1;
                if other {
                    self.button(
                        s,
                        Id::LockOther,
                        W - M - 220,
                        BTN_Y,
                        220,
                        56,
                        "Other user",
                        Tone::Plain,
                        true,
                    );
                }
            }
            _ => self.flow_buttons(s, None, t),
        }
    }

    fn step_done(&mut self, s: &mut Screen, t: u64) {
        let p = pal();
        let Some(f) = self.flow.as_ref() else {
            return;
        };
        let (name, col, glyph, first) = (
            f.name.clone(),
            f.color,
            f.glyph,
            f.purpose == Purpose::Setup,
        );
        confetti(s, t);
        let e = ease_back(prog(t, 150, 700));
        avatar(s, W / 2, 230, (110 * e / 1000).max(1) as usize, col, glyph);
        let line = alloc::format!("Welcome, {}!", users::clean(&name));
        let e = prog(t, 500, 400);
        s.label_c(
            W / 2,
            390,
            &fit(Face::Head, &line, 1000),
            Face::Head,
            fade(p.text, p.bg, e),
        );
        let sub = if first {
            "Everything is ready. Have fun."
        } else {
            "Switch to this profile from the Users page."
        };
        s.label_c(
            W / 2,
            440,
            sub,
            Face::Body,
            fade(p.dim, p.bg, prog(t, 800, 400)),
        );
        let e = ease_out(prog(t, 1000, 400));
        let y = 500 + ((1000 - e) * 40 / 1000) as usize;
        self.button(
            s,
            Id::FlowNext,
            W / 2 - 130,
            y,
            260,
            60,
            if first { "Start" } else { "Done" },
            Tone::Accent,
            true,
        );
    }
}

/// A sky of twinkling dots behind the splash.
fn stars(s: &mut Screen, t: u64, bg: u32, c: u32) {
    let ms = (t / 1_000_000) as i64;
    let grow = prog(t, 0, 800);
    for i in 0..36i64 {
        let x = ((i * 977 + 131) % 1240 + 20) as usize;
        let y = ((i * 613 + 57) % 640 + 20) as usize;
        let size = 2 + (i % 3) as usize;
        let a = (550 + sin(ms / 7 + i * 97) * 450 / 1000) * grow / 1000;
        s.fill_round(x, y, size, size, size / 2, fade(c, bg, a / 2));
    }
}

/// Paper falling from the top, one wave after the other.
fn confetti(s: &mut Screen, t: u64) {
    let ms = (t / 1_000_000) as i64;
    for i in 0..36i64 {
        let speed = 120 + (i * 37) % 160;
        let fallen = ms * speed / 1000 - (i % 9) * 60;
        if fallen < 0 {
            continue;
        }
        let y = fallen % (H as i64 + 40) - 20;
        let x = (i * 131 + 40) % 1240 + (sin(ms / 8 + i * 40) * 20 / 1000);
        if y < 0 || x < 0 {
            continue;
        }
        s.fill_round(x as usize, y as usize, 10, 6, 2, color((i % 8) as u8));
    }
}

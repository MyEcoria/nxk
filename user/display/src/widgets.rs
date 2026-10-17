//! Shapes the pages share: cards, bars, switches, sliders, badges and
//! the few icons, all drawn from rounded rectangles, circles and lines.

use alloc::string::String;

use crate::font::{self, Face};
use crate::screen::{mix, Screen};
use crate::theme::pal;

/// What a button means: its color when it can be pressed.
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Plain,
    Accent,
    Warn,
    Bad,
}

pub fn tone_color(t: Tone) -> u32 {
    let p = pal();
    match t {
        Tone::Plain => p.text,
        Tone::Accent => p.accent,
        Tone::Warn => p.warn,
        Tone::Bad => p.bad,
    }
}

/// Top of a line of `f` centered in a box of height `h` from `top`.
pub fn vc(f: Face, top: usize, h: usize) -> usize {
    top + h.saturating_sub(font::line_height(f)) / 2
}

/// `s`, cut with dots to fit in `w` pixels.
pub fn fit(f: Face, s: &str, w: usize) -> String {
    if font::width(f, s) <= w {
        return s.into();
    }
    let mut t: String = s.into();
    while !t.is_empty() && font::width(f, &t) + font::width(f, "...") > w {
        t.pop();
    }
    t.push_str("...");
    t
}

/// A card: surface with a hairline, rounded.
pub fn card(s: &mut Screen, x: usize, y: usize, w: usize, h: usize) {
    let p = pal();
    s.card_round(x, y, w, h, 10, 1, p.line, p.surface);
}

/// Card with its title, small and dim, at the top left.
pub fn titled(s: &mut Screen, x: usize, y: usize, w: usize, h: usize, title: &str) {
    card(s, x, y, w, h);
    s.text(x + 24, y + 18, title, Face::BodyB, pal().dim);
}

/// Bar of `pct` % in `c` on the track.
pub fn bar(s: &mut Screen, x: usize, y: usize, w: usize, h: usize, pct: u64, c: u32) {
    let f = (w * pct.min(100) as usize / 100)
        .max(if pct > 0 { h } else { 0 })
        .min(w);
    s.fill_round(x, y, w, h, h / 2, pal().track);
    if f > 0 {
        s.fill_round(x, y, f, h, h / 2, c);
    }
}

/// Load color: good, warning, bad.
pub fn load_color(p: u64) -> u32 {
    let c = pal();
    match p {
        85.. => c.bad,
        50.. => c.warn,
        _ => c.good,
    }
}

/// Memory split in three: kernel, programs, free.
#[allow(clippy::too_many_arguments)]
pub fn split_bar(
    s: &mut Screen,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    total: u64,
    a: u64,
    b: u64,
) {
    let p = pal();
    let part = |n: u64| ((w as u64 * n / total.max(1)) as usize).max(h);
    let (ka, kb) = (part(a).min(w), part(b));
    s.fill_round(x, y, w, h, h / 2, p.track);
    let both = (ka + kb).min(w);
    s.fill_round(x, y, both, h, h / 2, p.good);
    s.fill_round(x, y, ka, h, h / 2, p.accent);
}

/// Legend dot.
pub fn dot(s: &mut Screen, x: usize, y: usize, c: u32) {
    s.disc(x + 6, y + 6, 6, c);
}

/// Label on the left, value on the right of one row, optionally led by a
/// legend dot.
pub fn kv(
    s: &mut Screen,
    mut x: usize,
    xr: usize,
    y: usize,
    name: &str,
    value: &str,
    dot_c: Option<u32>,
) {
    let p = pal();
    if let Some(c) = dot_c {
        dot(s, x, y + 8, c);
        x += 22;
    }
    s.text(x, y, name, Face::Body, p.dim);
    s.text_r(xr, y, value, Face::Body, p.text);
}

/// Four signal bars from `x`, bottom at `y`; `n` of them lit.
pub fn signal_bars(s: &mut Screen, x: usize, y: usize, n: usize, lit: u32) {
    let p = pal();
    for i in 0..4 {
        let h = 8 + i * 5;
        s.fill_round(x + i * 8, y - h, 5, h, 2, if i < n { lit } else { p.track });
    }
}

/// Bars lit for a signal of -`dbm` dBm.
pub fn bars_for(dbm: u64) -> usize {
    match dbm {
        0..55 => 4,
        55..65 => 3,
        65..75 => 2,
        _ => 1,
    }
}

/// Outlined pill with a word, ending at `xr`; returns its width.
pub fn badge(s: &mut Screen, xr: usize, y: usize, text: &str, c: u32) -> usize {
    let w = font::width(Face::Cap, text) + 24;
    s.ring_round(xr - w, y, w, 28, 14, 2, c);
    s.text(xr - w + 12, vc(Face::Cap, y, 28), text, Face::Cap, c);
    w
}

/// Switch, 56 x 32.
pub fn switch(s: &mut Screen, x: usize, y: usize, on: bool, enabled: bool) {
    let p = pal();
    let bg = if !enabled {
        p.track
    } else if on {
        p.good
    } else {
        p.line
    };
    s.fill_round(x, y, 56, 32, 16, bg);
    let knob = if on { x + 56 - 16 - 4 } else { x + 16 + 4 };
    s.disc(knob, y + 16, 12, if enabled { 0xFFFF_FFFF } else { p.dim });
}

/// Switch-style slider with a 4 px mint-green track and a 28x28 rounded-square white knob.
pub fn slider(s: &mut Screen, x: usize, y: usize, w: usize, pct: u8, enabled: bool) {
    let p = pal();
    let (fill, knob) = if enabled {
        (p.good, 0xFFFF_FFFF)
    } else {
        (p.good, 0xFFF2_F4F8)
    };
    let kx = x + w * pct.min(100) as usize / 100;
    s.fill_round(x, y + 2, w, 4, 2, p.track);
    if kx > x {
        s.fill_round(x, y + 2, kx - x, 4, 2, fill);
    }
    let bx = kx.saturating_sub(14);
    let by = y.saturating_sub(10);
    s.round_a(bx, by + 2, 28, 28, 6, 0xFF00_0000, 70);
    s.fill_round(bx, by, 28, 28, 6, knob);
    if crate::theme::is_light() {
        s.ring_round(bx, by, 28, 28, 6, 1, p.line);
    }
}

/// Icon kinds of the UI.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum Icon {
    Gear,
    Sliders,
    Globe,
    Chip,
    Terminal,
    Logs,
    Moon,
    Power,
    Bolt,
    Sun,
    Speaker,
    Chevron,
    Left,
    Plane,
    WifiWave,
    Battery,
    SwitchHandheld,
    GameCard,
}

/// An icon centered on (cx, cy).
pub fn icon(s: &mut Screen, k: Icon, cx: usize, cy: usize, c: u32, bg: u32) {
    let (x, y) = (cx as i32, cy as i32);
    match k {
        Icon::Gear => {
            for a in 0..4 {
                let (dx, dy) = [(0, 1), (1, 0), (1, 1), (1, -1)][a];
                s.line(x - 11 * dx, y - 11 * dy, x + 11 * dx, y + 11 * dy, 5, c);
            }
            s.disc(cx, cy, 9, c);
            s.disc(cx, cy, 4, bg);
        }
        Icon::Sliders => {
            for (i, kx) in [(-6, -4), (0, 5), (6, -2)] {
                s.line(x - 10, y + i, x + 10, y + i, 2, c);
                s.fill_round((x + kx - 2) as usize, (y + i - 3) as usize, 4, 7, 1, c);
            }
        }
        Icon::Globe => {
            s.ring(cx, cy, 14, 2, c);
            s.line(x - 14, y, x + 14, y, 2, c);
            s.line(x, y - 14, x, y + 14, 2, c);
            s.ring_round(cx - 7, cy - 14, 14, 28, 7, 2, c);
        }
        Icon::Chip => {
            s.fill_round(cx - 10, cy - 10, 20, 20, 4, c);
            s.fill_round(cx - 5, cy - 5, 10, 10, 2, bg);
            for i in [-6, 0, 6] {
                s.line(x + i, y - 15, x + i, y - 11, 2, c);
                s.line(x + i, y + 11, x + i, y + 15, 2, c);
                s.line(x - 15, y + i, x - 11, y + i, 2, c);
                s.line(x + 11, y + i, x + 15, y + i, 2, c);
            }
        }
        Icon::Terminal => {
            s.ring_round(cx - 14, cy - 11, 28, 22, 4, 2, c);
            s.line(x - 8, y - 4, x - 3, y, 2, c);
            s.line(x - 8, y + 4, x - 3, y, 2, c);
            s.line(x + 1, y + 4, x + 8, y + 4, 2, c);
        }
        Icon::Logs => {
            for i in 0..4 {
                let w = if i == 3 { 12 } else { 24 };
                s.fill_round(cx - 12, cy - 11 + i * 7, w, 3, 1, c);
            }
        }
        Icon::Moon => {
            s.disc(cx, cy, 11, c);
            s.disc(cx + 5, cy - 4, 9, bg);
        }
        Icon::Power => {
            s.ring(cx, cy + 1, 10, 2, c);
            s.fill_round(cx - 5, cy - 13, 10, 12, 0, bg);
            s.line(x, y - 11, x, y + 1, 2, c);
        }
        Icon::Bolt => {
            s.line(x + 2, y - 9, x - 4, y + 1, 2, c);
            s.line(x - 4, y + 1, x + 3, y + 1, 2, c);
            s.line(x + 3, y + 1, x - 2, y + 10, 2, c);
        }
        Icon::Sun => {
            s.disc(cx, cy, 5, c);
            for (dx, dy) in [(0, 1), (1, 0), (1, 1), (1, -1)] {
                s.line(x - 10 * dx, y - 10 * dy, x - 7 * dx, y - 7 * dy, 2, c);
                s.line(x + 7 * dx, y + 7 * dy, x + 10 * dx, y + 10 * dy, 2, c);
            }
        }
        Icon::Speaker => {
            s.fill_round(cx - 9, cy - 4, 5, 8, 1, c);
            s.line(x - 5, y - 4, x, y - 8, 3, c);
            s.line(x - 5, y + 4, x, y + 8, 3, c);
            s.line(x, y - 8, x, y + 8, 3, c);
            s.line(x + 5, y - 4, x + 7, y, 2, c);
            s.line(x + 7, y, x + 5, y + 4, 2, c);
        }
        Icon::Chevron => {
            s.line(x - 3, y - 7, x + 4, y, 2, c);
            s.line(x - 3, y + 7, x + 4, y, 2, c);
        }
        Icon::Left => {
            s.line(x + 3, y - 8, x - 4, y, 3, c);
            s.line(x + 3, y + 8, x - 4, y, 3, c);
        }
        Icon::Plane => {
            s.line(x - 11, y + 2, x + 11, y - 6, 3, c);
            s.line(x - 2, y - 1, x - 8, y + 10, 3, c);
            s.line(x - 3, y - 6, x - 10, y - 11, 3, c);
        }
        Icon::WifiWave => {
            // Fan-shaped Wi-Fi icon: 2 top arcs + bottom dot.
            s.line(x - 8, y - 3, x, y - 8, 2, c);
            s.line(x, y - 8, x + 8, y - 3, 2, c);
            s.line(x - 5, y + 1, x, y - 3, 2, c);
            s.line(x, y - 3, x + 5, y + 1, 2, c);
            s.disc(cx, cy + 5, 2, c);
        }
        Icon::Battery => {
            // Horizontal battery body + right terminal nub.
            s.ring_round(cx - 13, cy - 7, 24, 14, 3, 2, c);
            s.fill_round(cx - 10, cy - 4, 18, 8, 1, c);
            s.fill_round(cx + 11, cy - 3, 3, 6, 1, c);
        }
        Icon::SwitchHandheld => {
            // Handheld Switch console icon (48 x 22).
            s.ring_round(cx - 24, cy - 11, 48, 22, 5, 2, c);
            s.fill_round(cx - 13, cy - 9, 26, 18, 1, c);
            s.disc(cx - 19, cy - 3, 2, c);
            s.disc(cx + 19, cy + 2, 2, c);
        }
        Icon::GameCard => {
            // Blue game cartridge icon next to selected title (18 x 22).
            s.fill_round(cx - 9, cy - 11, 18, 22, 4, c);
            s.fill_round(cx - 4, cy - 4, 8, 10, 2, bg);
        }
    }
}

/// Surface or raised background: `c` or a lighter `c` when pressed.
pub fn press(c: u32, pressed: bool) -> u32 {
    if pressed {
        mix(c, pal().accent, 60)
    } else {
        c
    }
}

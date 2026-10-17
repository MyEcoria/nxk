//! Shapes the pages share: cards, bars, switches, sliders, badges, the
//! controller's button glyphs and the icons, all drawn from rounded
//! rectangles, circles and lines.

use alloc::format;
use alloc::string::String;

use crate::font::{self, Face};
use crate::screen::{mix, Screen};
use crate::theme::{is_light, pal, BLACK, WHITE};

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

/// A card: the surface, rounded.
pub fn card(s: &mut Screen, x: usize, y: usize, w: usize, h: usize) {
    s.fill_round(x, y, w, h, 8, pal().surface);
}

/// Card with its heading, small capitals, at the top left.
pub fn titled(s: &mut Screen, x: usize, y: usize, w: usize, h: usize, title: &str) {
    card(s, x, y, w, h);
    heading(s, x + 24, y + 30, title);
}

/// A small heading in capitals, as the home row's play time.
pub fn heading(s: &mut Screen, x: usize, cy: usize, t: &str) -> usize {
    let up: String = t.chars().map(|c| c.to_ascii_uppercase()).collect();
    s.label(x, cy, &up, Face::CapB, pal().dim)
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
    s.fill_round(x, y, (ka + kb).min(w), h, h / 2, p.good);
    s.fill_round(x, y, ka, h, h / 2, p.accent);
}

/// Label on the left, value on the right of one row centered on `cy`,
/// optionally led by a legend dot.
pub fn kv(
    s: &mut Screen,
    mut x: usize,
    xr: usize,
    cy: usize,
    name: &str,
    value: &str,
    dot: Option<u32>,
) {
    let p = pal();
    if let Some(c) = dot {
        s.disc(x + 6, cy, 6, c);
        x += 22;
    }
    s.label(x, cy, name, Face::Small, p.dim);
    s.label_r(xr, cy, value, Face::Small, p.text);
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

/// Outlined pill with a word, ending at `xr`, centered on `cy`; returns
/// its width.
pub fn badge(s: &mut Screen, xr: usize, cy: usize, text: &str, c: u32) -> usize {
    let w = font::width(Face::CapB, text) + 24;
    s.ring_round(xr - w, cy - 14, w, 28, 14, 2, c);
    s.label(xr - w + 12, cy, text, Face::CapB, c);
    w
}

/// Switch, 56 x 32.
pub fn switch(s: &mut Screen, x: usize, y: usize, on: bool) {
    let p = pal();
    s.fill_round(x, y, 56, 32, 16, if on { p.good } else { p.track });
    let knob = if on { x + 56 - 16 } else { x + 16 };
    s.disc(knob, y + 16, 12, WHITE);
}

/// The drawer's slider: a thin track filled to `pct` and a square knob,
/// centered on `cy`; grey when it cannot be moved.
pub fn slider(s: &mut Screen, x: usize, cy: usize, w: usize, pct: u8, enabled: bool) {
    let p = pal();
    let kx = x + w * pct.min(100) as usize / 100;
    s.fill_round(x, cy - 1, w, 3, 1, p.track);
    if kx > x {
        s.fill_round(
            x,
            cy - 1,
            kx - x,
            3,
            1,
            if enabled { p.good } else { p.faint },
        );
    }
    let (bx, by) = (kx.saturating_sub(20), cy - 20);
    s.round_a(bx, by + 3, 40, 40, 6, BLACK, 90);
    s.fill_round(bx, by, 40, 40, 6, if enabled { WHITE } else { p.faint });
    if is_light() {
        s.ring_round(bx, by, 40, 40, 6, 1, p.line);
    }
}

/// A controller button as the hint bar shows it: a disc with its letter
/// (or the - and + of the small buttons).
pub fn button_glyph(s: &mut Screen, cx: usize, cy: usize, b: &str, c: u32, bg: u32) {
    s.disc(cx, cy, 11, c);
    let (x, y) = (cx as i32, cy as i32);
    match b {
        "-" => s.line(x - 5, y, x + 5, y, 3, bg),
        "+" => {
            s.line(x - 5, y, x + 5, y, 3, bg);
            s.line(x, y - 5, x, y + 5, 3, bg);
        }
        // A house, as on the HOME button.
        "HOME" => {
            s.line(x - 6, y, x, y - 6, 2, bg);
            s.line(x, y - 6, x + 6, y, 2, bg);
            s.fill(cx - 4, cy, 9, 5, bg);
        }
        _ => {
            s.label_c(cx + 1, cy, b, Face::CapB, bg);
        }
    }
}

/// Icon kinds of the UI.
#[derive(Clone, Copy)]
pub enum Icon {
    Sliders,
    Power,
    Bolt,
    Sun,
    Speaker,
    Chevron,
    Plane,
    Wifi,
    Handheld,
    GameCard,
}

/// An icon centered on (cx, cy) in `c`, on `bg`.
pub fn icon(s: &mut Screen, k: Icon, cx: usize, cy: usize, c: u32, bg: u32) {
    let (x, y) = (cx as i32, cy as i32);
    match k {
        Icon::Sliders => {
            for (dy, kx) in [(-7, 4), (0, -5), (7, 2)] {
                s.line(x - 11, y + dy, x + 11, y + dy, 2, c);
                s.fill_round((x + kx - 2) as usize, (y + dy - 4) as usize, 5, 9, 1, c);
            }
        }
        Icon::Power => {
            s.ring(cx, cy + 1, 10, 2, c);
            s.fill_round(cx - 5, cy - 13, 10, 12, 0, bg);
            s.line(x, y - 11, x, y + 1, 2, c);
        }
        Icon::Bolt => {
            s.line(x + 2, y - 6, x - 3, y + 1, 2, c);
            s.line(x - 3, y + 1, x + 3, y - 1, 2, c);
            s.line(x + 3, y - 1, x - 2, y + 6, 2, c);
        }
        Icon::Sun => {
            s.disc(cx, cy, 5, c);
            for (dx, dy) in [(0, 1), (1, 0), (1, 1), (1, -1)] {
                s.line(x - 11 * dx, y - 11 * dy, x - 8 * dx, y - 8 * dy, 2, c);
                s.line(x + 8 * dx, y + 8 * dy, x + 11 * dx, y + 11 * dy, 2, c);
            }
        }
        Icon::Speaker => {
            s.fill_round(cx - 10, cy - 4, 5, 9, 1, c);
            s.line(x - 6, y - 4, x, y - 9, 3, c);
            s.line(x - 6, y + 4, x, y + 9, 3, c);
            s.line(x, y - 9, x, y + 9, 3, c);
            s.line(x + 5, y - 4, x + 7, y, 2, c);
            s.line(x + 7, y, x + 5, y + 4, 2, c);
        }
        Icon::Chevron => {
            s.line(x - 3, y - 7, x + 4, y, 2, c);
            s.line(x - 3, y + 7, x + 4, y, 2, c);
        }
        Icon::Plane => {
            s.line(x - 10, y + 2, x + 10, y - 5, 3, c);
            s.line(x - 2, y - 1, x - 7, y + 9, 3, c);
            s.line(x - 3, y - 5, x - 9, y - 10, 3, c);
        }
        Icon::Wifi => {
            s.arc_up(cx, cy + 8, 16, 3, c);
            s.arc_up(cx, cy + 8, 10, 3, c);
            s.disc(cx, cy + 5, 3, c);
        }
        Icon::Handheld => {
            // The Switch with its Joy-Con, 74 x 32.
            s.fill_round(cx - 37, cy - 16, 74, 32, 9, c);
            s.fill_round(cx - 25, cy - 14, 50, 28, 2, bg);
            s.ring_round(cx - 22, cy - 11, 44, 22, 2, 2, c);
            s.disc(cx - 31, cy - 6, 3, bg);
            s.disc(cx + 31, cy + 5, 3, bg);
        }
        Icon::GameCard => {
            s.fill_round(cx - 10, cy - 13, 20, 26, 4, c);
            s.fill_round(cx - 5, cy - 4, 10, 11, 2, bg);
        }
    }
}

/// The battery centered on (cx, cy), its inside filled `pct` % in `fill`;
/// `None`: level unknown, left empty.
pub fn battery(s: &mut Screen, cx: usize, cy: usize, pct: Option<u64>, c: u32, fill: u32) {
    s.ring_round(cx - 14, cy - 8, 26, 16, 3, 2, c);
    s.fill_round(cx + 13, cy - 3, 3, 6, 1, c);
    if let Some(p) = pct {
        let w = (p.min(100) as usize * 20).div_ceil(100);
        if w > 0 {
            s.fill_round(cx - 11, cy - 5, w, 10, if w > 2 { 1 } else { 0 }, fill);
        }
    }
}

/// A control's background: `c`, or tinted while pressed.
pub fn press(c: u32, pressed: bool) -> u32 {
    if pressed {
        mix(c, pal().accent, 60)
    } else {
        c
    }
}

/// The volume indicator: a pill at the top center with the speaker, the
/// level bar and the percentage.
pub fn volume_hud(s: &mut Screen, y: usize, pct: u8) {
    let p = pal();
    // Sized for "100%" so the bar does not move with the level.
    let tw = font::width(Face::Body, "100%");
    let (w, h) = (76 + 200 + 20 + tw + 28, 56);
    let x = (crate::ui::W - w) / 2;
    s.shadow(x, y, w, h, h / 2, BLACK);
    s.fill_round(x, y, w, h, h / 2, p.text);
    icon(s, Icon::Speaker, x + 40, y + h / 2, p.bg, p.text);
    if pct == 0 {
        let (x0, y0) = ((x + 28) as i32, (y + 40) as i32);
        s.line(x0, y0, x0 + 24, y0 - 24, 3, p.bg);
    }
    let (bx, bw) = (x + 76, 200);
    let cy = y + h / 2;
    s.fill_round(bx, cy - 3, bw, 7, 3, p.dim);
    let fw = bw * pct.min(100) as usize / 100;
    if fw > 0 {
        s.fill_round(bx, cy - 3, fw.max(7), 7, 3, p.bg);
    }
    s.label_r(x + w - 28, cy, &format!("{}%", pct), Face::Body, p.bg);
}

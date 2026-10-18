//! A user's picture: a disc in one of `users::COLORS` with a small
//! pixel-art face on it, and the animation helpers the user screens share
//! (integer easing, a sine, fades). Times are nanoseconds, easing values
//! thousandths.

use crate::screen::{mix, Screen};
use crate::theme::{rgb, BLACK, WHITE};
use crate::users::{COLORS, GLYPHS};

/// 8x8 faces, one byte per row, the high bit on the left.
const SPRITES: [[u8; 8]; GLYPHS] = [
    // smile
    [0x3C, 0x42, 0xA5, 0x81, 0xA5, 0x99, 0x42, 0x3C],
    // heart
    [0x00, 0x66, 0xFF, 0xFF, 0xFF, 0x7E, 0x3C, 0x18],
    // star
    [0x18, 0x18, 0xFF, 0x7E, 0x3C, 0x7E, 0x66, 0x42],
    // cat
    [0x81, 0xC3, 0xFF, 0xDB, 0xFF, 0xE7, 0x7E, 0x3C],
    // ghost
    [0x3C, 0x7E, 0xDB, 0xDB, 0xFF, 0xFF, 0xFF, 0xA5],
    // bolt
    [0x0C, 0x18, 0x30, 0x7E, 0x0C, 0x18, 0x30, 0x20],
    // alien
    [0x24, 0x18, 0x3C, 0x5A, 0xFF, 0xBD, 0xA5, 0x24],
    // gem
    [0x18, 0x3C, 0x7E, 0xFF, 0x7E, 0x3C, 0x18, 0x00],
];

pub fn color(i: u8) -> u32 {
    rgb(COLORS[i as usize % COLORS.len()])
}

/// The picture centered on (cx, cy), `r` its radius.
pub fn avatar(s: &mut Screen, cx: usize, cy: usize, r: usize, col: u8, glyph: u8) {
    if r == 0 {
        return;
    }
    let c = color(col);
    s.disc(cx, cy, r, c);
    let px = (r * 13 / 80).max(1);
    let (x0, y0) = (cx - 4 * px, cy - 4 * px);
    let shade = mix(c, BLACK, 90);
    let off = (px / 2).max(1);
    for (ink, dx) in [(shade, off), (WHITE, 0)] {
        for (row, &bits) in SPRITES[glyph as usize % GLYPHS].iter().enumerate() {
            let mut x = 0;
            while x < 8 {
                if bits & (0x80 >> x) == 0 {
                    x += 1;
                    continue;
                }
                let mut end = x + 1;
                while end < 8 && bits & (0x80 >> end) != 0 {
                    end += 1;
                }
                s.fill(
                    x0 + x * px + dx,
                    y0 + row * px + dx,
                    (end - x) * px,
                    px,
                    ink,
                );
                x = end;
            }
        }
    }
}

/// A padlock centered on (cx, cy), about 14 px wide.
pub fn padlock(s: &mut Screen, cx: usize, cy: usize, c: u32) {
    s.ring_round(cx - 5, cy - 10, 10, 14, 5, 2, c);
    s.fill_round(cx - 7, cy - 2, 14, 11, 2, c);
}

/// Progress of an animation that starts `delay_ms` after `t` = 0 and lasts
/// `dur_ms`: 0..=1000.
pub fn prog(t: u64, delay_ms: u64, dur_ms: u64) -> i64 {
    let ms = t / 1_000_000;
    if ms < delay_ms {
        0
    } else {
        ((ms - delay_ms) * 1000 / dur_ms.max(1)).min(1000) as i64
    }
}

/// Slows down towards the end.
pub fn ease_out(t: i64) -> i64 {
    let u = 1000 - t.clamp(0, 1000);
    1000 - u * u / 1000
}

/// Goes past 1000 and settles back: things that pop in.
pub fn ease_back(t: i64) -> i64 {
    let x = t.clamp(0, 1000) - 1000;
    let x2 = x * x / 1000;
    let x3 = x2 * x / 1000;
    1000 + 2702 * x3 / 1000 + 1702 * x2 / 1000
}

/// Sine of `p` (1024 to the turn), -1000..=1000, from two parabolas.
pub fn sin(p: i64) -> i64 {
    let p = p.rem_euclid(1024);
    let (x, sign) = if p < 512 { (p, 1) } else { (p - 512, -1) };
    sign * x * (512 - x) * 1000 / 65536
}

/// `c` seen through `e` thousandths on `bg`.
pub fn fade(c: u32, bg: u32, e: i64) -> u32 {
    mix(bg, c, (e.clamp(0, 1000) * 255 / 1000) as usize)
}

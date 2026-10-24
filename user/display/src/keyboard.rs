//! The on-screen keyboard the user flows (a name) and the Wi-Fi sheet (a
//! passphrase) share: digits, letters with Shift and, where the text may
//! have them, a layer of the 32 ASCII symbols. A key is `Id::Key(byte)`.

use crate::screen::Screen;
use crate::ui::{Id, Ui};
use crate::widgets::Tone;

const DIGITS: &[u8] = b"1234567890";
const LETTERS: [&[u8]; 3] = [b"qwertyuiop", b"asdfghjkl", b"zxcvbnm"];
const SYMBOLS: [&[u8]; 3] = [b"!@#$%^&*()", b"-_=+[]{};:", b"'\"\\|,.<>/?"];
/// The two symbols left over, on either side of the space bar.
const EDGE: [u8; 2] = *b"`~";

/// Where and how the keyboard is drawn: `w` is the width of ten keys.
pub(crate) struct Keys {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub kh: usize,
    pub gx: usize,
    pub gy: usize,
    /// The layer of symbols is offered (and `symbols` says it is shown).
    pub layers: bool,
    pub symbols: bool,
    pub shift: bool,
    pub on: bool,
}

impl Ui {
    /// Five rows of keys; returns where they end.
    pub(crate) fn keyboard(&mut self, s: &mut Screen, k: &Keys) -> usize {
        let kw = (k.w - 9 * k.gx) / 10;
        let wide = 130;
        let row_y = |r: usize| k.y + r * (k.kh + k.gy);
        let letters = !k.symbols;
        let rows: [&[u8]; 4] = if letters {
            [DIGITS, LETTERS[0], LETTERS[1], LETTERS[2]]
        } else {
            [DIGITS, SYMBOLS[0], SYMBOLS[1], SYMBOLS[2]]
        };
        for (r, row) in rows.iter().enumerate() {
            let shifted = letters && r == 3;
            let used = row.len() * (kw + k.gx) - k.gx;
            let side = if shifted { wide + k.gx } else { 0 };
            let mut x = k.x + (k.w - used - side) / 2;
            let y = row_y(r);
            if shifted {
                self.chip(s, Id::KeyShift, x, y, wide, k.kh, "Shift", k.shift, k.on);
                x += side;
            }
            for &c in row.iter() {
                self.key(s, c, x, y, kw, k);
                x += kw + k.gx;
            }
        }
        // Layer toggle, a key, the space bar, a key, delete.
        let y = row_y(4);
        let mut x = k.x;
        if k.layers {
            let space = k.w - 2 * wide - 2 * kw - 4 * k.gx;
            let label = if k.symbols { "abc" } else { "?123" };
            let edge = if k.symbols { EDGE } else { *b",." };
            self.button(s, Id::KeyLayer, x, y, wide, k.kh, label, Tone::Plain, k.on);
            x += wide + k.gx;
            self.key(s, edge[0], x, y, kw, k);
            x += kw + k.gx;
            self.button(
                s,
                Id::KeySpace,
                x,
                y,
                space,
                k.kh,
                "Space",
                Tone::Plain,
                k.on,
            );
            x += space + k.gx;
            self.key(s, edge[1], x, y, kw, k);
            x += kw + k.gx;
        } else {
            let space = k.w - wide - k.gx;
            self.button(
                s,
                Id::KeySpace,
                x,
                y,
                space,
                k.kh,
                "Space",
                Tone::Plain,
                k.on,
            );
            x += space + k.gx;
        }
        self.button(
            s,
            Id::KeyBack,
            x,
            y,
            wide,
            k.kh,
            "Delete",
            Tone::Plain,
            k.on,
        );
        y + k.kh
    }

    fn key(&mut self, s: &mut Screen, c: u8, x: usize, y: usize, kw: usize, k: &Keys) {
        let up = if k.shift && !k.symbols {
            c.to_ascii_uppercase()
        } else {
            c
        };
        let label = [up];
        let label = core::str::from_utf8(&label).unwrap_or("?");
        self.button(s, Id::Key(c), x, y, kw, k.kh, label, Tone::Plain, k.on);
    }
}

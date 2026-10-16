//! The UI's proportional font: `build/uifont.bin` (tools/mkuifont.py),
//! antialiased 4-bit glyphs in a few faces. Static data, no copy in RAM.

const DATA: &[u8] = include_bytes!("../../../build/uifont.bin");
/// Glyph records per face: ASCII 32..=126, then `°` `·` `•` `×` `‹` `›`.
const SLOTS: usize = 101;
const RECORD: usize = 12;

#[derive(Clone, Copy, PartialEq)]
pub enum Face {
    /// Captions.
    Cap,
    Body,
    BodyB,
    Title,
    TitleB,
    Head,
    /// Numerals only (digits, `.:%-°` and a few unit letters).
    Big,
}

/// One glyph: its bitmap size and position from the pen on the baseline.
pub struct Glyph {
    pub w: usize,
    pub h: usize,
    pub xo: i32,
    /// Rows from the baseline up to the top of the bitmap.
    pub yo: i32,
    pub adv: usize,
    bits: usize,
}

fn le32(at: usize) -> usize {
    u32::from_le_bytes([DATA[at], DATA[at + 1], DATA[at + 2], DATA[at + 3]]) as usize
}

/// Start of a face's block: its records, then the bitmaps.
fn block(f: Face) -> usize {
    le32(1 + 8 * f as usize + 4)
}

pub fn line_height(f: Face) -> usize {
    DATA[1 + 8 * f as usize] as usize
}

pub fn ascent(f: Face) -> usize {
    DATA[1 + 8 * f as usize + 1] as usize
}

fn slot(c: char) -> usize {
    match c {
        ' '..='~' => c as usize - 32,
        '\u{b0}' => 95,
        '\u{b7}' => 96,
        '\u{2022}' => 97,
        '\u{d7}' => 98,
        '\u{2039}' => 99,
        '\u{203a}' => 100,
        _ => '?' as usize - 32,
    }
}

pub fn glyph(f: Face, c: char) -> Glyph {
    let b = block(f);
    let r = b + slot(c) * RECORD;
    Glyph {
        w: DATA[r] as usize,
        h: DATA[r + 1] as usize,
        xo: DATA[r + 2] as i8 as i32,
        yo: DATA[r + 3] as i8 as i32,
        adv: DATA[r + 4] as usize,
        bits: b + SLOTS * RECORD + le32(r + 8),
    }
}

impl Glyph {
    /// Alpha (0..=15) of pixel (x, y) of the bitmap.
    pub fn alpha(&self, x: usize, y: usize) -> u32 {
        let i = y * self.w + x;
        let byte = DATA[self.bits + i / 2];
        (if i.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 15
        }) as u32
    }
}

/// Width of `s` in pixels.
pub fn width(f: Face, s: &str) -> usize {
    s.chars().map(|c| glyph(f, c).adv).sum()
}

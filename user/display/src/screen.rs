//! Landscape drawing on the window image: rectangles, 8x16 glyphs and
//! text grids that redraw a cell only when it changes. A portrait panel
//! (Switch) is drawn turned, as hekate stores its boot logo.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use libnx::mem::SharedMemory;

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");

// Colors, A B G R.
pub const BG: u32 = 0xFF3A_1E10;
pub const FG: u32 = 0xFFD8_D8D8;
pub const DIM: u32 = 0xFF98_8880;
pub const GOOD: u32 = 0xFF6A_D06A;
pub const WARN: u32 = 0xFF40_C0F0;
pub const BAD: u32 = 0xFF50_60F0;

/// Widest text line: 1280 px of 8 px glyphs.
pub const MAX_COLS: usize = 160;

pub struct Screen {
    pub fb: SharedMemory,
    /// Image size, in panel scan order.
    w: usize,
    h: usize,
    /// Portrait panel: landscape view turned 90 degrees counterclockwise.
    rot: bool,
}

impl Screen {
    pub fn new(fb: SharedMemory, w: usize, h: usize) -> Self {
        Self {
            fb,
            w,
            h,
            rot: h > w,
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.fb.as_ptr() as *mut u32, self.w * self.h) }
    }

    /// Landscape size.
    pub fn size(&self) -> (usize, usize) {
        if self.rot {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        }
    }

    pub fn turned(&self) -> bool {
        self.rot
    }

    /// Landscape rectangle: runs of image rows (image columns when turned).
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        let (iw, ih, rot) = (self.w, self.h, self.rot);
        let px = self.pixels();
        if rot {
            // Landscape (x, y) is image pixel (row ih-1-x, column y).
            for r in ih - x - w..ih - x {
                px[r * iw + y..r * iw + y + h].fill(c);
            }
        } else {
            for r in y..y + h {
                px[r * iw + x..r * iw + x + w].fill(c);
            }
        }
    }

    pub fn clear(&mut self) {
        self.pixels().fill(BG);
    }

    /// Glyph `ch` (Latin-1) at landscape pixel (x, y), `scale` times enlarged.
    pub fn glyph(&mut self, x: usize, y: usize, ch: u8, scale: usize, fg: u32) {
        let g = ch as usize * 16;
        for (dy, &bits) in FONT[g..g + 16].iter().enumerate() {
            // Runs of equal pixels in the row: one rectangle each.
            let mut dx = 0;
            while dx < 8 {
                let on = bits & (0x80 >> dx) != 0;
                let mut end = dx + 1;
                while end < 8 && (bits & (0x80 >> end) != 0) == on {
                    end += 1;
                }
                let c = if on { fg } else { BG };
                self.fill(x + dx * scale, y + dy * scale, (end - dx) * scale, scale, c);
                dx = end;
            }
        }
    }
}

/// One line being composed: characters with their colors.
pub struct Line {
    cells: [(u8, u32); MAX_COLS],
    len: usize,
    color: u32,
}

impl Line {
    pub fn new() -> Self {
        Self {
            cells: [(b' ', FG); MAX_COLS],
            len: 0,
            color: FG,
        }
    }

    /// Color of what is written next.
    pub fn color(&mut self, c: u32) -> &mut Self {
        self.color = c;
        self
    }

    /// Spaces up to column `col`.
    pub fn pad(&mut self, col: usize) -> &mut Self {
        while self.len < col.min(MAX_COLS) {
            self.cells[self.len] = (b' ', FG);
            self.len += 1;
        }
        self
    }

    fn cell(&self, col: usize) -> (u8, u32) {
        if col < self.len {
            self.cells[col]
        } else {
            (b' ', FG)
        }
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            if self.len == MAX_COLS {
                break;
            }
            let b = u8::try_from(ch as u32).unwrap_or(b'?');
            self.cells[self.len] = (b, self.color);
            self.len += 1;
        }
        Ok(())
    }
}

/// Text cells at `scale` from landscape pixel (x0, y0).
pub struct Grid {
    x0: usize,
    y0: usize,
    pub scale: usize,
    pub cols: usize,
    pub rows: usize,
    cells: Vec<(u8, u32)>,
}

impl Grid {
    pub fn new(x0: usize, y0: usize, scale: usize, cols: usize, rows: usize) -> Self {
        Self {
            x0,
            y0,
            scale,
            cols,
            rows,
            cells: vec![(b' ', FG); cols * rows],
        }
    }

    /// Landscape pixel of cell (col, row).
    pub fn pos(&self, col: usize, row: usize) -> (usize, usize) {
        (
            self.x0 + col * 8 * self.scale,
            self.y0 + row * 16 * self.scale,
        )
    }

    /// Row `row` becomes `l` (blank past its end); only changed cells
    /// are drawn.
    pub fn line(&mut self, scr: &mut Screen, row: usize, l: &Line) {
        if row >= self.rows {
            return;
        }
        for col in 0..self.cols {
            let c = l.cell(col);
            let i = row * self.cols + col;
            if self.cells[i] != c {
                self.cells[i] = c;
                let (x, y) = self.pos(col, row);
                scr.glyph(x, y, c.0, self.scale, c.1);
            }
        }
    }
}

/// Byte count with a binary unit and one decimal.
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let n = self.0;
        match [(30, "GiB"), (20, "MiB"), (10, "KiB")]
            .iter()
            .find(|(s, _)| n >> s != 0)
        {
            Some(&(s, unit)) => write!(
                f,
                "{}.{} {}",
                n >> s,
                ((n & ((1 << s) - 1)) * 10) >> s,
                unit
            ),
            None => write!(f, "{} B", n),
        }
    }
}

/// IPv4 address (`a << 24 | b << 16 | c << 8 | d`).
pub struct Ipv4(pub u32);

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let [a, b, c, d] = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", a, b, c, d)
    }
}

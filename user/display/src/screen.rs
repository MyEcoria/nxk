//! Landscape drawing: rectangles, 8x16 glyphs, the logo. A frame is drawn
//! whole into a cached image, then only its changed pixels are copied to
//! the displayed one (uncached): no flicker, few slow writes. A portrait
//! panel (Switch) is drawn turned, as hekate stores its boot logo.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use libnx::mem::SharedMemory;

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
/// tools/mklogo.py --raw: width, height, 8 R G B A palette entries, indices.
const LOGO: &[u8] = include_bytes!("../../../build/logo.bin");

// Colors, A B G R (docs/ui/home.html uses the same values).
pub const BG: u32 = 0xFF3A_1E10;
pub const FG: u32 = 0xFFD8_D8D8;
pub const DIM: u32 = 0xFF98_8880;
pub const GOOD: u32 = 0xFF6A_D06A;
pub const WARN: u32 = 0xFF40_C0F0;
pub const BAD: u32 = 0xFF50_60F0;
/// Empty part of a bar.
pub const TRACK: u32 = 0xFF50_3020;
/// Cards, their outline, a pressed control.
pub const PANEL: u32 = 0xFF4C_2916;
pub const EDGE: u32 = 0xFF75_462B;
pub const PRESS: u32 = 0xFF8A_4F2A;
pub const BLUE: u32 = 0xFFF0_A06C;
pub const TERM_BG: u32 = 0xFF24_130B;

/// Logo palette entry `i` (A B G R).
pub fn palette(i: usize) -> u32 {
    u32::from_le_bytes([
        LOGO[2 + 4 * i],
        LOGO[3 + 4 * i],
        LOGO[4 + 4 * i],
        LOGO[5 + 4 * i],
    ])
}

/// Labels and titles: the logo's light amber.
pub fn accent() -> u32 {
    palette(6)
}

pub fn logo_size() -> (usize, usize) {
    (LOGO[0] as usize, LOGO[1] as usize)
}

/// Width of `s` in pixels at `scale`.
pub fn width(s: &str, scale: usize) -> usize {
    s.chars().count() * 8 * scale
}

pub struct Screen {
    pub fb: SharedMemory,
    /// Frame being drawn, and what the displayed image holds; both in
    /// panel scan order.
    back: Vec<u32>,
    shown: Vec<u32>,
    /// Image size, in panel scan order.
    w: usize,
    h: usize,
    /// Added to every x of the drawing calls (a card that is partly off
    /// the screen is drawn in its own coordinates); clipped to the screen.
    pub dx: i32,
    /// Portrait panel: landscape view turned 90 degrees counterclockwise.
    rot: bool,
}

impl Screen {
    pub fn new(fb: SharedMemory, w: usize, h: usize) -> Self {
        Self {
            fb,
            back: vec![BG; w * h],
            // No drawn color is 0 (all opaque): the first frame is copied whole.
            shown: vec![0; w * h],
            w,
            h,
            dx: 0,
            rot: h > w,
        }
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

    /// Landscape rectangle, clipped to the screen: runs of image rows
    /// (image columns when turned).
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        let (sw, sh) = self.size();
        let x0 = (x as i32 + self.dx).max(0);
        let x1 = (x as i32 + self.dx + w as i32).min(sw as i32);
        if x1 <= x0 {
            return;
        }
        let (x, w) = (x0 as usize, (x1 - x0) as usize);
        let (w, h) = (w.min(sw.saturating_sub(x)), h.min(sh.saturating_sub(y)));
        if w == 0 || h == 0 {
            return;
        }
        let (iw, ih) = (self.w, self.h);
        if self.rot {
            // Landscape (x, y) is image pixel (row ih-1-x, column y).
            for r in ih - x - w..ih - x {
                self.back[r * iw + y..r * iw + y + h].fill(c);
            }
        } else {
            for r in y..y + h {
                self.back[r * iw + x..r * iw + x + w].fill(c);
            }
        }
    }

    pub fn clear(&mut self, c: u32) {
        self.back.fill(c);
    }

    /// Rectangle with corners of radius `r`.
    pub fn fill_round(&mut self, x: usize, y: usize, w: usize, h: usize, r: usize, c: u32) {
        let r = r.min(w / 2).min(h / 2);
        for dy in 0..r {
            // Width of the circle at the middle of this row, in half pixels.
            let a = 2 * (r - dy) - 1;
            let ext = isqrt(4 * r * r - a * a) / 2;
            let inset = r - ext.min(r);
            self.fill(x + inset, y + dy, w - 2 * inset, 1, c);
            self.fill(x + inset, y + h - 1 - dy, w - 2 * inset, 1, c);
        }
        self.fill(x, y + r, w, h - 2 * r, c);
    }

    /// Rounded outline `t` thick: `edge` around `inner`.
    #[allow(clippy::too_many_arguments)]
    pub fn card_round(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        r: usize,
        t: usize,
        edge: u32,
        inner: u32,
    ) {
        self.fill_round(x, y, w, h, r, edge);
        self.fill_round(
            x + t,
            y + t,
            w - 2 * t,
            h - 2 * t,
            r.saturating_sub(t),
            inner,
        );
    }

    /// Vertical gradient from `top` to `bottom`.
    pub fn gradient(&mut self, x: usize, y: usize, w: usize, h: usize, top: u32, bottom: u32) {
        for i in 0..h {
            self.fill(x, y + i, w, 1, mix(top, bottom, i * 255 / h.max(1)));
        }
    }

    /// `c` over what is there, `a` of 255 opaque.
    pub fn fill_alpha(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32, a: usize) {
        let (sw, sh) = self.size();
        let x0 = (x as i32 + self.dx).max(0);
        let x1 = (x as i32 + self.dx + w as i32).min(sw as i32);
        if x1 <= x0 {
            return;
        }
        let (x, w) = (x0 as usize, (x1 - x0) as usize);
        let (w, h) = (w.min(sw.saturating_sub(x)), h.min(sh.saturating_sub(y)));
        let (iw, ih) = (self.w, self.h);
        for ly in y..y + h {
            for lx in x..x + w {
                let i = if self.rot {
                    (ih - 1 - lx) * iw + ly
                } else {
                    ly * iw + lx
                };
                self.back[i] = mix(self.back[i], c, a);
            }
        }
    }

    /// Outline `t` pixels thick.
    pub fn frame(&mut self, x: usize, y: usize, w: usize, h: usize, t: usize, c: u32) {
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
        self.fill(x, y, t, h, c);
        self.fill(x + w - t, y, t, h, c);
    }

    /// Glyph `ch` (Latin-1) at landscape pixel (x, y), `scale` times
    /// enlarged, on `bg`.
    pub fn glyph(&mut self, x: usize, y: usize, ch: u8, scale: usize, fg: u32, bg: u32) {
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
                let c = if on { fg } else { bg };
                // A background of 0 is not drawn: text over art.
                if on || bg != 0 {
                    self.fill(x + dx * scale, y + dy * scale, (end - dx) * scale, scale, c);
                }
                dx = end;
            }
        }
    }

    /// Text from (x, y), a character outside Latin-1 as '?'; returns its width.
    pub fn text(&mut self, x: usize, y: usize, s: &str, scale: usize, fg: u32, bg: u32) -> usize {
        let mut k = 0;
        for ch in s.chars() {
            let b = u8::try_from(ch as u32).unwrap_or(b'?');
            self.glyph(x + 8 * scale * k, y, b, scale, fg, bg);
            k += 1;
        }
        8 * scale * k
    }

    /// Text ending at `xr`.
    pub fn text_r(
        &mut self,
        xr: usize,
        y: usize,
        s: &str,
        scale: usize,
        fg: u32,
        bg: u32,
    ) -> usize {
        self.text(xr.saturating_sub(width(s, scale)), y, s, scale, fg, bg)
    }

    /// Text centered on `cx`.
    pub fn text_c(
        &mut self,
        cx: usize,
        y: usize,
        s: &str,
        scale: usize,
        fg: u32,
        bg: u32,
    ) -> usize {
        self.text(cx.saturating_sub(width(s, scale) / 2), y, s, scale, fg, bg)
    }

    /// The logo, `s` times enlarged, index 0 transparent.
    pub fn logo(&mut self, x0: usize, y0: usize, s: usize) {
        let (w, h) = logo_size();
        for (i, &p) in LOGO[34..34 + w * h].iter().enumerate() {
            if p != 0 {
                self.fill(x0 + i % w * s, y0 + i / w * s, s, s, palette(p as usize));
            }
        }
    }

    /// Copy the pixels that changed since the last call to the displayed
    /// image: per image row, the span from the first to the last change.
    pub fn present(&mut self) {
        let fb = unsafe {
            core::slice::from_raw_parts_mut(self.fb.as_ptr() as *mut u32, self.w * self.h)
        };
        for r in 0..self.h {
            let row = r * self.w..(r + 1) * self.w;
            let (b, s) = (&self.back[row.clone()], &mut self.shown[row.clone()]);
            let Some(first) = b.iter().zip(s.iter()).position(|(a, c)| a != c) else {
                continue;
            };
            let last = b
                .iter()
                .zip(s.iter())
                .rposition(|(a, c)| a != c)
                .unwrap_or(first);
            s[first..=last].copy_from_slice(&b[first..=last]);
            fb[row.start + first..=row.start + last].copy_from_slice(&b[first..=last]);
        }
    }

    /// A client drew on the image itself: the next frame is copied whole.
    pub fn forget(&mut self) {
        self.shown.fill(0);
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

/// Millidegrees as `42.1°C`.
pub struct Celsius(pub i64);

impl fmt::Display for Celsius {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}.{}\u{b0}C",
            self.0 / 1000,
            (self.0 % 1000).abs() / 100
        )
    }
}

/// Seconds as `h:mm:ss`.
pub struct Hms(pub u64);

impl fmt::Display for Hms {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = self.0;
        write!(f, "{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }
}

/// Integer square root.
fn isqrt(n: usize) -> usize {
    let mut r = 0;
    while (r + 1) * (r + 1) <= n {
        r += 1;
    }
    r
}

/// `a` towards `b`, `t` of 255 of the way, per channel (A B G R).
pub fn mix(a: u32, b: u32, t: usize) -> u32 {
    let t = t.min(255) as u32;
    let ch = |s: u32| ((a >> s & 0xFF) * (255 - t) + (b >> s & 0xFF) * t) / 255;
    0xFF00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

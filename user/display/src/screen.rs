//! Landscape drawing: antialiased rounded shapes, lines, text in the UI
//! font and Terminus (terminal and logs), the logo. A frame is drawn
//! whole into a 16-bit cached image (RGB565), then only its changed
//! pixels are copied, widened to 32 bits, to the displayed one
//! (uncached): no flicker, few slow writes, half the cache's memory. A
//! portrait panel (Switch) is drawn turned, as hekate stores its boot
//! logo.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use libnx::mem::SharedMemory;

use crate::art::Image;
use crate::font::{self, Face};

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
/// tools/mklogo.py --raw: width, height, 8 R G B A palette entries, indices.
const LOGO: &[u8] = include_bytes!("../../../build/logo.bin");

// The terminal's colors, A B G R: it is dark in both themes.
pub const FG: u32 = 0xFFD8_D8D8;
pub const DIM: u32 = 0xFF98_8880;
pub const GOOD: u32 = 0xFF6A_D06A;
pub const BAD: u32 = 0xFF50_60F0;
pub const BLUE: u32 = 0xFFF0_A06C;
pub const TERM_BG: u32 = 0xFF14_110F;

/// Logo palette entry `i` (A B G R).
pub fn palette(i: usize) -> u32 {
    u32::from_le_bytes([
        LOGO[2 + 4 * i],
        LOGO[3 + 4 * i],
        LOGO[4 + 4 * i],
        LOGO[5 + 4 * i],
    ])
}

pub fn logo_size() -> (usize, usize) {
    (LOGO[0] as usize, LOGO[1] as usize)
}

/// Width of `s` in the UI font.
pub fn width(f: Face, s: &str) -> usize {
    font::width(f, s)
}

/// A B G R to RGB565.
fn c565(c: u32) -> u16 {
    ((c & 0xF8) << 8 | (c >> 5 & 0x7E0) | (c >> 19 & 0x1F)) as u16
}

fn pack(r: u32, g: u32, b: u32) -> u16 {
    ((r & 0xF8) << 8 | (g & 0xFC) << 3 | b >> 3) as u16
}

/// RGB565 widened to a displayed pixel (R8G8B8A8, opaque).
fn expand(p: u16) -> u32 {
    let (r, g, b) = ((p >> 11) as u32 & 31, (p >> 5) as u32 & 63, p as u32 & 31);
    0xFF00_0000 | (b << 3 | b >> 2) << 16 | (g << 2 | g >> 4) << 8 | (r << 3 | r >> 2)
}

/// Coverage `a` over coverage `d` (premultiplied over).
fn over_a(d: u8, a: u32) -> u8 {
    (a + (d as u32 * (255 - a) + 127) / 255) as u8
}

/// `c` (A B G R) over `d`, `a` of 255 opaque.
fn blend(d: u16, c: u32, a: u32) -> u16 {
    let (r, g, b) = ((d >> 11) as u32 & 31, (d >> 5) as u32 & 63, d as u32 & 31);
    let (r, g, b) = (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2);
    let m = |d: u32, s: u32| (d * (255 - a) + s * a + 127) / 255;
    pack(m(r, c & 0xFF), m(g, c >> 8 & 0xFF), m(b, c >> 16 & 0xFF))
}

/// Coverage (0..=255) of pixel (px, py) of a w x h box with corners of
/// radius `r`: distances in eighths of a pixel from the pixel's centre.
fn rr_cov(px: usize, py: usize, w: usize, h: usize, r: usize) -> u32 {
    let cx = if px < r {
        r
    } else if px + r >= w {
        w - r
    } else {
        return 255;
    };
    let cy = if py < r {
        r
    } else if py + r >= h {
        h - r
    } else {
        return 255;
    };
    let (ex, ey) = (
        (2 * px as i32 + 1 - 2 * cx as i32) * 4,
        (2 * py as i32 + 1 - 2 * cy as i32) * 4,
    );
    let d = isqrt32((ex * ex + ey * ey) as u32) as i32;
    ((r as i32 * 8 - d + 4).clamp(0, 8) * 255 / 8) as u32
}

pub struct Screen {
    pub fb: SharedMemory,
    /// Frame being drawn, and what the displayed image holds; both in
    /// panel scan order, RGB565.
    back: Vec<u16>,
    shown: Vec<u16>,
    /// The displayed image is not what `shown` says: copy it whole.
    all: bool,
    /// Image size, in panel scan order.
    w: usize,
    h: usize,
    /// Added to every x of the drawing calls (a card that is partly off
    /// the screen is drawn in its own coordinates); clipped to the screen.
    pub dx: i32,
    /// Portrait panel: landscape view turned 90 degrees counterclockwise.
    rot: bool,
    /// Overlay mode (over a game): each pixel's coverage, drawn and
    /// displayed; the colors are then premultiplied. Empty otherwise.
    alpha: Vec<u8>,
    ashown: Vec<u8>,
}

impl Screen {
    pub fn new(fb: SharedMemory, w: usize, h: usize) -> Self {
        Self {
            fb,
            back: vec![0; w * h],
            shown: vec![0; w * h],
            all: true,
            w,
            h,
            dx: 0,
            rot: h > w,
            alpha: Vec::new(),
            ashown: Vec::new(),
        }
    }

    /// Overlay mode on (pixels carry their coverage, the frame starts
    /// transparent) or off (opaque, as the UI); the next frame is copied whole.
    pub fn set_overlay(&mut self, on: bool) {
        if on == !self.alpha.is_empty() {
            return;
        }
        let n = self.w * self.h;
        (self.alpha, self.ashown) = if on {
            (vec![0; n], vec![0; n])
        } else {
            (Vec::new(), Vec::new())
        };
        self.all = true;
    }

    /// Nothing drawn: every pixel transparent (overlay mode).
    pub fn clear_clear(&mut self) {
        self.back.fill(0);
        self.alpha.fill(0);
    }

    /// The image rectangle (x, y, w, h, panel scan order) holding every
    /// pixel not transparent; `None` if there is none or not in overlay mode.
    pub fn ink(&self) -> Option<(usize, usize, usize, usize)> {
        if self.alpha.is_empty() {
            return None;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (self.w, 0, self.h, 0);
        for (r, row) in self.alpha.chunks_exact(self.w).enumerate() {
            let Some(f) = row.iter().position(|&a| a != 0) else {
                continue;
            };
            let l = row.iter().rposition(|&a| a != 0).unwrap_or(f);
            (x0, x1) = (x0.min(f), x1.max(l));
            (y0, y1) = (y0.min(r), r);
        }
        (y0 <= y1 && y0 < self.h).then(|| (x0, y0, x1 + 1 - x0, y1 + 1 - y0))
    }

    /// Image size, panel scan order.
    pub fn image_size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// The displayed pixel at image (x, y): R8G8B8A8, premultiplied in
    /// overlay mode.
    pub fn shown_at(&self, x: usize, y: usize) -> u32 {
        let i = y * self.w + x;
        let a = self.ashown.get(i).map_or(255, |&a| a as u32);
        expand(self.shown[i]) & 0x00FF_FFFF | a << 24
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
        let (iw, ih, c) = (self.w, self.h, c565(c));
        let ov = !self.alpha.is_empty();
        if self.rot {
            // Landscape (x, y) is image pixel (row ih-1-x, column y).
            for r in ih - x - w..ih - x {
                self.back[r * iw + y..r * iw + y + h].fill(c);
                if ov {
                    self.alpha[r * iw + y..r * iw + y + h].fill(255);
                }
            }
        } else {
            for r in y..y + h {
                self.back[r * iw + x..r * iw + x + w].fill(c);
                if ov {
                    self.alpha[r * iw + x..r * iw + x + w].fill(255);
                }
            }
        }
    }

    pub fn clear(&mut self, c: u32) {
        self.back.fill(c565(c));
        self.alpha.fill(255);
    }

    /// One pixel at landscape (x, y) (the shift `dx` applied), `c` over
    /// what is there, `a` of 255 opaque; clipped.
    fn plot(&mut self, x: i32, y: i32, c: u32, a: u32) {
        let (sw, sh) = self.size();
        let x = x + self.dx;
        if x < 0 || y < 0 || x >= sw as i32 || y >= sh as i32 {
            return;
        }
        let i = if self.rot {
            (self.h - 1 - x as usize) * self.w + y as usize
        } else {
            y as usize * self.w + x as usize
        };
        self.back[i] = if a >= 255 {
            c565(c)
        } else {
            blend(self.back[i], c, a)
        };
        if let Some(d) = self.alpha.get_mut(i) {
            *d = over_a(*d, a.min(255));
        }
    }

    /// `c` over what is there, `a` of 255 opaque.
    pub fn fill_alpha(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32, a: usize) {
        if a >= 255 {
            return self.fill(x, y, w, h, c);
        }
        let (sw, sh) = self.size();
        let x0 = (x as i32 + self.dx).max(0);
        let x1 = (x as i32 + self.dx + w as i32).min(sw as i32);
        if x1 <= x0 {
            return;
        }
        let (x, w) = (x0 as usize, (x1 - x0) as usize);
        let (w, h) = (w.min(sw.saturating_sub(x)), h.min(sh.saturating_sub(y)));
        let (iw, ih) = (self.w, self.h);
        let a = a as u32;
        let ov = !self.alpha.is_empty();
        // Over a transparent pixel (the overlay's scrim): no blending.
        let (clear_c, clear_a) = (blend(0, c, a), a as u8);
        let run = |s: &mut Self, from: usize, n: usize| {
            if !ov {
                for p in &mut s.back[from..from + n] {
                    *p = blend(*p, c, a);
                }
                return;
            }
            let (b, al) = (&mut s.back[from..from + n], &mut s.alpha[from..from + n]);
            for (p, d) in b.iter_mut().zip(al.iter_mut()) {
                if *d == 0 {
                    (*p, *d) = (clear_c, clear_a);
                } else {
                    (*p, *d) = (blend(*p, c, a), over_a(*d, a));
                }
            }
        };
        if self.rot {
            for lx in x..x + w {
                run(self, (ih - 1 - lx) * iw + y, h);
            }
        } else {
            for ly in y..y + h {
                run(self, ly * iw + x, w);
            }
        }
    }

    /// Rectangle with antialiased corners of radius `r`.
    pub fn fill_round(&mut self, x: usize, y: usize, w: usize, h: usize, r: usize, c: u32) {
        self.round_a(x, y, w, h, r, c, 255);
    }

    /// Rounded rectangle, `a` of 255 opaque.
    #[allow(clippy::too_many_arguments)]
    pub fn round_a(&mut self, x: usize, y: usize, w: usize, h: usize, r: usize, c: u32, a: u32) {
        let r = r.min(w / 2).min(h / 2);
        self.fill_alpha(x, y + r, w, h - 2 * r, c, a as usize);
        if r == 0 {
            return;
        }
        self.fill_alpha(x + r, y, w - 2 * r, r, c, a as usize);
        self.fill_alpha(x + r, y + h - r, w - 2 * r, r, c, a as usize);
        let (x, y) = (x as i32, y as i32);
        let (w, h) = (w as i32, h as i32);
        for py in 0..r {
            for px in 0..r {
                let cov = rr_cov(px, py, 2 * r, 2 * r, r) * a / 255;
                if cov == 0 {
                    continue;
                }
                let (px, py) = (px as i32, py as i32);
                self.plot(x + px, y + py, c, cov);
                self.plot(x + w - 1 - px, y + py, c, cov);
                self.plot(x + px, y + h - 1 - py, c, cov);
                self.plot(x + w - 1 - px, y + h - 1 - py, c, cov);
            }
        }
    }

    /// Outline only, `t` thick, over what is there.
    #[allow(clippy::too_many_arguments)]
    pub fn ring_round(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        r: usize,
        t: usize,
        c: u32,
    ) {
        let r = r.min(w / 2).min(h / 2);
        let ri = r.saturating_sub(t);
        let (iw, ih) = (w.saturating_sub(2 * t), h.saturating_sub(2 * t));
        for py in 0..h {
            let thin = py >= t + ri && py + t + ri < h;
            let band = |s: &mut Self, from: usize, to: usize| {
                for px in from..to {
                    let out = rr_cov(px, py, w, h, r);
                    let inn = if px < t || py < t || px + t >= w || py + t >= h {
                        0
                    } else {
                        rr_cov(px - t, py - t, iw, ih, ri)
                    };
                    let a = out * (255 - inn) / 255;
                    if a > 0 {
                        s.plot((x + px) as i32, (y + py) as i32, c, a);
                    }
                }
            };
            if thin {
                band(self, 0, t);
                band(self, w - t, w);
            } else {
                band(self, 0, w);
            }
        }
    }

    /// Soft shadow under a floating sheet or toast.
    pub fn shadow(&mut self, x: usize, y: usize, w: usize, h: usize, r: usize, c: u32) {
        self.round_a(x, y + 3, w, h + 2, r + 2, c, 20);
        self.round_a(x + 1, y + 1, w.saturating_sub(2), h, r, c, 24);
    }

    /// Disc of radius `r` centered on (cx, cy).
    pub fn disc(&mut self, cx: usize, cy: usize, r: usize, c: u32) {
        self.fill_round(cx - r, cy - r, 2 * r, 2 * r, r, c);
    }

    /// Circle outline `t` thick.
    pub fn ring(&mut self, cx: usize, cy: usize, r: usize, t: usize, c: u32) {
        self.ring_round(cx - r, cy - r, 2 * r, 2 * r, r, t, c);
    }

    /// The upper quarter of a ring (between the diagonals), as the Wi-Fi
    /// icon's waves.
    pub fn arc_up(&mut self, cx: usize, cy: usize, r: usize, t: usize, c: u32) {
        let (d, ri) = (2 * r, r.saturating_sub(t));
        for py in 0..r {
            for px in 0..d {
                let (dx, dy) = (
                    (2 * px as i32 + 1 - d as i32).abs(),
                    d as i32 - 1 - 2 * py as i32,
                );
                if dx > dy {
                    continue;
                }
                let out = rr_cov(px, py, d, d, r);
                let inn = if px < t || py < t || px + t >= d {
                    0
                } else {
                    rr_cov(px - t, py - t, d - 2 * t, d - 2 * t, ri)
                };
                let a = out * (255 - inn) / 255;
                if a > 0 {
                    self.plot((cx - r + px) as i32, (cy - r + py) as i32, c, a);
                }
            }
        }
    }

    /// Antialiased segment `t` thick with round ends, between pixel
    /// centres.
    #[allow(clippy::too_many_arguments)]
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, t: i32, c: u32) {
        let (ax, ay, bx, by) = (x0 * 8 + 4, y0 * 8 + 4, x1 * 8 + 4, y1 * 8 + 4);
        let (dx, dy) = ((bx - ax) as i64, (by - ay) as i64);
        let den = dx * dx + dy * dy;
        let m = t / 2 + 1;
        for py in y0.min(y1) - m..=y0.max(y1) + m {
            for px in x0.min(x1) - m..=x0.max(x1) + m {
                let (qx, qy) = ((px * 8 + 4 - ax) as i64, (py * 8 + 4 - ay) as i64);
                let u = if den == 0 {
                    0
                } else {
                    (qx * dx + qy * dy).clamp(0, den)
                };
                let (ex, ey) = if den == 0 {
                    (qx, qy)
                } else {
                    (qx - dx * u / den, qy - dy * u / den)
                };
                let d = isqrt32((ex * ex + ey * ey) as u32) as i32;
                let cov = ((t * 4 - d + 4).clamp(0, 8) * 255 / 8) as u32;
                if cov > 0 {
                    self.plot(px, py, c, cov);
                }
            }
        }
    }

    /// Text from (x, y), the top of its line, in `f`; returns its width.
    pub fn text(&mut self, x: usize, y: usize, s: &str, f: Face, c: u32) -> usize {
        let base = (y + font::ascent(f)) as i32;
        let mut pen = x as i32;
        for ch in s.chars() {
            let g = font::glyph(f, ch);
            for gy in 0..g.h {
                for gx in 0..g.w {
                    let a = g.alpha(gx, gy);
                    if a != 0 {
                        self.plot(pen + g.xo + gx as i32, base - g.yo + gy as i32, c, a * 17);
                    }
                }
            }
            pen += g.adv as i32;
        }
        (pen - x as i32) as usize
    }

    /// Text from `x` with its capitals centered on `cy`; returns its width.
    pub fn label(&mut self, x: usize, cy: usize, s: &str, f: Face, c: u32) -> usize {
        let top = (cy + font::cap(f).div_ceil(2)).saturating_sub(font::ascent(f));
        self.text(x, top, s, f, c)
    }

    /// `label` ending at `xr`.
    pub fn label_r(&mut self, xr: usize, cy: usize, s: &str, f: Face, c: u32) -> usize {
        self.label(xr.saturating_sub(width(f, s)), cy, s, f, c)
    }

    /// `label` centered on `cx`.
    pub fn label_c(&mut self, cx: usize, cy: usize, s: &str, f: Face, c: u32) -> usize {
        self.label(cx.saturating_sub(width(f, s) / 2), cy, s, f, c)
    }

    /// Terminus glyph `ch` (Latin-1) at landscape pixel (x, y), `scale`
    /// times enlarged, on `bg`.
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

    /// Terminus text from (x, y), a character outside Latin-1 as '?';
    /// returns its width.
    pub fn mono(&mut self, x: usize, y: usize, s: &str, scale: usize, fg: u32, bg: u32) -> usize {
        let mut k = 0;
        for ch in s.chars() {
            let b = u8::try_from(ch as u32).unwrap_or(b'?');
            self.glyph(x + 8 * scale * k, y, b, scale, fg, bg);
            k += 1;
        }
        8 * scale * k
    }

    /// Terminus text ending at `xr`.
    pub fn mono_r(
        &mut self,
        xr: usize,
        y: usize,
        s: &str,
        scale: usize,
        fg: u32,
        bg: u32,
    ) -> usize {
        self.mono(
            xr.saturating_sub(s.chars().count() * 8 * scale),
            y,
            s,
            scale,
            fg,
            bg,
        )
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

    /// `img` scaled to (x0, y0, w, h), corners rounded to `r`.
    #[allow(clippy::too_many_arguments)]
    pub fn image(&mut self, img: &Image, x0: usize, y0: usize, w: usize, h: usize, r: usize) {
        let r = r.min(w / 2).min(h / 2);
        for py in 0..h {
            let sy = (py * img.h / h).min(img.h - 1);
            for px in 0..w {
                let cov = rr_cov(px, py, w, h, r);
                if cov == 0 {
                    continue;
                }
                let sx = (px * img.w / w).min(img.w - 1);
                let (x, y) = (x0 as i32 + px as i32, y0 as i32 + py as i32);
                self.plot(x, y, expand(img.at(sx, sy)), cov);
            }
        }
    }

    /// Black from transparent at the top to `a` of 255 at the bottom of
    /// (x, y, w, h): text over art reads on it.
    pub fn fade(&mut self, x: usize, y: usize, w: usize, h: usize, a: usize) {
        for r in 0..h {
            self.fill_alpha(x, y + r, w, 1, 0xFF00_0000, a * r / h.max(1));
        }
    }

    /// Copy the pixels that changed since the last call to the displayed
    /// image: per image row, the span from the first to the last change.
    pub fn present(&mut self) {
        let fb = unsafe {
            core::slice::from_raw_parts_mut(self.fb.as_ptr() as *mut u32, self.w * self.h)
        };
        let all = core::mem::take(&mut self.all);
        if !self.alpha.is_empty() {
            return self.present_overlay(fb, all);
        }
        for r in 0..self.h {
            let row = r * self.w..(r + 1) * self.w;
            let (b, s) = (&self.back[row.clone()], &mut self.shown[row.clone()]);
            let (first, last) = if all {
                (0, self.w - 1)
            } else {
                let Some(first) = b.iter().zip(s.iter()).position(|(a, c)| a != c) else {
                    continue;
                };
                let last = b
                    .iter()
                    .zip(s.iter())
                    .rposition(|(a, c)| a != c)
                    .unwrap_or(first);
                (first, last)
            };
            s[first..=last].copy_from_slice(&b[first..=last]);
            for (d, &p) in fb[row.start + first..=row.start + last]
                .iter_mut()
                .zip(&b[first..=last])
            {
                *d = expand(p);
            }
        }
    }

    /// `present` with each pixel's coverage as its alpha: a pixel changed
    /// if its color or its coverage did. Rows compared 8 bytes at a time
    /// (`memcmp` is bytewise here), pixels stored 4 at a time: the image
    /// is uncached.
    fn present_overlay(&mut self, fb: &mut [u32], all: bool) {
        for r in 0..self.h {
            let row = r * self.w..(r + 1) * self.w;
            let (b, s) = (&self.back[row.clone()], &mut self.shown[row.clone()]);
            let (a, sa) = (&self.alpha[row.clone()], &mut self.ashown[row.clone()]);
            let (first, last) = if all {
                (0, self.w - 1)
            } else {
                let (b8, s8) = (bytes(b), bytes(s));
                let Some(f) = min_opt(first_diff(b8, s8).map(|k| k / 2), first_diff(a, sa)) else {
                    continue;
                };
                let l = max_opt(last_diff(b8, s8).map(|k| k / 2), last_diff(a, sa)).unwrap_or(f);
                (f, l)
            };
            s[first..=last].copy_from_slice(&b[first..=last]);
            sa[first..=last].copy_from_slice(&a[first..=last]);
            let px = |k: usize| expand(b[k]) & 0x00FF_FFFF | (a[k] as u32) << 24;
            let out = &mut fb[row.start + first..=row.start + last];
            let mut k = first;
            let (quads, rest) = out.as_chunks_mut::<4>();
            for q in quads {
                let v = [px(k), px(k + 1), px(k + 2), px(k + 3)];
                // One 16-byte store: the write buffer sends whole bursts.
                unsafe { (q as *mut [u32; 4]).write_unaligned(v) };
                k += 4;
            }
            for d in rest {
                *d = px(k);
                k += 1;
            }
        }
    }

    /// A client drew on the image itself: the next frame is copied whole.
    pub fn forget(&mut self) {
        self.all = true;
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

/// A row of pixels as bytes.
fn bytes(v: &[u16]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) }
}

fn word(v: &[u8], i: usize) -> u64 {
    u64::from_ne_bytes(v[i..i + 8].try_into().unwrap_or([0; 8]))
}

/// The first byte where `a` and `b` (as long) differ.
fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n && word(a, i) == word(b, i) {
        i += 8;
    }
    (i..n).find(|&k| a[k] != b[k])
}

/// The last byte where `a` and `b` (as long) differ.
fn last_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    let mut n = a.len().min(b.len());
    while n >= 8 && word(a, n - 8) == word(b, n - 8) {
        n -= 8;
    }
    (0..n).rev().find(|&k| a[k] != b[k])
}

fn min_opt(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

fn max_opt(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// Integer square root.
fn isqrt32(mut n: u32) -> u32 {
    let (mut r, mut bit) = (0u32, 1u32 << 30);
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if n >= r + bit {
            n -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}

/// `a` towards `b`, `t` of 255 of the way, per channel (A B G R).
pub fn mix(a: u32, b: u32, t: usize) -> u32 {
    let t = t.min(255) as u32;
    let ch = |s: u32| ((a >> s & 0xFF) * (255 - t) + (b >> s & 0xFF) * t) / 255;
    0xFF00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0)
}

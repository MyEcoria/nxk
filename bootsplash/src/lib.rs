//! The boot screen: the nxk logo and a progress bar on the dark theme's
//! background. nxk-bl31, the kernel's screen console and `display` all
//! draw it from here, so the picture does not move between them.
//!
//! Everything is rectangles in the landscape view (1280x720 on a Switch),
//! handed to a `fill(x, y, w, h, color)` callback; `turn` maps them onto a
//! portrait panel's image. Colors are A B G R words (R8G8B8A8 in memory),
//! already rounded through RGB565, as `display` shows them. Only `u32`
//! arithmetic: it also builds for the BPMP (`thumbv4t-none-eabi`).

#![no_std]

/// `tools/mklogo.py --raw`: width, height, 8 R G B A palette entries, indices.
const LOGO: &[u8] = include_bytes!("../../build/logo.bin");
const LOGO_PIXELS: usize = 34;

/// Background, the bar's track and its filled part (dark theme: bg, rule, accent).
pub const BG: u32 = exact(rgb(0x1C1C1C));
pub const TRACK: u32 = exact(rgb(0x444444));
pub const FILL: u32 = exact(rgb(0x3BC3E3));

/// Progress, in thousandths, each stage starts at.
pub const AT_IPL: u32 = 50;
pub const AT_LOADED: u32 = 100;
pub const AT_MONITOR: u32 = 150;
pub const AT_KERNEL: u32 = 300;
pub const AT_INIT: u32 = 450;
pub const AT_DISPLAY: u32 = 600;
pub const DONE: u32 = 1000;

/// `0xRRGGBB` as an opaque A B G R word.
pub const fn rgb(c: u32) -> u32 {
    0xFF00_0000 | (c & 0xFF) << 16 | (c >> 8 & 0xFF) << 8 | c >> 16
}

/// `c` as `display` shows it: through RGB565 and widened back.
pub const fn exact(c: u32) -> u32 {
    let (r, g, b) = ((c & 0xFF) >> 3, (c >> 8 & 0xFF) >> 2, (c >> 16 & 0xFF) >> 3);
    0xFF00_0000 | (b << 3 | b >> 2) << 16 | (g << 2 | g >> 4) << 8 | (r << 3 | r >> 2)
}

/// Pixel formats of window A: hekate's image and `display`'s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// B8G8R8A8 (`WIN_COLOR_DEPTH` 12).
    Bgra,
    /// R8G8B8A8 (`WIN_COLOR_DEPTH` 13).
    Rgba,
}

impl Format {
    /// The format of a `WIN_COLOR_DEPTH` value, if 32-bit.
    pub fn from_depth(d: u32) -> Option<Self> {
        match d & 0x7F {
            12 => Some(Self::Bgra),
            13 => Some(Self::Rgba),
            _ => None,
        }
    }

    /// The word stored for color `c` (A B G R).
    pub const fn pixel(self, c: u32) -> u32 {
        match self {
            Self::Rgba => c,
            Self::Bgra => c & 0xFF00_FF00 | (c & 0xFF) << 16 | (c >> 16 & 0xFF),
        }
    }
}

/// Landscape rectangle `(x, y, w, h)` as an image rectangle on a portrait
/// panel `ih` rows high, turned as `display` draws: landscape (x, y) is
/// image pixel (column y, row ih-1-x).
pub const fn turn(x: u32, y: u32, w: u32, h: u32, ih: u32) -> (u32, u32, u32, u32) {
    (y, ih - x - w, h, w)
}

/// The boot screen's layout for a landscape view of `vw` x `vh` pixels.
#[derive(Clone, Copy, Debug)]
pub struct Splash {
    vw: u32,
    vh: u32,
    scale: u32,
    lx: u32,
    ly: u32,
    bx: u32,
    by: u32,
    bw: u32,
    bh: u32,
}

impl Splash {
    pub fn new(vw: u32, vh: u32) -> Self {
        let (w, h) = logo_size();
        let scale = if vw >= 1024 { 8 } else { 4 };
        let (lw, lh) = (w * scale, h * scale);
        let ly = (vh * 45 / 100).saturating_sub(lh / 2);
        let bw = vw / 4;
        Self {
            vw,
            vh,
            scale,
            lx: vw.saturating_sub(lw) / 2,
            ly,
            bx: vw.saturating_sub(bw) / 2,
            by: (ly + lh + 12 * scale).min(vh.saturating_sub(16)),
            bw,
            bh: scale * 3 / 4,
        }
    }

    /// The whole screen at progress `p` (thousandths).
    pub fn draw(&self, p: u32, fill: &mut impl FnMut(u32, u32, u32, u32, u32)) {
        fill(0, 0, self.vw, self.vh, BG);
        self.logo(fill);
        self.bar(p, fill);
    }

    pub fn logo(&self, fill: &mut impl FnMut(u32, u32, u32, u32, u32)) {
        let (w, h) = logo_size();
        let s = self.scale;
        for y in 0..h {
            // Runs of one color in a row: fewer, longer fills.
            let mut x = 0;
            while x < w {
                let i = LOGO[LOGO_PIXELS + (y * w + x) as usize];
                let mut n = 1;
                while x + n < w && LOGO[LOGO_PIXELS + (y * w + x + n) as usize] == i {
                    n += 1;
                }
                if i != 0 {
                    fill(
                        self.lx + x * s,
                        self.ly + y * s,
                        n * s,
                        s,
                        palette(i as usize),
                    );
                }
                x += n;
            }
        }
    }

    /// The bar alone at progress `p`: a stage moves it without a redraw.
    pub fn bar(&self, p: u32, fill: &mut impl FnMut(u32, u32, u32, u32, u32)) {
        let done = self.bw * p.min(DONE) / DONE;
        if done > 0 {
            fill(self.bx, self.by, done, self.bh, FILL);
        }
        if done < self.bw {
            fill(self.bx + done, self.by, self.bw - done, self.bh, TRACK);
        }
    }

    /// The bar's rectangle (landscape).
    pub fn bar_rect(&self) -> (u32, u32, u32, u32) {
        (self.bx, self.by, self.bw, self.bh)
    }
}

fn logo_size() -> (u32, u32) {
    (LOGO[0] as u32, LOGO[1] as u32)
}

/// Logo palette entry `i` (A B G R), rounded as shown.
fn palette(i: usize) -> u32 {
    let e = &LOGO[2 + 4 * i..6 + 4 * i];
    exact(u32::from_le_bytes([e[0], e[1], e[2], e[3]]))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// Landscape image, `display`'s way: every fill into a 1280x720 buffer.
    fn landscape(p: u32) -> Vec<u32> {
        let (w, h) = (1280u32, 720u32);
        let mut img = vec![0u32; (w * h) as usize];
        Splash::new(w, h).draw(p, &mut |x, y, fw, fh, c| {
            for yy in y..y + fh {
                for xx in x..x + fw {
                    img[(yy * w + xx) as usize] = c;
                }
            }
        });
        img
    }

    /// Portrait image, the monitor's way: turned fills, stored in `fmt`.
    fn portrait(p: u32, fmt: Format) -> Vec<u32> {
        let (iw, ih) = (720u32, 1280u32);
        let mut img = vec![0u32; (iw * ih) as usize];
        Splash::new(ih, iw).draw(p, &mut |x, y, w, h, c| {
            let (col, row, cw, rh) = turn(x, y, w, h, ih);
            for r in row..row + rh {
                for k in col..col + cw {
                    img[(r * iw + k) as usize] = fmt.pixel(c);
                }
            }
        });
        img
    }

    #[test]
    fn turned_matches_landscape() {
        for p in [0, AT_MONITOR, AT_DISPLAY, DONE] {
            let (land, port) = (landscape(p), portrait(p, Format::Rgba));
            for y in 0..720u32 {
                for x in 0..1280u32 {
                    let i = (1279 - x) * 720 + y;
                    assert_eq!(
                        land[(y * 1280 + x) as usize],
                        port[i as usize],
                        "({x}, {y}) at {p}"
                    );
                }
            }
        }
    }

    #[test]
    fn bgra_swaps_red_and_blue() {
        assert_eq!(Format::Bgra.pixel(0xFF11_2233), 0xFF33_2211);
        let (a, b) = (
            portrait(AT_MONITOR, Format::Rgba),
            portrait(AT_MONITOR, Format::Bgra),
        );
        assert!(a.iter().zip(&b).all(|(&x, &y)| Format::Bgra.pixel(x) == y));
    }

    #[test]
    fn colors_survive_rgb565() {
        for c in [BG, TRACK, FILL].into_iter().chain((1..8).map(palette)) {
            assert_eq!(exact(c), c);
        }
    }

    #[test]
    fn logo_is_drawn_and_centered() {
        let s = Splash::new(1280, 720);
        let (mut x0, mut x1) = (u32::MAX, 0);
        s.logo(&mut |x, _, w, _, _| {
            x0 = x0.min(x);
            x1 = x1.max(x + w);
        });
        assert!(x0 < x1);
        assert!((x0 as i32 - (1280 - x1) as i32).abs() <= 8 * 2);
    }

    #[test]
    fn bar_fills_from_the_left() {
        let s = Splash::new(1280, 720);
        let (bx, _, bw, _) = s.bar_rect();
        let mut fills = Vec::new();
        s.bar(500, &mut |x, _, w, _, c| fills.push((x, w, c)));
        assert_eq!(
            fills,
            [(bx, bw / 2, FILL), (bx + bw / 2, bw - bw / 2, TRACK)]
        );
    }
}

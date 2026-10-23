//! Covers: Amazon's image servers resize on request (`._SL<side>_` before
//! the extension, a JPEG back); the box art fills the square the home
//! screen shows: scaled until it covers it, the middle kept.

use std::io::Cursor;

use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// Bytes of a cover download at most.
pub const MAX_BYTES: usize = 2 << 20;
/// Pictures wider or taller than this are not decoded.
const MAX_SIDE: usize = 2048;

/// `url` asking for `side` pixels on its longest side; `None` if it is
/// not an `https` file with an extension.
pub fn sized_url(url: &str, side: u32) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let name = url.rfind('/')?;
    let dot = url.rfind('.').filter(|&d| d > name)?;
    Some(format!("{}._SL{}_{}", &url[..dot], side, &url[dot..]))
}

/// The address of the picture a `side` square cover is made from: the
/// portrait art if the game has it (a square cut of it loses the least),
/// else the landscape one, asked for large enough that its short side
/// reaches `side` (3:4 and 16:9 at least).
pub fn source_url(portrait: Option<&str>, landscape: Option<&str>, side: u32) -> Option<String> {
    portrait
        .and_then(|u| sized_url(u, side * 3 / 2))
        .or_else(|| landscape.and_then(|u| sized_url(u, side * 16 / 9)))
}

/// An RGB picture, 3 bytes a pixel, rows from the top.
pub struct Picture {
    pub w: usize,
    pub h: usize,
    pub rgb: Vec<u8>,
}

impl Picture {
    fn at(&self, x: usize, y: usize) -> [u32; 3] {
        let i = (y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 3;
        [
            self.rgb[i] as u32,
            self.rgb[i + 1] as u32,
            self.rgb[i + 2] as u32,
        ]
    }

    /// The mean of the area `[x0, x1) x [y0, y1)` (at least one pixel).
    fn mean(&self, x0: usize, x1: usize, y0: usize, y1: usize) -> [u32; 3] {
        let (x1, y1) = (x1.max(x0 + 1), y1.max(y0 + 1));
        let mut s = [0u32; 3];
        for y in y0..y1 {
            for x in x0..x1 {
                let p = self.at(x, y);
                (0..3).for_each(|k| s[k] += p[k]);
            }
        }
        let n = ((x1 - x0) * (y1 - y0)) as u32;
        s.map(|v| v / n)
    }

    /// The color at (`fx`, `fy`) in 1/256 pixels, between the four pixels
    /// around it.
    fn bilinear(&self, fx: usize, fy: usize) -> [u32; 3] {
        let (x0, y0, tx, ty) = (fx >> 8, fy >> 8, (fx & 255) as u32, (fy & 255) as u32);
        let (a, b) = (self.at(x0, y0), self.at(x0 + 1, y0));
        let (c, d) = (self.at(x0, y0 + 1), self.at(x0 + 1, y0 + 1));
        let mut o = [0u32; 3];
        for (k, v) in o.iter_mut().enumerate() {
            let top = a[k] * (256 - tx) + b[k] * tx;
            let bottom = c[k] * (256 - tx) + d[k] * tx;
            *v = (top * (256 - ty) + bottom * ty) >> 16;
        }
        o
    }
}

/// A JPEG's pixels; `None` if it does not decode or is too large.
pub fn decode(jpeg: &[u8]) -> Option<Picture> {
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(MAX_SIDE)
        .set_max_height(MAX_SIDE);
    let mut d = JpegDecoder::new_with_options(Cursor::new(jpeg), opts);
    let rgb = d.decode().ok()?;
    let info = d.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    (w > 0 && h > 0 && rgb.len() == w * h * 3).then_some(Picture { w, h, rgb })
}

/// `p` scaled to cover a `side` square, its middle kept (a portrait loses
/// its top and bottom, a landscape its sides): 0xRRGGBB pixels, rows from
/// the top. Area means going down in size, bilinear going up.
pub fn square(p: &Picture, side: usize) -> Vec<u32> {
    // The centered square of the picture, `s` pixels a side.
    let s = p.w.min(p.h);
    let (left, top) = ((p.w - s) / 2, (p.h - s) / 2);
    let mut out = Vec::with_capacity(side * side);
    for y in 0..side {
        for x in 0..side {
            let c = if s >= side {
                p.mean(
                    left + x * s / side,
                    left + (x + 1) * s / side,
                    top + y * s / side,
                    top + (y + 1) * s / side,
                )
            } else {
                // Pixel centers: output pixel `u` is at (u + 1/2) / side of the way.
                let along = |u: usize| ((2 * u + 1) * s * 256 / (2 * side)).saturating_sub(128);
                let max = (s - 1) * 256;
                p.bilinear(
                    (left << 8) + along(x).min(max),
                    (top << 8) + along(y).min(max),
                )
            };
            out.push(c[0] << 16 | c[1] << 8 | c[2]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_go_before_the_extension() {
        assert_eq!(
            sized_url("https://m.media-amazon.com/images/I/81wx.png", 256).as_deref(),
            Some("https://m.media-amazon.com/images/I/81wx._SL256_.png")
        );
        assert_eq!(sized_url("http://example.com/a.png", 256), None);
        assert_eq!(sized_url("https://example.com/noext", 256), None);
    }

    #[test]
    fn the_portrait_art_is_asked_for_first_and_large_enough() {
        let (p, l) = (Some("https://x.com/p.png"), Some("https://x.com/l.png"));
        // 3:4 art at 384 is 288 wide; 16:9 art at 455 is 256 high.
        assert_eq!(
            source_url(p, l, 256).as_deref(),
            Some("https://x.com/p._SL384_.png")
        );
        assert_eq!(
            source_url(None, l, 256).as_deref(),
            Some("https://x.com/l._SL455_.png")
        );
        assert_eq!(source_url(None, None, 256), None);
        assert_eq!(source_url(Some("http://x.com/p.png"), None, 256), None);
    }

    /// A picture of `w` x `h` whose pixels are `rgb(x, y)`.
    fn made(w: usize, h: usize, rgb: impl Fn(usize, usize) -> [u8; 3]) -> Picture {
        let rgb = (0..w * h).flat_map(|i| rgb(i % w, i / w)).collect();
        Picture { w, h, rgb }
    }

    const RED: u32 = 255 << 16;
    const GREEN: u32 = 255 << 8;
    const BLUE: u32 = 255;

    #[test]
    fn a_portrait_loses_its_top_and_bottom() {
        // 20 x 30: 5 red rows, 20 green rows (the middle square), 5 blue rows.
        let p = made(20, 30, |_, y| match y {
            0..=4 => [255, 0, 0],
            5..=24 => [0, 255, 0],
            _ => [0, 0, 255],
        });
        for side in [10, 20, 40] {
            let s = square(&p, side);
            assert_eq!(s.len(), side * side);
            assert!(s.iter().all(|&c| c == GREEN), "side {}", side);
        }
    }

    #[test]
    fn a_landscape_loses_its_sides() {
        let p = made(40, 20, |x, _| match x {
            0..=9 => [255, 0, 0],
            10..=29 => [0, 255, 0],
            _ => [0, 0, 255],
        });
        for side in [10, 20, 40] {
            assert!(
                square(&p, side).iter().all(|&c| c == GREEN),
                "side {}",
                side
            );
        }
        assert_ne!(RED, BLUE);
    }

    #[test]
    fn going_up_in_size_blends_smoothly() {
        // Left column black, right column white, blown up to 4 pixels.
        let p = made(2, 2, |x, _| [x as u8 * 255; 3]);
        let row: Vec<u32> = square(&p, 4)[..4].iter().map(|c| c & 255).collect();
        assert_eq!((row[0], row[3]), (0, 255));
        assert!(row.windows(2).all(|w| w[0] < w[1]), "{:?}", row);
    }

    #[test]
    fn a_real_jpeg_decodes() {
        let p = decode(include_bytes!("../tests/sample.jpg")).expect("decodes");
        assert_eq!((p.w, p.h), (24, 36));
        assert!(decode(b"not a jpeg").is_none());
    }

    #[test]
    fn a_progressive_jpeg_decodes_and_fills_a_cover() {
        // What Amazon sends for the larger sizes.
        let p = decode(include_bytes!("../tests/progressive.jpg")).expect("decodes");
        assert_eq!((p.w, p.h), (96, 128));
        let s = square(&p, 64);
        let rgb = |x: usize, y: usize| {
            [
                s[y * 64 + x] >> 16 & 255,
                s[y * 64 + x] >> 8 & 255,
                s[y * 64 + x] & 255,
            ]
        };
        // The 96 x 96 middle of the picture, shrunk by 2/3: the yellow
        // band is at row 16, the red disc at row 49.
        let (yellow, red) = (rgb(32, 16), rgb(33, 49));
        assert!(
            yellow[0] > 200 && yellow[1] > 150 && yellow[2] < 100,
            "{:?}",
            yellow
        );
        assert!(red[0] > 150 && red[1] < 100 && red[2] < 100, "{:?}", red);
    }
}

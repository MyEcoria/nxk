//! Covers: Amazon's image servers resize on request (`._SL<side>_` before
//! the extension, a JPEG back); the box art is fitted into the square the
//! home screen shows, on a blurred, darker copy of itself.

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

/// `p` fitted into a `side` square (ratio kept, by area means), on itself
/// stretched, blurred and darkened: 0xRRGGBB pixels, rows from the top.
pub fn square(p: &Picture, side: usize) -> Vec<u32> {
    const GRID: usize = 8;
    let cells: Vec<[u32; 3]> = (0..GRID * GRID)
        .map(|i| {
            let (cx, cy) = (i % GRID, i / GRID);
            p.mean(
                cx * p.w / GRID,
                (cx + 1) * p.w / GRID,
                cy * p.h / GRID,
                (cy + 1) * p.h / GRID,
            )
        })
        .collect();
    let back = |x: usize, y: usize| -> [u32; 3] {
        // Bilinear between the cell means: a blur.
        let fx = (x * GRID * 256 / side).saturating_sub(128) as u32;
        let fy = (y * GRID * 256 / side).saturating_sub(128) as u32;
        let (x0, y0) = ((fx >> 8) as usize, (fy >> 8) as usize);
        let (x1, y1) = ((x0 + 1).min(GRID - 1), (y0 + 1).min(GRID - 1));
        let (tx, ty) = (fx & 255, fy & 255);
        let c = |cx: usize, cy: usize| cells[cy * GRID + cx];
        let mut o = [0u32; 3];
        for (k, v) in o.iter_mut().enumerate() {
            let top = c(x0, y0)[k] * (256 - tx) + c(x1, y0)[k] * tx;
            let bottom = c(x0, y1)[k] * (256 - tx) + c(x1, y1)[k] * tx;
            *v = (top * (256 - ty) + bottom * ty) >> 16;
        }
        o.map(|v| v * 3 / 5)
    };
    // The fitted picture's size and place.
    let (fw, fh) = if p.w >= p.h {
        (side, (p.h * side / p.w).max(1))
    } else {
        ((p.w * side / p.h).max(1), side)
    };
    let (ox, oy) = ((side - fw) / 2, (side - fh) / 2);
    let mut out = Vec::with_capacity(side * side);
    for y in 0..side {
        for x in 0..side {
            let inside = (ox..ox + fw).contains(&x) && (oy..oy + fh).contains(&y);
            let c = if inside {
                let (u, v) = (x - ox, y - oy);
                p.mean(
                    u * p.w / fw,
                    (u + 1) * p.w / fw,
                    v * p.h / fh,
                    (v + 1) * p.h / fh,
                )
            } else {
                back(x, y)
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

    fn picture(w: usize, h: usize, rgb: [u8; 3]) -> Picture {
        Picture {
            w,
            h,
            rgb: rgb.iter().copied().cycle().take(w * h * 3).collect(),
        }
    }

    #[test]
    fn a_portrait_is_centered_on_its_darker_self() {
        let p = picture(20, 30, [200, 100, 50]);
        let s = square(&p, 30);
        assert_eq!(s.len(), 900);
        // Middle: the picture; left edge: the backdrop, 3/5 as bright.
        assert_eq!(s[15 * 30 + 15], 200 << 16 | 100 << 8 | 50);
        assert_eq!(s[15 * 30], 120 << 16 | 60 << 8 | 30);
    }

    #[test]
    fn a_landscape_fills_the_width() {
        let s = square(&picture(40, 20, [10, 20, 30]), 20);
        assert_eq!(s[10 * 20], 10 << 16 | 20 << 8 | 30);
        assert_eq!(s[0], 6 << 16 | 12 << 8 | 18);
    }

    #[test]
    fn a_real_jpeg_decodes() {
        let p = decode(include_bytes!("../tests/sample.jpg")).expect("decodes");
        assert_eq!((p.w, p.h), (24, 36));
        assert!(decode(b"not a jpeg").is_none());
    }
}

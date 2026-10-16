//! 2D drawing on the GPU: rectangles, gradients and text, recorded in
//! landscape pixels (1280 x 720) as clip-space triangles for the flat
//! pipeline. A `Canvas` is filled each frame and handed to `Frame::canvas`.

use alloc::vec::Vec;

use crate::{FlatVertex, HEIGHT, WIDTH};

/// 5 x 7 glyphs: seven rows, bit 4 the leftmost column.
const FONT: &[(char, [u8; 7])] = &[
    ('0', [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E]),
    ('1', [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('2', [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F]),
    ('3', [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E]),
    ('4', [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02]),
    ('5', [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E]),
    ('6', [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E]),
    ('7', [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
    ('8', [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E]),
    ('9', [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C]),
    ('A', [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]),
    ('B', [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E]),
    ('C', [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E]),
    ('D', [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E]),
    ('E', [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F]),
    ('F', [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10]),
    ('G', [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F]),
    ('H', [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11]),
    ('I', [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E]),
    ('J', [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C]),
    ('K', [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11]),
    ('L', [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F]),
    ('M', [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11]),
    ('N', [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11]),
    ('O', [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E]),
    ('P', [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10]),
    ('Q', [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D]),
    ('R', [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11]),
    ('S', [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E]),
    ('T', [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04]),
    ('U', [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E]),
    ('V', [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04]),
    ('W', [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11]),
    ('X', [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11]),
    ('Y', [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04]),
    ('Z', [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F]),
    ('-', [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00]),
    (':', [0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00]),
    ('.', [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C]),
    ('!', [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04]),
    ('/', [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10]),
];

/// Width of a text of `n` characters at `scale` pixels per dot.
pub fn text_width(n: usize, scale: f32) -> f32 {
    n as f32 * 6.0 * scale - scale
}

/// RGBA in 0..1.
pub type Rgba = [f32; 4];

pub fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

pub struct Canvas {
    pub(crate) verts: Vec<FlatVertex>,
    /// Depth of what is drawn next, in clip space (-1 in front, 1 behind
    /// everything): HUDs in front, backgrounds behind.
    pub z: f32,
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas {
    /// A canvas for the front of the scene.
    pub fn new() -> Self {
        Self {
            verts: Vec::new(),
            z: -0.999,
        }
    }

    /// A canvas for the back of the scene.
    pub fn background() -> Self {
        Self {
            verts: Vec::new(),
            z: 0.995,
        }
    }

    pub fn clear(&mut self) {
        self.verts.clear();
    }

    /// Landscape pixel to the clip space the panel scans out (turned a
    /// quarter, see `Graphics::panel`), in front of everything.
    fn clip(&self, x: f32, y: f32) -> [f32; 4] {
        [
            y / (HEIGHT as f32 / 2.0) - 1.0,
            1.0 - x / (WIDTH as f32 / 2.0),
            self.z,
            1.0,
        ]
    }

    fn tri(&mut self, p: [(f32, f32); 3], c: [Rgba; 3]) {
        for k in 0..3 {
            let pos = self.clip(p[k].0, p[k].1);
            self.verts.push(FlatVertex { pos, color: c[k] });
        }
    }

    /// Rectangle with a color at each corner: top-left, top-right,
    /// bottom-right, bottom-left.
    pub fn quad(&mut self, x: f32, y: f32, w: f32, h: f32, c: [Rgba; 4]) {
        let (x1, y1) = (x + w, y + h);
        self.tri([(x, y), (x1, y), (x1, y1)], [c[0], c[1], c[2]]);
        self.tri([(x, y), (x1, y1), (x, y1)], [c[0], c[2], c[3]]);
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
        self.quad(x, y, w, h, [c; 4]);
    }

    /// Vertical gradient from `top` to `bottom`.
    pub fn gradient(&mut self, x: f32, y: f32, w: f32, h: f32, top: Rgba, bottom: Rgba) {
        self.quad(x, y, w, h, [top, top, bottom, bottom]);
    }

    /// Text in capitals (other characters are blank), `scale` pixels per dot.
    pub fn text(&mut self, x: f32, y: f32, scale: f32, s: &str, c: Rgba) {
        let mut cx = x;
        for ch in s.chars() {
            let ch = ch.to_ascii_uppercase();
            if let Some((_, rows)) = FONT.iter().find(|(g, _)| *g == ch) {
                for (r, bits) in rows.iter().enumerate() {
                    // Runs of dots in a row make one rectangle.
                    let mut col = 0;
                    while col < 5 {
                        if bits >> (4 - col) & 1 == 0 {
                            col += 1;
                            continue;
                        }
                        let start = col;
                        while col < 5 && bits >> (4 - col) & 1 != 0 {
                            col += 1;
                        }
                        self.rect(
                            cx + start as f32 * scale,
                            y + r as f32 * scale,
                            (col - start) as f32 * scale,
                            scale,
                            c,
                        );
                    }
                }
            }
            cx += 6.0 * scale;
        }
    }

    /// Text centered on `cx`.
    pub fn text_center(&mut self, cx: f32, y: f32, scale: f32, s: &str, c: Rgba) {
        self.text(cx - text_width(s.len(), scale) / 2.0, y, scale, s, c);
    }

    pub fn len(&self) -> usize {
        self.verts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.verts.is_empty()
    }
}

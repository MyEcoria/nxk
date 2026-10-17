//! Paletted RGB565 images from tools/mkcovers.py: the UI's own (avatar,
//! round buttons, header glyphs), built in, and each game's icon, read
//! from the card. `u8 count`, then per image `u16 w, u16 h, u32 offset`;
//! at the offset 256 RGB565 colors, then one index per pixel.

pub const UI: &[u8] = include_bytes!("../../../build/uiart.bin");
/// In `UI`: the avatar, then the round buttons (`home::BUTTONS`' order)
/// on the dark theme's disc and the light one's, then the same glyphs
/// on each theme's background for the page headers.
pub const AVATAR: usize = 0;
pub const AVATAR_SMALL: usize = 1;
const BUTTON: usize = 2;
const HEAD: usize = 23;
const KINDS: usize = 7;
/// In a game's `cover.bin`: its square icon.
pub const ICON: usize = 0;

/// Dock button `k` (`home::BUTTONS`) for the theme in force (`sel`: on the
/// protruding selected circle).
pub fn button(k: usize, sel: bool) -> usize {
    let bank = if crate::theme::is_light() {
        2
    } else {
        sel as usize
    };
    BUTTON + KINDS * bank + k
}

/// Header glyph `k` (the dock buttons' order) for the theme in force.
pub fn head(k: usize) -> usize {
    HEAD + KINDS * crate::theme::is_light() as usize + k
}

pub struct Image<'a> {
    pub w: usize,
    pub h: usize,
    pal: &'a [u8],
    px: &'a [u8],
}

/// Image `i` of `data`, or `None` if it is not there or cut short.
pub fn get(data: &[u8], i: usize) -> Option<Image<'_>> {
    if i >= *data.first()? as usize {
        return None;
    }
    let rec = data.get(1 + i * 8..9 + i * 8)?;
    let w = u16::from_le_bytes([rec[0], rec[1]]) as usize;
    let h = u16::from_le_bytes([rec[2], rec[3]]) as usize;
    let off = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]) as usize;
    let pal = data.get(off..off + 512)?;
    let px = data.get(off + 512..off + 512 + w * h)?;
    (w > 0 && h > 0).then_some(Image { w, h, pal, px })
}

impl Image<'_> {
    /// RGB565 color of pixel (x, y).
    pub fn at(&self, x: usize, y: usize) -> u16 {
        let i = self.px[y * self.w + x] as usize * 2;
        u16::from_le_bytes([self.pal[i], self.pal[i + 1]])
    }
}

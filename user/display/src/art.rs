//! Paletted RGB565 images from tools/mkcovers.py: the UI's own (app
//! tiles, avatar), built in, and each game's cover, read from the card.
//! `u8 count`, then per image `u16 w, u16 h, u32 offset`; at the offset
//! 256 RGB565 colors, then one index per pixel.

pub const UI: &[u8] = include_bytes!("../../../build/uiart.bin");
/// In `UI`: the tiles first (`home::TILES`' order), then the avatar at 64
/// and 56 px.
pub const AVATAR: usize = 6;
pub const AVATAR_SMALL: usize = 7;
/// In a cover: the selected card's art, then the other cards'.
pub const COVER_BIG: usize = 0;
pub const COVER_SMALL: usize = 1;

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

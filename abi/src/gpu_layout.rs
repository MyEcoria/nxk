//! Block-linear images of the GM20B (render targets and textures): the
//! sizes and byte offsets the `gpu` service and the games must agree on.
//! Pure arithmetic: `shader/src/layout_tests.rs` checks it on the PC
//! against NVK's NIL (`nil_image_init`, `Tiling::clamp`).
//!
//! A GOB is 64 bytes x 8 rows (512 bytes); a block is one GOB wide and
//! `1 << block_log2` GOBs high. Mip levels follow each other, each with
//! the level 0 block height cut down to what the level's height needs
//! (the hardware does the same cut when it samples).

/// Bytes per pixel of a `TF_*` format, 0 if there is no such format.
pub const fn tf_bytes(format: u64) -> u32 {
    match format {
        0 | 1 => 4,
        2 => 8,
        3 => 16,
        _ => 0,
    }
}

/// log2 of `n` rounded up (0 for 0 and 1).
const fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

/// Bytes of a block-linear image `width` x `height` of `bpp`-byte pixels,
/// blocks `1 << block_log2` GOBs high.
pub const fn tiled_bytes_bpp(width: u32, height: u32, bpp: u32, block_log2: u32) -> usize {
    let gobs_x = (width as usize * bpp as usize).div_ceil(64);
    let rows = 8usize << block_log2;
    gobs_x * (height as usize).div_ceil(rows) * (512usize << block_log2)
}

/// Byte offset of pixel (`x`, `y`) in a level `width` pixels wide: the
/// GOB's swizzle is x bits 3-0, y bit 0, x bit 4, y bits 2-1, x bit 5
/// (of the byte offset in the row).
pub const fn tiled_offset(width: u32, x: u32, y: u32, bpp: u32, block_log2: u32) -> usize {
    let xb = x as usize * bpp as usize;
    let (bx, gob_x) = (xb % 64, xb / 64);
    let (gob_row, row) = (y as usize / 8, y as usize % 8);
    let gobs_x = (width as usize * bpp as usize).div_ceil(64);
    let block = (gob_row >> block_log2) * gobs_x + gob_x;
    let in_block = gob_row & ((1 << block_log2) - 1);
    let inner = (bx / 32) * 256 + (row / 2) * 64 + ((bx % 32) / 16) * 32 + (row % 2) * 16 + bx % 16;
    block * (512 << block_log2) + in_block * 512 + inner
}

/// Block height (log2 of GOBs) a level `height` rows high really has
/// when the image's is `block_log2`: no taller than the level needs.
pub const fn level_block_log2(block_log2: u32, height: u32) -> u32 {
    let need = ceil_log2(height.div_ceil(8));
    if block_log2 < need {
        block_log2
    } else {
        need
    }
}

/// Size of level `level` along one axis: halved each level, at least 1.
pub const fn level_extent(v: u32, level: u32) -> u32 {
    let e = v >> level;
    if e == 0 {
        1
    } else {
        e
    }
}

/// Levels of a full chain down to 1 x 1.
pub const fn max_levels(width: u32, height: u32) -> u32 {
    let m = if width > height { width } else { height };
    if m == 0 {
        0
    } else {
        32 - m.leading_zeros()
    }
}

/// Bytes of level `level` of a chain whose level 0 is `width` x `height`,
/// blocks `1 << block_log2` GOBs high at level 0.
pub const fn level_bytes(width: u32, height: u32, bpp: u32, block_log2: u32, level: u32) -> usize {
    let (w, h) = (level_extent(width, level), level_extent(height, level));
    tiled_bytes_bpp(w, h, bpp, level_block_log2(block_log2, h))
}

/// Offset of level `level`: the sizes of the levels before it.
pub const fn level_offset(width: u32, height: u32, bpp: u32, block_log2: u32, level: u32) -> usize {
    let mut at = 0;
    let mut l = 0;
    while l < level {
        at += level_bytes(width, height, bpp, block_log2, l);
        l += 1;
    }
    at
}

/// Bytes of a chain of `levels` levels.
pub const fn mip_bytes(width: u32, height: u32, bpp: u32, block_log2: u32, levels: u32) -> usize {
    level_offset(width, height, bpp, block_log2, levels)
}

//! The block-linear layout the `gpu` service and the games share
//! (`abi/src/gpu_layout.rs`, included here as the abi crate only builds
//! for the console), against NIL's rules for sizes and mip levels.

#[path = "../../abi/src/gpu_layout.rs"]
mod layout;

use layout::*;
use std::collections::HashSet;
use std::vec::Vec;

/// NIL (`nil_image_init`), written the way it reads: byte extents aligned
/// to the level's tile, the tile clamped to the level.
fn nil_level(width: u32, height: u32, bpp: u32, y_log2: u32, level: u32) -> (usize, u32) {
    let (w, h) = ((width >> level).max(1), (height >> level).max(1));
    let width_b = w as usize * bpp as usize;
    let gob_rows = h.div_ceil(8);
    let mut y = y_log2;
    while y > 0 && (1u32 << (y - 1)) >= gob_rows {
        y -= 1;
    }
    let (tile_w, tile_h) = (64usize, 8usize << y);
    let size = width_b.next_multiple_of(tile_w) * (h as usize).next_multiple_of(tile_h);
    (size, y)
}

#[test]
fn sizes_match_nil() {
    for bpp in [4, 8, 16] {
        for (w, h) in [
            (1, 1),
            (8, 8),
            (64, 64),
            (100, 37),
            (256, 1),
            (1280, 720),
            (1024, 1024),
        ] {
            for block in 0..=4 {
                let block = level_block_log2(block, h);
                let mut at = 0;
                for level in 0..max_levels(w, h) {
                    let (size, y) = nil_level(w, h, bpp, block, level);
                    assert_eq!(
                        level_bytes(w, h, bpp, block, level),
                        size,
                        "{w}x{h} {bpp} L{level}"
                    );
                    assert_eq!(level_block_log2(block, (h >> level).max(1)), y);
                    assert_eq!(level_offset(w, h, bpp, block, level), at);
                    at += size;
                }
                assert_eq!(mip_bytes(w, h, bpp, block, max_levels(w, h)), at);
                assert_eq!(
                    mip_bytes(w, h, bpp, block, 1),
                    tiled_bytes_bpp(w, h, bpp, block)
                );
            }
        }
    }
}

#[test]
fn chain_lengths() {
    assert_eq!(max_levels(1, 1), 1);
    assert_eq!(max_levels(256, 256), 9);
    assert_eq!(max_levels(1280, 720), 11);
    assert_eq!(max_levels(16, 1), 5);
}

#[test]
fn offsets_are_a_bijection_inside_the_image() {
    for bpp in [4u32, 8, 16] {
        for (w, h) in [(8u32, 8u32), (30, 70), (64, 33), (17, 5)] {
            for block in 0..=3 {
                let block = level_block_log2(block, h);
                let size = tiled_bytes_bpp(w, h, bpp, block);
                let mut seen = HashSet::new();
                for y in 0..h {
                    for x in 0..w {
                        let o = tiled_offset(w, x, y, bpp, block);
                        assert!(o + bpp as usize <= size, "{w}x{h} bpp {bpp} ({x},{y})");
                        // Pixels do not overlap.
                        for k in 0..bpp as usize {
                            assert!(seen.insert(o + k));
                        }
                    }
                }
            }
        }
    }
}

/// The swizzle the service and the games used before it moved here.
fn tiled_offset_4(width: u32, x: u32, y: u32, block_log2: u32) -> usize {
    let (bx, gob_x) = ((x as usize * 4) % 64, (x as usize * 4) / 64);
    let (gob_row, row) = (y as usize / 8, y as usize % 8);
    let gobs_x = (width as usize * 4).div_ceil(64);
    let block = (gob_row >> block_log2) * gobs_x + gob_x;
    let in_block = gob_row & ((1 << block_log2) - 1);
    let inner = (bx / 32) * 256 + (row / 2) * 64 + ((bx % 32) / 16) * 32 + (row % 2) * 16 + bx % 16;
    block * (512 << block_log2) + in_block * 512 + inner
}

#[test]
fn four_byte_swizzle_is_unchanged() {
    let pts: Vec<(u32, u32)> = (0..40).flat_map(|y| (0..40).map(move |x| (x, y))).collect();
    for block in 0..=4 {
        for &(x, y) in &pts {
            assert_eq!(
                tiled_offset(40, x, y, 4, block),
                tiled_offset_4(40, x, y, block)
            );
        }
    }
}

#[test]
fn formats() {
    assert_eq!([0, 1, 2, 3, 4].map(tf_bytes), [4, 4, 8, 16, 0]);
}

/// What `nxgfx::texture_mipped` writes: every texel of every level lands
/// inside its own level, nothing overlaps.
#[test]
fn a_mip_chain_packs_its_levels() {
    for (w, h, bpp) in [
        (256u32, 256u32, 4u32),
        (100, 37, 4),
        (64, 8, 8),
        (33, 65, 16),
    ] {
        let bl = (0..=4)
            .rev()
            .find(|&b| level_block_log2(b, h) == b)
            .unwrap();
        let levels = max_levels(w, h);
        let mut seen = HashSet::new();
        for level in 0..levels {
            let (lw, lh) = (level_extent(w, level), level_extent(h, level));
            let lbl = level_block_log2(bl, lh);
            let (base, size) = (
                level_offset(w, h, bpp, bl, level),
                level_bytes(w, h, bpp, bl, level),
            );
            for y in 0..lh {
                for x in 0..lw {
                    let o = tiled_offset(lw, x, y, bpp, lbl);
                    assert!(o + bpp as usize <= size);
                    for k in 0..bpp as usize {
                        assert!(seen.insert(base + o + k));
                    }
                }
            }
        }
        assert!(seen.iter().all(|&b| b < mip_bytes(w, h, bpp, bl, levels)));
    }
}

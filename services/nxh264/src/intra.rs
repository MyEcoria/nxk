//! Intra prediction (8.3): 4x4, 8x8 (with the reference filtering), 16x16
//! luma and 8x8 chroma. Predictors write into a plane at `(x, y)`.

#[inline]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// What the neighbours of a block offer.
#[derive(Clone, Copy, Default)]
pub struct Avail {
    pub left: bool,
    pub top: bool,
    pub top_left: bool,
    pub top_right: bool,
}

/// Reads the samples around a block of size `n` (4 or 8) at `(x, y)`;
/// missing ones are 0 (never used by a conforming stream) and the top-right
/// is replicated from the last top sample when it is missing.
fn gather(plane: &[u8], stride: usize, x: usize, y: usize, n: usize, a: Avail, e: &mut [i32; 64]) {
    // Layout: e[0..n] left bottom to top, e[n] corner, e[n+1..n+1+2n] top.
    let o = y * stride + x;
    for i in 0..n {
        e[n - 1 - i] = if a.left {
            plane[o + i * stride - 1] as i32
        } else {
            0
        };
    }
    e[n] = if a.top_left {
        plane[o - stride - 1] as i32
    } else {
        0
    };
    for i in 0..2 * n {
        e[n + 1 + i] = if i < n {
            if a.top {
                plane[o - stride + i] as i32
            } else {
                0
            }
        } else if a.top_right {
            plane[o - stride + i] as i32
        } else if a.top {
            plane[o - stride + n - 1] as i32
        } else {
            0
        };
    }
}

/// p(x, y) over the gathered array for a block of size `n`.
#[inline]
fn p(e: &[i32; 64], n: usize, x: i32, y: i32) -> i32 {
    if y < 0 {
        e[(n as i32 + 1 + x) as usize]
    } else {
        e[n - 1 - y as usize]
    }
}

/// The nine directional/DC predictors shared by 4x4 and 8x8 blocks.
fn predict_nxn(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    n: usize,
    mode: u8,
    a: Avail,
    e: &[i32; 64],
) {
    let o = y * stride + x;
    let ni = n as i32;
    let mut put = |px: usize, py: usize, v: i32| plane[o + py * stride + px] = clip(v);
    match mode {
        0 => {
            for py in 0..n {
                for px in 0..n {
                    put(px, py, p(e, n, px as i32, -1));
                }
            }
        }
        1 => {
            for py in 0..n {
                for px in 0..n {
                    put(px, py, p(e, n, -1, py as i32));
                }
            }
        }
        2 => {
            let sum_t: i32 = (0..ni).map(|i| p(e, n, i, -1)).sum();
            let sum_l: i32 = (0..ni).map(|i| p(e, n, -1, i)).sum();
            let shift = if n == 4 { 2 } else { 3 };
            let dc = match (a.top, a.left) {
                (true, true) => (sum_t + sum_l + ni) >> (shift + 1),
                (false, true) => (sum_l + ni / 2) >> shift,
                (true, false) => (sum_t + ni / 2) >> shift,
                (false, false) => 128,
            };
            for py in 0..n {
                for px in 0..n {
                    put(px, py, dc);
                }
            }
        }
        3 => {
            for py in 0..ni {
                for px in 0..ni {
                    let v = if px + py == 2 * ni - 2 {
                        (p(e, n, 2 * ni - 2, -1) + 3 * p(e, n, 2 * ni - 1, -1) + 2) >> 2
                    } else {
                        (p(e, n, px + py, -1)
                            + 2 * p(e, n, px + py + 1, -1)
                            + p(e, n, px + py + 2, -1)
                            + 2)
                            >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
        4 => {
            for py in 0..ni {
                for px in 0..ni {
                    let v = if px > py {
                        (p(e, n, px - py - 2, -1)
                            + 2 * p(e, n, px - py - 1, -1)
                            + p(e, n, px - py, -1)
                            + 2)
                            >> 2
                    } else if px < py {
                        (p(e, n, -1, py - px - 2)
                            + 2 * p(e, n, -1, py - px - 1)
                            + p(e, n, -1, py - px)
                            + 2)
                            >> 2
                    } else {
                        (p(e, n, 0, -1) + 2 * p(e, n, -1, -1) + p(e, n, -1, 0) + 2) >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
        5 => {
            for py in 0..ni {
                for px in 0..ni {
                    let z = 2 * px - py;
                    let v = if z >= 0 && z & 1 == 0 {
                        (p(e, n, px - (py >> 1) - 1, -1) + p(e, n, px - (py >> 1), -1) + 1) >> 1
                    } else if z >= 0 {
                        (p(e, n, px - (py >> 1) - 2, -1)
                            + 2 * p(e, n, px - (py >> 1) - 1, -1)
                            + p(e, n, px - (py >> 1), -1)
                            + 2)
                            >> 2
                    } else if z == -1 {
                        (p(e, n, -1, 0) + 2 * p(e, n, -1, -1) + p(e, n, 0, -1) + 2) >> 2
                    } else {
                        (p(e, n, -1, py - 2 * px - 1)
                            + 2 * p(e, n, -1, py - 2 * px - 2)
                            + p(e, n, -1, py - 2 * px - 3)
                            + 2)
                            >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
        6 => {
            for py in 0..ni {
                for px in 0..ni {
                    let z = 2 * py - px;
                    let v = if z >= 0 && z & 1 == 0 {
                        (p(e, n, -1, py - (px >> 1) - 1) + p(e, n, -1, py - (px >> 1)) + 1) >> 1
                    } else if z >= 0 {
                        (p(e, n, -1, py - (px >> 1) - 2)
                            + 2 * p(e, n, -1, py - (px >> 1) - 1)
                            + p(e, n, -1, py - (px >> 1))
                            + 2)
                            >> 2
                    } else if z == -1 {
                        (p(e, n, -1, 0) + 2 * p(e, n, -1, -1) + p(e, n, 0, -1) + 2) >> 2
                    } else {
                        (p(e, n, px - 2 * py - 1, -1)
                            + 2 * p(e, n, px - 2 * py - 2, -1)
                            + p(e, n, px - 2 * py - 3, -1)
                            + 2)
                            >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
        7 => {
            for py in 0..ni {
                for px in 0..ni {
                    let i = px + (py >> 1);
                    let v = if py & 1 == 0 {
                        (p(e, n, i, -1) + p(e, n, i + 1, -1) + 1) >> 1
                    } else {
                        (p(e, n, i, -1) + 2 * p(e, n, i + 1, -1) + p(e, n, i + 2, -1) + 2) >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
        _ => {
            for py in 0..ni {
                for px in 0..ni {
                    let z = px + 2 * py;
                    let i = py + (px >> 1);
                    let v = if z > 2 * ni - 3 {
                        p(e, n, -1, ni - 1)
                    } else if z == 2 * ni - 3 {
                        (p(e, n, -1, ni - 2) + 3 * p(e, n, -1, ni - 1) + 2) >> 2
                    } else if z & 1 == 0 {
                        (p(e, n, -1, i) + p(e, n, -1, i + 1) + 1) >> 1
                    } else {
                        (p(e, n, -1, i) + 2 * p(e, n, -1, i + 1) + p(e, n, -1, i + 2) + 2) >> 2
                    };
                    put(px as usize, py as usize, v);
                }
            }
        }
    }
}

/// Intra 4x4 prediction of the block at `(x, y)` in `plane`.
pub fn predict_4x4(plane: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, a: Avail) {
    let mut e = [0i32; 64];
    gather(plane, stride, x, y, 4, a, &mut e);
    predict_nxn(plane, stride, x, y, 4, mode, a, &e);
}

/// Intra 8x8 prediction: the reference samples are filtered first (8.3.2.2.1).
pub fn predict_8x8(plane: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, a: Avail) {
    let mut r = [0i32; 64];
    gather(plane, stride, x, y, 8, a, &mut r);
    // r: left bottom to top at 0..8, corner at 8, top at 9..25.
    let mut f = r;
    let top = |i: usize| r[9 + i];
    let left = |i: usize| r[7 - i];
    if a.top {
        f[9] = if a.top_left {
            (r[8] + 2 * top(0) + top(1) + 2) >> 2
        } else {
            (3 * top(0) + top(1) + 2) >> 2
        };
        for i in 1..15 {
            f[9 + i] = (top(i - 1) + 2 * top(i) + top(i + 1) + 2) >> 2;
        }
        f[9 + 15] = (top(14) + 3 * top(15) + 2) >> 2;
    }
    if a.top_left {
        f[8] = match (a.top, a.left) {
            (true, true) => (top(0) + 2 * r[8] + left(0) + 2) >> 2,
            (true, false) => (3 * r[8] + top(0) + 2) >> 2,
            (false, true) => (3 * r[8] + left(0) + 2) >> 2,
            (false, false) => r[8],
        };
    }
    if a.left {
        f[7] = if a.top_left {
            (r[8] + 2 * left(0) + left(1) + 2) >> 2
        } else {
            (3 * left(0) + left(1) + 2) >> 2
        };
        for i in 1..7 {
            f[7 - i] = (left(i - 1) + 2 * left(i) + left(i + 1) + 2) >> 2;
        }
        f[0] = (left(6) + 3 * left(7) + 2) >> 2;
    }
    predict_nxn(plane, stride, x, y, 8, mode, a, &f);
}

/// Intra 16x16 luma prediction: 0 vertical, 1 horizontal, 2 DC, 3 plane.
pub fn predict_16x16(plane: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, a: Avail) {
    let o = y * stride + x;
    match mode {
        0 => {
            for py in 0..16 {
                for px in 0..16 {
                    plane[o + py * stride + px] = plane[o - stride + px];
                }
            }
        }
        1 => {
            for py in 0..16 {
                let v = plane[o + py * stride - 1];
                plane[o + py * stride..o + py * stride + 16].fill(v);
            }
        }
        2 => {
            let sum_t: i32 = if a.top {
                (0..16).map(|i| plane[o - stride + i] as i32).sum()
            } else {
                0
            };
            let sum_l: i32 = if a.left {
                (0..16).map(|i| plane[o + i * stride - 1] as i32).sum()
            } else {
                0
            };
            let dc = match (a.top, a.left) {
                (true, true) => (sum_t + sum_l + 16) >> 5,
                (false, true) => (sum_l + 8) >> 4,
                (true, false) => (sum_t + 8) >> 4,
                (false, false) => 128,
            };
            for py in 0..16 {
                plane[o + py * stride..o + py * stride + 16].fill(dc as u8);
            }
        }
        _ => {
            let top = |i: i32| plane[(o as i32 - stride as i32 + i) as usize] as i32;
            let left = |i: i32| {
                if i < 0 {
                    plane[o - stride - 1] as i32
                } else {
                    plane[o + i as usize * stride - 1] as i32
                }
            };
            let (mut h, mut v) = (0, 0);
            for i in 0..8 {
                h += (i + 1) * (top(8 + i) - top(6 - i));
                v += (i + 1) * (left(8 + i) - left(6 - i));
            }
            let a16 = 16 * (left(15) + top(15));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for py in 0..16i32 {
                for px in 0..16i32 {
                    plane[o + py as usize * stride + px as usize] =
                        clip((a16 + b * (px - 7) + c * (py - 7) + 16) >> 5);
                }
            }
        }
    }
}

/// Intra chroma prediction of an 8x8 block: 0 DC, 1 horizontal, 2 vertical, 3 plane.
pub fn predict_chroma(plane: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, a: Avail) {
    let o = y * stride + x;
    match mode {
        0 => {
            for blk in 0..4 {
                let (bx, by) = ((blk & 1) * 4, (blk >> 1) * 4);
                let sum_t: i32 = if a.top {
                    (0..4).map(|i| plane[o - stride + bx + i] as i32).sum()
                } else {
                    0
                };
                let sum_l: i32 = if a.left {
                    (0..4)
                        .map(|i| plane[o + (by + i) * stride - 1] as i32)
                        .sum()
                } else {
                    0
                };
                let dc = if (bx == 0 && by == 0) || (bx > 0 && by > 0) {
                    match (a.top, a.left) {
                        (true, true) => (sum_t + sum_l + 4) >> 3,
                        (false, true) => (sum_l + 2) >> 2,
                        (true, false) => (sum_t + 2) >> 2,
                        (false, false) => 128,
                    }
                } else if bx > 0 {
                    if a.top {
                        (sum_t + 2) >> 2
                    } else if a.left {
                        (sum_l + 2) >> 2
                    } else {
                        128
                    }
                } else if a.left {
                    (sum_l + 2) >> 2
                } else if a.top {
                    (sum_t + 2) >> 2
                } else {
                    128
                };
                for py in 0..4 {
                    let r = o + (by + py) * stride + bx;
                    plane[r..r + 4].fill(dc as u8);
                }
            }
        }
        1 => {
            for py in 0..8 {
                let v = plane[o + py * stride - 1];
                plane[o + py * stride..o + py * stride + 8].fill(v);
            }
        }
        2 => {
            for py in 0..8 {
                for px in 0..8 {
                    plane[o + py * stride + px] = plane[o - stride + px];
                }
            }
        }
        _ => {
            let top = |i: i32| plane[(o as i32 - stride as i32 + i) as usize] as i32;
            let left = |i: i32| {
                if i < 0 {
                    plane[o - stride - 1] as i32
                } else {
                    plane[o + i as usize * stride - 1] as i32
                }
            };
            let (mut h, mut v) = (0, 0);
            for i in 0..4 {
                h += (i + 1) * (top(4 + i) - top(2 - i));
                v += (i + 1) * (left(4 + i) - left(2 - i));
            }
            let a16 = 16 * (left(7) + top(7));
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;
            for py in 0..8i32 {
                for px in 0..8i32 {
                    plane[o + py as usize * stride + px as usize] =
                        clip((a16 + b * (px - 3) + c * (py - 3) + 16) >> 5);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_with(border_top: &[u8], border_left: &[u8], corner: u8) -> Vec<u8> {
        // 16 wide, 16 tall; the block at (4, 4).
        let mut p = vec![0u8; 16 * 16];
        p[3 * 16 + 3] = corner;
        for (i, &v) in border_top.iter().enumerate() {
            p[3 * 16 + 4 + i] = v;
        }
        for (i, &v) in border_left.iter().enumerate() {
            p[(4 + i) * 16 + 3] = v;
        }
        p
    }

    const ALL: Avail = Avail {
        left: true,
        top: true,
        top_left: true,
        top_right: true,
    };

    #[test]
    fn vertical_and_horizontal_copy_the_edges() {
        let mut p = plane_with(&[10, 20, 30, 40, 50, 60, 70, 80], &[1, 2, 3, 4], 5);
        predict_4x4(&mut p, 16, 4, 4, 0, ALL);
        for y in 0..4 {
            assert_eq!(&p[(4 + y) * 16 + 4..(4 + y) * 16 + 8], &[10, 20, 30, 40]);
        }
        predict_4x4(&mut p, 16, 4, 4, 1, ALL);
        for y in 0..4 {
            assert!(p[(4 + y) * 16 + 4..(4 + y) * 16 + 8]
                .iter()
                .all(|&v| v == y as u8 + 1));
        }
    }

    #[test]
    fn dc_averages_what_is_available() {
        let mut p = plane_with(&[8, 8, 8, 8, 0, 0, 0, 0], &[16, 16, 16, 16], 0);
        predict_4x4(&mut p, 16, 4, 4, 2, ALL);
        assert_eq!(p[4 * 16 + 4], (32 + 64 + 4) >> 3);
        let none = Avail::default();
        predict_4x4(&mut p, 16, 4, 4, 2, none);
        assert_eq!(p[4 * 16 + 4], 128);
    }

    #[test]
    fn diagonal_down_left_uses_the_top_right() {
        let mut p = plane_with(&[0, 0, 0, 0, 80, 80, 80, 80], &[0; 4], 0);
        predict_4x4(&mut p, 16, 4, 4, 3, ALL);
        // The bottom-right sample is (p[6] + 3 p[7] + 2) >> 2 = 80.
        assert_eq!(p[7 * 16 + 7], 80);
        assert_eq!(p[4 * 16 + 4], 0);
    }

    #[test]
    fn a_missing_top_right_repeats_the_last_top_sample() {
        let mut p = plane_with(&[0, 0, 0, 40, 99, 99, 99, 99], &[0; 4], 0);
        let a = Avail {
            top_right: false,
            ..ALL
        };
        predict_4x4(&mut p, 16, 4, 4, 3, a);
        assert_eq!(p[7 * 16 + 7], 40);
    }

    #[test]
    fn plane_prediction_of_a_flat_area_is_flat() {
        let mut p = vec![77u8; 32 * 32];
        predict_16x16(&mut p, 32, 8, 8, 3, ALL);
        assert!(p[8 * 32 + 8..8 * 32 + 24].iter().all(|&v| v == 77));
        predict_chroma(&mut p, 32, 8, 8, 3, ALL);
        assert!(p[8 * 32 + 8..8 * 32 + 16].iter().all(|&v| v == 77));
    }

    #[test]
    fn the_8x8_filter_leaves_a_flat_edge_flat() {
        let mut p = vec![50u8; 32 * 32];
        predict_8x8(&mut p, 32, 8, 8, 0, ALL);
        assert!(p[8 * 32 + 8..8 * 32 + 16].iter().all(|&v| v == 50));
        predict_8x8(&mut p, 32, 8, 8, 8, ALL);
        assert!(p[15 * 32 + 8..15 * 32 + 16].iter().all(|&v| v == 50));
    }

    #[test]
    fn chroma_dc_follows_the_block_rules() {
        // Top row all 100, left column all 20: block 0 mixes both, block 1
        // takes the top, block 2 the left, block 3 mixes both.
        let mut p = vec![0u8; 16 * 16];
        for i in 0..8 {
            p[3 * 16 + 4 + i] = 100;
            p[(4 + i) * 16 + 3] = 20;
        }
        predict_chroma(&mut p, 16, 4, 4, 0, ALL);
        let mix = ((400i32 + 80 + 4) >> 3) as u8;
        assert_eq!(p[4 * 16 + 4], mix);
        assert_eq!(p[4 * 16 + 8], 100);
        assert_eq!(p[8 * 16 + 4], 20);
        assert_eq!(p[8 * 16 + 8], mix);
    }
}

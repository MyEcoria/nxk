//! Motion compensation (8.4.2.2): quarter-sample luma interpolation with
//! the 6-tap filter, eighth-sample bilinear chroma, edges repeated.

use crate::pic::Plane;

#[inline]
fn clip(v: i32) -> i32 {
    v.clamp(0, 255)
}

#[inline]
fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// Where the samples of a block's neighbourhood are: in the plane itself
/// when the whole `(w + 5) x (h + 5)` window is inside it, else in a copy
/// with the edges repeated.
struct Source<'a> {
    data: &'a [u8],
    stride: usize,
    /// Index of the block's top-left sample.
    base: usize,
}

const MAX_WINDOW: usize = 21 * 21;

fn source<'a>(
    plane: &'a Plane,
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    tmp: &'a mut [u8; MAX_WINDOW],
) -> Source<'a> {
    let (pw, ph) = (plane.width as i32, plane.height as i32);
    if x >= 2 && y >= 2 && x + w as i32 + 3 <= pw && y + h as i32 + 3 <= ph {
        return Source {
            data: &plane.data,
            stride: plane.stride,
            base: y as usize * plane.stride + x as usize,
        };
    }
    let ww = w + 5;
    for r in 0..h + 5 {
        let o = (y - 2 + r as i32).clamp(0, ph - 1) as usize * plane.stride;
        for c in 0..ww {
            tmp[r * ww + c] = plane.data[o + (x - 2 + c as i32).clamp(0, pw - 1) as usize];
        }
    }
    Source {
        data: &tmp[..],
        stride: ww,
        base: 2 * ww + 2,
    }
}

/// Luma prediction of a `w` x `h` block (up to 16x16) whose top-left
/// sample is at `(x, y)` in the picture, motion vector `mv` in quarter
/// samples: written to `dst` at `o`.
#[allow(clippy::too_many_arguments)]
pub fn luma(
    plane: &Plane,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    mv: [i16; 2],
    dst: &mut [u8],
    o: usize,
    stride: usize,
) {
    let (fx, fy) = ((mv[0] & 3) as usize, (mv[1] & 3) as usize);
    let (sx, sy) = (
        x as i32 + (mv[0] >> 2) as i32,
        y as i32 + (mv[1] >> 2) as i32,
    );
    let mut tmp = [0u8; MAX_WINDOW];
    let src = source(plane, sx, sy, w, h, &mut tmp);
    let ss = src.stride;
    let d = src.data;
    let at = |cx: usize, cy: usize| src.base + cy * ss + cx;
    // Horizontal half sample right of (cx, cy), unrounded and rounded.
    let b1 = |cx: usize, cy: usize| {
        let i = at(cx, cy);
        tap6(
            d[i - 2] as i32,
            d[i - 1] as i32,
            d[i] as i32,
            d[i + 1] as i32,
            d[i + 2] as i32,
            d[i + 3] as i32,
        )
    };
    let hh = |cx: usize, cy: usize| {
        let i = at(cx, cy);
        clip(
            (tap6(
                d[i - 2 * ss] as i32,
                d[i - ss] as i32,
                d[i] as i32,
                d[i + ss] as i32,
                d[i + 2 * ss] as i32,
                d[i + 3 * ss] as i32,
            ) + 16)
                >> 5,
        )
    };
    let out = |dst: &mut [u8], cx: usize, cy: usize, v: i32| dst[o + cy * stride + cx] = v as u8;
    match (fx, fy) {
        (0, 0) => {
            for cy in 0..h {
                let i = at(0, cy);
                dst[o + cy * stride..o + cy * stride + w].copy_from_slice(&d[i..i + w]);
            }
        }
        (_, 0) => {
            for cy in 0..h {
                for cx in 0..w {
                    let b = clip((b1(cx, cy) + 16) >> 5);
                    let v = match fx {
                        1 => (d[at(cx, cy)] as i32 + b + 1) >> 1,
                        2 => b,
                        _ => (d[at(cx + 1, cy)] as i32 + b + 1) >> 1,
                    };
                    out(dst, cx, cy, v);
                }
            }
        }
        (0, _) => {
            for cy in 0..h {
                for cx in 0..w {
                    let hv = hh(cx, cy);
                    let v = match fy {
                        1 => (d[at(cx, cy)] as i32 + hv + 1) >> 1,
                        2 => hv,
                        _ => (d[at(cx, cy + 1)] as i32 + hv + 1) >> 1,
                    };
                    out(dst, cx, cy, v);
                }
            }
        }
        (2, _) | (_, 2) => {
            // The unrounded horizontal half samples of rows -2..h+3, then the vertical filter over them.
            let mut t = [0i32; 16 * 21];
            for r in 0..h + 5 {
                for cx in 0..w {
                    t[r * w + cx] = {
                        let i = at(cx, 0) + r * ss - 2 * ss;
                        tap6(
                            d[i - 2] as i32,
                            d[i - 1] as i32,
                            d[i] as i32,
                            d[i + 1] as i32,
                            d[i + 2] as i32,
                            d[i + 3] as i32,
                        )
                    };
                }
            }
            for cy in 0..h {
                for cx in 0..w {
                    let j = clip(
                        (tap6(
                            t[cy * w + cx],
                            t[(cy + 1) * w + cx],
                            t[(cy + 2) * w + cx],
                            t[(cy + 3) * w + cx],
                            t[(cy + 4) * w + cx],
                            t[(cy + 5) * w + cx],
                        ) + 512)
                            >> 10,
                    );
                    let v = match (fx, fy) {
                        (2, 2) => j,
                        (2, 1) => (clip((t[(cy + 2) * w + cx] + 16) >> 5) + j + 1) >> 1,
                        (2, _) => (clip((t[(cy + 3) * w + cx] + 16) >> 5) + j + 1) >> 1,
                        (1, _) => (hh(cx, cy) + j + 1) >> 1,
                        _ => (hh(cx + 1, cy) + j + 1) >> 1,
                    };
                    out(dst, cx, cy, v);
                }
            }
        }
        _ => {
            // (1|3, 1|3): the horizontal and the vertical half sample around the corner.
            for cy in 0..h {
                for cx in 0..w {
                    let b = clip((b1(cx, cy + (fy == 3) as usize) + 16) >> 5);
                    let hv = hh(cx + (fx == 3) as usize, cy);
                    out(dst, cx, cy, (b + hv + 1) >> 1);
                }
            }
        }
    }
}

/// Chroma prediction of a `w` x `h` block at `(x, y)` of the chroma plane,
/// the luma motion vector `mv` read in eighths of a chroma sample.
#[allow(clippy::too_many_arguments)]
pub fn chroma(
    plane: &Plane,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    mv: [i16; 2],
    dst: &mut [u8],
    o: usize,
    stride: usize,
) {
    let (fx, fy) = ((mv[0] & 7) as i32, (mv[1] & 7) as i32);
    let (sx, sy) = (
        x as i32 + (mv[0] >> 3) as i32,
        y as i32 + (mv[1] >> 3) as i32,
    );
    let (pw, ph) = (plane.width as i32, plane.height as i32);
    let (w00, w10, w01, w11) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    if sx >= 0 && sy >= 0 && sx + (w as i32) < pw && sy + (h as i32) < ph {
        let ps = plane.stride;
        for cy in 0..h {
            let i = (sy as usize + cy) * ps + sx as usize;
            let (r0, r1) = (
                &plane.data[i..i + w + 1],
                &plane.data[i + ps..i + ps + w + 1],
            );
            let d = &mut dst[o + cy * stride..o + cy * stride + w];
            if fx == 0 && fy == 0 {
                d.copy_from_slice(&r0[..w]);
            } else {
                for cx in 0..w {
                    d[cx] = ((w00 * r0[cx] as i32
                        + w10 * r0[cx + 1] as i32
                        + w01 * r1[cx] as i32
                        + w11 * r1[cx + 1] as i32
                        + 32)
                        >> 6) as u8;
                }
            }
        }
        return;
    }
    let at = |cx: i32, cy: i32| {
        plane.data[cy.clamp(0, ph - 1) as usize * plane.stride + cx.clamp(0, pw - 1) as usize]
            as i32
    };
    for cy in 0..h as i32 {
        for cx in 0..w as i32 {
            let (a, b) = (at(sx + cx, sy + cy), at(sx + cx + 1, sy + cy));
            let (c, d) = (at(sx + cx, sy + cy + 1), at(sx + cx + 1, sy + cy + 1));
            let v = (w00 * a + w10 * b + w01 * c + w11 * d + 32) >> 6;
            dst[o + cy as usize * stride + cx as usize] = v as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Plane {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.push(f(x, y));
            }
        }
        Plane {
            data,
            stride: w,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_whole_sample_vector_copies() {
        let p = plane(32, 32, |x, y| (x * 7 + y * 3) as u8);
        let mut out = vec![0u8; 16 * 16];
        luma(&p, 8, 8, 16, 16, [4 * 3, 4 * -2], &mut out, 0, 16);
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(out[y * 16 + x], p.data[(8 + y - 2) * 32 + 8 + x + 3]);
            }
        }
    }

    #[test]
    fn a_flat_area_stays_flat_at_every_fraction() {
        let p = plane(40, 40, |_, _| 90);
        for fy in 0..4 {
            for fx in 0..4 {
                let mut out = vec![0u8; 64];
                luma(&p, 12, 12, 8, 8, [fx, fy], &mut out, 0, 8);
                assert!(out.iter().all(|&v| v == 90), "{} {}", fx, fy);
            }
        }
        let mut out = vec![0u8; 16];
        chroma(&p, 3, 3, 4, 4, [5, 3], &mut out, 0, 4);
        assert!(out.iter().all(|&v| v == 90));
    }

    #[test]
    fn a_vector_outside_repeats_the_edge() {
        let p = plane(32, 32, |x, y| (x + y) as u8);
        let mut out = vec![0u8; 16];
        luma(&p, 0, 0, 4, 4, [-4 * 20, -4 * 20], &mut out, 0, 4);
        assert!(out.iter().all(|&v| v == 0));
        luma(&p, 28, 28, 4, 4, [4 * 20, 4 * 20], &mut out, 0, 4);
        assert!(out.iter().all(|&v| v == 62));
    }

    #[test]
    fn half_sample_positions_use_the_six_tap_filter() {
        // A step edge: left half 0, right half 200. The horizontal half
        // sample between columns 15 and 16 is (0 - 0 + 20*0 + 20*200 - 5*200 + 200 + 16) >> 5.
        let p = plane(32, 8, |x, _| if x < 16 { 0 } else { 200 });
        let mut out = vec![0u8; 4];
        luma(&p, 15, 2, 1, 1, [2, 0], &mut out, 0, 1);
        assert_eq!(out[0] as i32, (20 * 200 - 5 * 200 + 200 + 16) >> 5);
        // A quarter sample averages it with the whole one on its left.
        luma(&p, 15, 2, 1, 1, [1, 0], &mut out, 0, 1);
        assert_eq!(
            out[0] as i32,
            (((20 * 200 - 5 * 200 + 200 + 16) >> 5) + 1) >> 1
        );
    }

    #[test]
    fn chroma_is_bilinear() {
        let p = plane(16, 16, |x, _| (x * 8) as u8);
        let mut out = vec![0u8; 1];
        chroma(&p, 4, 4, 1, 1, [4, 0], &mut out, 0, 1);
        // Halfway between 32 and 40.
        assert_eq!(out[0], 36);
    }
}

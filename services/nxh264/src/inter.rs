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

/// The `w + 5` by `h + 5` window of `plane` whose top-left sample is
/// `(x - 2, y - 2)`, coordinates clamped to the plane.
fn window(plane: &Plane, x: i32, y: i32, w: usize, h: usize, out: &mut [i32]) {
    let (pw, ph) = (plane.width as i32, plane.height as i32);
    let ww = w + 5;
    let inside = x >= 2 && y >= 2 && x + w as i32 + 3 <= pw && y + h as i32 + 3 <= ph;
    for r in 0..h + 5 {
        let sy = y - 2 + r as i32;
        if inside {
            let o = sy as usize * plane.stride + (x - 2) as usize;
            for c in 0..ww {
                out[r * ww + c] = plane.data[o + c] as i32;
            }
        } else {
            let o = sy.clamp(0, ph - 1) as usize * plane.stride;
            for c in 0..ww {
                let sx = (x - 2 + c as i32).clamp(0, pw - 1) as usize;
                out[r * ww + c] = plane.data[o + sx] as i32;
            }
        }
    }
}

/// Luma prediction of a `w` x `h` block (up to 16x16) whose top-left
/// sample is at `(x, y)` in the picture, motion vector `mv` in quarter
/// samples: written to `dst` at `o`.
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
    let mut win = [0i32; 21 * 21];
    window(plane, sx, sy, w, h, &mut win);
    let ww = w + 5;
    // G at (cx, cy) of the block, in window coordinates.
    let g = |cx: usize, cy: usize| win[(cy + 2) * ww + cx + 2];
    // b: horizontal half sample right of G, unrounded and rounded.
    let b1 = |cx: usize, cy: usize| {
        let r = (cy + 2) * ww + cx;
        tap6(
            win[r],
            win[r + 1],
            win[r + 2],
            win[r + 3],
            win[r + 4],
            win[r + 5],
        )
    };
    let h1 = |cx: usize, cy: usize| {
        let c = cx + 2;
        tap6(
            win[cy * ww + c],
            win[(cy + 1) * ww + c],
            win[(cy + 2) * ww + c],
            win[(cy + 3) * ww + c],
            win[(cy + 4) * ww + c],
            win[(cy + 5) * ww + c],
        )
    };
    let b = |cx: usize, cy: usize| clip((b1(cx, cy) + 16) >> 5);
    let hh = |cx: usize, cy: usize| clip((h1(cx, cy) + 16) >> 5);
    // j: the center half sample, from the unrounded horizontal values of six rows.
    let j = |cx: usize, cy: usize| {
        let v = |dy: usize| {
            // b1 of row cy - 2 + dy (window row cy + dy)
            let r = (cy + dy) * ww + cx;
            tap6(
                win[r],
                win[r + 1],
                win[r + 2],
                win[r + 3],
                win[r + 4],
                win[r + 5],
            )
        };
        clip((tap6(v(0), v(1), v(2), v(3), v(4), v(5)) + 512) >> 10)
    };
    for cy in 0..h {
        for cx in 0..w {
            let v = match (fx, fy) {
                (0, 0) => g(cx, cy),
                (1, 0) => (g(cx, cy) + b(cx, cy) + 1) >> 1,
                (2, 0) => b(cx, cy),
                (3, 0) => (g(cx + 1, cy) + b(cx, cy) + 1) >> 1,
                (0, 1) => (g(cx, cy) + hh(cx, cy) + 1) >> 1,
                (0, 2) => hh(cx, cy),
                (0, 3) => (g(cx, cy + 1) + hh(cx, cy) + 1) >> 1,
                (1, 1) => (b(cx, cy) + hh(cx, cy) + 1) >> 1,
                (3, 1) => (b(cx, cy) + hh(cx + 1, cy) + 1) >> 1,
                (1, 3) => (hh(cx, cy) + b(cx, cy + 1) + 1) >> 1,
                (3, 3) => (hh(cx + 1, cy) + b(cx, cy + 1) + 1) >> 1,
                (2, 2) => j(cx, cy),
                (2, 1) => (b(cx, cy) + j(cx, cy) + 1) >> 1,
                (2, 3) => (j(cx, cy) + b(cx, cy + 1) + 1) >> 1,
                (1, 2) => (hh(cx, cy) + j(cx, cy) + 1) >> 1,
                _ => (j(cx, cy) + hh(cx + 1, cy) + 1) >> 1, // (3, 2)
            };
            dst[o + cy * stride + cx] = v as u8;
        }
    }
}

/// Chroma prediction of a `w` x `h` block at `(x, y)` of the chroma plane,
/// the luma motion vector `mv` read in eighths of a chroma sample.
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
    let at = |cx: i32, cy: i32| {
        plane.data[cy.clamp(0, ph - 1) as usize * plane.stride + cx.clamp(0, pw - 1) as usize]
            as i32
    };
    for cy in 0..h as i32 {
        for cx in 0..w as i32 {
            let (a, b) = (at(sx + cx, sy + cy), at(sx + cx + 1, sy + cy));
            let (c, d) = (at(sx + cx, sy + cy + 1), at(sx + cx + 1, sy + cy + 1));
            let v = ((8 - fx) * (8 - fy) * a
                + fx * (8 - fy) * b
                + (8 - fx) * fy * c
                + fx * fy * d
                + 32)
                >> 6;
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
            (0 + ((20 * 200 - 5 * 200 + 200 + 16) >> 5) + 1) >> 1
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

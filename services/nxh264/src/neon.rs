//! Luma interpolation with NEON (aarch64): the six-tap filter on eight
//! samples at a time. Same results as the portable version in `inter`
//! (tests compare them: on the PC through the emulator, `h264bench selftest`).

use core::arch::aarch64::*;

/// a - 5 b + 20 c + 20 d - 5 e + f on eight lanes, 16 bits (the sum fits).
#[inline(always)]
unsafe fn six(
    a: uint8x8_t,
    b: uint8x8_t,
    c: uint8x8_t,
    d: uint8x8_t,
    e: uint8x8_t,
    f: uint8x8_t,
) -> int16x8_t {
    let af = vreinterpretq_s16_u16(vaddl_u8(a, f));
    let cd = vreinterpretq_s16_u16(vaddl_u8(c, d));
    let be = vreinterpretq_s16_u16(vaddl_u8(b, e));
    vmlsq_n_s16(vmlaq_n_s16(af, cd, 20), be, 5)
}

/// The six rows' filter on 16-bit inputs, rounded and clamped: the centre sample.
#[inline(always)]
unsafe fn centre(t: [int16x8_t; 6]) -> uint8x8_t {
    let part = |lo: bool| {
        let g = |v: int16x8_t| {
            if lo {
                vget_low_s16(v)
            } else {
                vget_high_s16(v)
            }
        };
        let a = vaddl_s16(g(t[0]), g(t[5]));
        let m = vaddl_s16(g(t[2]), g(t[3]));
        let n = vaddl_s16(g(t[1]), g(t[4]));
        vqrshrun_n_s32::<10>(vmlsq_n_s32(vmlaq_n_s32(a, m, 20), n, 5))
    };
    vqmovn_u16(vcombine_u16(part(true), part(false)))
}

/// Luma prediction of a `w` x `h` block, `w` 8 or 16: `d` is the window
/// (`w + 5` columns from two before the block, `h + 5` rows from two above)
/// with rows `ss` apart, its top-left sample at index `org`.
#[allow(clippy::too_many_arguments)]
pub fn luma(
    d: &[u8],
    org: usize,
    ss: usize,
    w: usize,
    h: usize,
    fx: usize,
    fy: usize,
    dst: &mut [u8],
    o: usize,
    stride: usize,
) {
    assert!(w == 8 || w == 16);
    assert!(org + (h + 4) * ss + w + 5 <= d.len());
    assert!(o + (h - 1) * stride + w <= dst.len());
    let p = d.as_ptr();
    let q = dst.as_mut_ptr();
    // SAFETY: every load reads inside the window checked above (at most
    // columns 0 to w + 4 of rows 0 to h + 4), every store inside `dst`.
    unsafe {
        let at = |r: usize, c: usize| p.add(org + r * ss + c);
        let out = |cy: usize, c: usize, v: uint8x8_t| vst1_u8(q.add(o + cy * stride + c), v);
        // Horizontal half sample of row `r` of the window, columns c..c+8 of the block: rounded and raw.
        let hraw = |r: usize, c: usize| {
            let l = |k: usize| vld1_u8(at(r, c + k));
            six(l(0), l(1), l(2), l(3), l(4), l(5))
        };
        let hrow = |r: usize, c: usize| vqrshrun_n_s16::<5>(hraw(r, c));
        // Vertical half sample at window column `col` for the block's row `cy`.
        let vraw = |cy: usize, col: usize| {
            let l = |k: usize| vld1_u8(at(cy + k, col));
            six(l(0), l(1), l(2), l(3), l(4), l(5))
        };
        let vcol = |cy: usize, col: usize| vqrshrun_n_s16::<5>(vraw(cy, col));
        let whole = |r: usize, col: usize| vld1_u8(at(r, col));
        let mut c = 0;
        while c < w {
            match (fx, fy) {
                (0, 0) => {
                    for cy in 0..h {
                        out(cy, c, whole(cy + 2, c + 2));
                    }
                }
                (_, 0) => {
                    let g = if fx == 3 { 3 } else { 2 };
                    for cy in 0..h {
                        let b = hrow(cy + 2, c);
                        out(
                            cy,
                            c,
                            if fx == 2 {
                                b
                            } else {
                                vrhadd_u8(b, whole(cy + 2, c + g))
                            },
                        );
                    }
                }
                (0, _) => {
                    let g = if fy == 3 { 3 } else { 2 };
                    for cy in 0..h {
                        let v = vcol(cy, c + 2);
                        out(
                            cy,
                            c,
                            if fy == 2 {
                                v
                            } else {
                                vrhadd_u8(v, whole(cy + g, c + 2))
                            },
                        );
                    }
                }
                (2, _) | (_, 2) => {
                    // Raw horizontal half samples of the h + 5 rows, then the centre.
                    let mut t = [vdupq_n_s16(0); 21];
                    for r in 0..h + 5 {
                        t[r] = hraw(r, c);
                    }
                    for cy in 0..h {
                        let j =
                            centre([t[cy], t[cy + 1], t[cy + 2], t[cy + 3], t[cy + 4], t[cy + 5]]);
                        let v = match (fx, fy) {
                            (2, 2) => j,
                            (2, _) => vrhadd_u8(
                                vqrshrun_n_s16::<5>(t[cy + if fy == 3 { 3 } else { 2 }]),
                                j,
                            ),
                            _ => vrhadd_u8(vcol(cy, c + if fx == 3 { 3 } else { 2 }), j),
                        };
                        out(cy, c, v);
                    }
                }
                _ => {
                    // The horizontal half sample of this row (fy 1) or the next (fy 3) with the
                    // vertical one of this column (fx 1) or the next (fx 3).
                    let (rb, col) = (if fy == 3 { 3 } else { 2 }, if fx == 3 { 3 } else { 2 });
                    for cy in 0..h {
                        out(cy, c, vrhadd_u8(hrow(cy + rb, c), vcol(cy, c + col)));
                    }
                }
            }
            c += 8;
        }
    }
}

// ---- the deblocking filter

/// 8x8 bytes transposed: row `i` of the result holds byte `i` of every input row.
#[inline(always)]
unsafe fn transpose8(r: [uint8x8_t; 8]) -> [uint8x8_t; 8] {
    let x0 = vtrn_u8(r[0], r[1]);
    let x1 = vtrn_u8(r[2], r[3]);
    let x2 = vtrn_u8(r[4], r[5]);
    let x3 = vtrn_u8(r[6], r[7]);
    let y0 = vtrn_u16(vreinterpret_u16_u8(x0.0), vreinterpret_u16_u8(x1.0));
    let y1 = vtrn_u16(vreinterpret_u16_u8(x0.1), vreinterpret_u16_u8(x1.1));
    let y2 = vtrn_u16(vreinterpret_u16_u8(x2.0), vreinterpret_u16_u8(x3.0));
    let y3 = vtrn_u16(vreinterpret_u16_u8(x2.1), vreinterpret_u16_u8(x3.1));
    let z0 = vtrn_u32(vreinterpret_u32_u16(y0.0), vreinterpret_u32_u16(y2.0));
    let z1 = vtrn_u32(vreinterpret_u32_u16(y1.0), vreinterpret_u32_u16(y3.0));
    let z2 = vtrn_u32(vreinterpret_u32_u16(y0.1), vreinterpret_u32_u16(y2.1));
    let z3 = vtrn_u32(vreinterpret_u32_u16(y1.1), vreinterpret_u32_u16(y3.1));
    let u = |v: uint32x2_t| vreinterpret_u8_u32(v);
    [
        u(z0.0),
        u(z1.0),
        u(z2.0),
        u(z3.0),
        u(z0.1),
        u(z1.1),
        u(z2.1),
        u(z3.1),
    ]
}

#[inline(always)]
unsafe fn wide(v: uint8x8_t) -> int16x8_t {
    vreinterpretq_s16_u16(vmovl_u8(v))
}

#[inline(always)]
unsafe fn clip255(v: int16x8_t) -> int16x8_t {
    vmaxq_s16(vminq_s16(v, vdupq_n_s16(255)), vdupq_n_s16(0))
}

/// What the eight lanes of an edge share and differ in.
pub struct Lanes {
    pub alpha: i16,
    pub beta: i16,
    /// Per lane: tC0 for bS 1 to 3 (chroma: already plus one), whatever for bS 0 and 4.
    pub tc0: [i16; 8],
    /// Per lane: 0xFFFF where bS is not 0.
    pub active: [u16; 8],
    /// Per lane: 0xFFFF where bS is 4.
    pub strong: [u16; 8],
}

/// The luma filter on eight lines: the eight samples across the edge in,
/// p2 to q2 out (p3 and q3 are not changed).
#[inline(always)]
unsafe fn luma8(s: [uint8x8_t; 8], l: &Lanes) -> [uint8x8_t; 6] {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = s.map(|v| wide(v));
    let (alpha, beta) = (vdupq_n_s16(l.alpha), vdupq_n_s16(l.beta));
    let (tc0, active, strong4) = (
        vld1q_s16(l.tc0.as_ptr()),
        vld1q_u16(l.active.as_ptr()),
        vld1q_u16(l.strong.as_ptr()),
    );
    let on = vandq_u16(
        vandq_u16(
            vcltq_s16(vabdq_s16(p0, q0), alpha),
            vandq_u16(
                vcltq_s16(vabdq_s16(p1, p0), beta),
                vcltq_s16(vabdq_s16(q1, q0), beta),
            ),
        ),
        active,
    );
    let ap = vcltq_s16(vabdq_s16(p2, p0), beta);
    let aq = vcltq_s16(vabdq_s16(q2, q0), beta);
    let one = vdupq_n_s16(1);
    // bS 1 to 3.
    let tc = vsubq_s16(
        vsubq_s16(tc0, vreinterpretq_s16_u16(ap)),
        vreinterpretq_s16_u16(aq),
    );
    let delta = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(vshlq_n_s16::<2>(vsubq_s16(q0, p0)), vsubq_s16(p1, q1)),
        vdupq_n_s16(4),
    ));
    let delta = vminq_s16(vmaxq_s16(delta, vnegq_s16(tc)), tc);
    let (p0w, q0w) = (clip255(vaddq_s16(p0, delta)), clip255(vsubq_s16(q0, delta)));
    let avg = vshrq_n_s16::<1>(vaddq_s16(vaddq_s16(p0, q0), one));
    let ntc0 = vnegq_s16(tc0);
    let dp1 = vminq_s16(
        vmaxq_s16(
            vshrq_n_s16::<1>(vsubq_s16(vaddq_s16(p2, avg), vshlq_n_s16::<1>(p1))),
            ntc0,
        ),
        tc0,
    );
    let dq1 = vminq_s16(
        vmaxq_s16(
            vshrq_n_s16::<1>(vsubq_s16(vaddq_s16(q2, avg), vshlq_n_s16::<1>(q1))),
            ntc0,
        ),
        tc0,
    );
    let p1w = vbslq_s16(ap, clip255(vaddq_s16(p1, dp1)), p1);
    let q1w = vbslq_s16(aq, clip255(vaddq_s16(q1, dq1)), q1);
    // bS 4.
    let near = vcltq_s16(vabdq_s16(p0, q0), vdupq_n_s16((l.alpha >> 2) + 2));
    let (sp, sq) = (vandq_u16(ap, near), vandq_u16(aq, near));
    let two = |v: int16x8_t| vshlq_n_s16::<1>(v);
    let p0s = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(vaddq_s16(p2, two(p1)), vaddq_s16(two(p0), two(q0))),
        vaddq_s16(q1, vdupq_n_s16(4)),
    ));
    let p1s = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(vaddq_s16(p2, p1), vaddq_s16(p0, q0)),
        vdupq_n_s16(2),
    ));
    let p2s = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(
            vaddq_s16(two(p3), vaddq_s16(two(p2), p2)),
            vaddq_s16(p1, p0),
        ),
        vaddq_s16(q0, vdupq_n_s16(4)),
    ));
    let p0f = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(two(p1), vaddq_s16(p0, q1)),
        vdupq_n_s16(2),
    ));
    let q0s = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(vaddq_s16(p1, two(p0)), vaddq_s16(two(q0), two(q1))),
        vaddq_s16(q2, vdupq_n_s16(4)),
    ));
    let q1s = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(vaddq_s16(p0, q0), vaddq_s16(q1, q2)),
        vdupq_n_s16(2),
    ));
    let q2s = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(
            vaddq_s16(two(q3), vaddq_s16(two(q2), q2)),
            vaddq_s16(q1, q0),
        ),
        vaddq_s16(p0, vdupq_n_s16(4)),
    ));
    let q0f = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(two(q1), vaddq_s16(q0, p1)),
        vdupq_n_s16(2),
    ));
    // Per lane: the bS 4 result where bS is 4, the bS 1 to 3 one otherwise, the input where nothing is filtered.
    let pick = |s4: int16x8_t, w: int16x8_t, keep: int16x8_t| {
        vbslq_s16(on, vbslq_s16(strong4, s4, w), keep)
    };
    let np0 = pick(vbslq_s16(sp, p0s, p0f), p0w, p0);
    let np1 = pick(vbslq_s16(sp, p1s, p1), p1w, p1);
    let np2 = pick(vbslq_s16(sp, p2s, p2), p2, p2);
    let nq0 = pick(vbslq_s16(sq, q0s, q0f), q0w, q0);
    let nq1 = pick(vbslq_s16(sq, q1s, q1), q1w, q1);
    let nq2 = pick(vbslq_s16(sq, q2s, q2), q2, q2);
    [np2, np1, np0, nq0, nq1, nq2].map(|v| vqmovun_s16(v))
}

/// The chroma filter on eight lines: p1, p0, q0, q1 in, p0 and q0 out.
#[inline(always)]
unsafe fn chroma8(s: [uint8x8_t; 4], l: &Lanes) -> [uint8x8_t; 2] {
    let [p1, p0, q0, q1] = s.map(|v| wide(v));
    let (alpha, beta) = (vdupq_n_s16(l.alpha), vdupq_n_s16(l.beta));
    let (tc, active, strong4) = (
        vld1q_s16(l.tc0.as_ptr()),
        vld1q_u16(l.active.as_ptr()),
        vld1q_u16(l.strong.as_ptr()),
    );
    let on = vandq_u16(
        vandq_u16(
            vcltq_s16(vabdq_s16(p0, q0), alpha),
            vandq_u16(
                vcltq_s16(vabdq_s16(p1, p0), beta),
                vcltq_s16(vabdq_s16(q1, q0), beta),
            ),
        ),
        active,
    );
    let delta = vshrq_n_s16::<3>(vaddq_s16(
        vaddq_s16(vshlq_n_s16::<2>(vsubq_s16(q0, p0)), vsubq_s16(p1, q1)),
        vdupq_n_s16(4),
    ));
    let delta = vminq_s16(vmaxq_s16(delta, vnegq_s16(tc)), tc);
    let (p0w, q0w) = (clip255(vaddq_s16(p0, delta)), clip255(vsubq_s16(q0, delta)));
    let two = |v: int16x8_t| vshlq_n_s16::<1>(v);
    let p0s = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(two(p1), vaddq_s16(p0, q1)),
        vdupq_n_s16(2),
    ));
    let q0s = vshrq_n_s16::<2>(vaddq_s16(
        vaddq_s16(two(q1), vaddq_s16(q0, p1)),
        vdupq_n_s16(2),
    ));
    let np0 = vbslq_s16(on, vbslq_s16(strong4, p0s, p0w), p0);
    let nq0 = vbslq_s16(on, vbslq_s16(strong4, q0s, q0w), q0);
    [vqmovun_s16(np0), vqmovun_s16(nq0)]
}

/// Luma edge across rows (a horizontal edge), eight columns from `o`
/// (the first q0 sample): reads rows -4 to 3 around the edge, writes -3 to 2.
///
/// # Safety
/// Rows `o - 4 * stride` to `o + 3 * stride`, eight bytes from `o`, are inside the plane.
pub unsafe fn luma_h8(d: *mut u8, o: usize, stride: usize, l: &Lanes) {
    let at = |k: isize| d.offset(o as isize + k * stride as isize);
    let s = [-4isize, -3, -2, -1, 0, 1, 2, 3].map(|k| vld1_u8(at(k)));
    let r = luma8(s, l);
    for (k, v) in r.into_iter().enumerate() {
        vst1_u8(at(k as isize - 3), v);
    }
}

/// Luma edge across columns (a vertical edge), eight rows from `o` (the
/// q0 sample of the first one).
///
/// # Safety
/// Columns -4 to 3 around the edge of the eight rows are inside the plane.
pub unsafe fn luma_v8(d: *mut u8, o: usize, stride: usize, l: &Lanes) {
    let row = |i: usize| d.add(o + i * stride - 4);
    let rows: [uint8x8_t; 8] = core::array::from_fn(|i| vld1_u8(row(i)));
    let c = transpose8(rows);
    let r = luma8(c, l);
    let back = transpose8([c[0], r[0], r[1], r[2], r[3], r[4], r[5], c[7]]);
    for (i, v) in back.into_iter().enumerate() {
        vst1_u8(row(i), v);
    }
}

/// Chroma edge across rows, eight columns.
///
/// # Safety
/// Rows -2 to 1 around the edge, eight bytes from `o`, are inside the plane.
pub unsafe fn chroma_h8(d: *mut u8, o: usize, stride: usize, l: &Lanes) {
    let at = |k: isize| d.offset(o as isize + k * stride as isize);
    let s = [-2isize, -1, 0, 1].map(|k| vld1_u8(at(k)));
    let [p0, q0] = chroma8(s, l);
    vst1_u8(at(-1), p0);
    vst1_u8(at(0), q0);
}

/// Chroma edge across columns, eight rows.
///
/// # Safety
/// Columns -4 to 3 around the edge of the eight rows are inside the plane.
pub unsafe fn chroma_v8(d: *mut u8, o: usize, stride: usize, l: &Lanes) {
    let row = |i: usize| d.add(o + i * stride - 4);
    let rows: [uint8x8_t; 8] = core::array::from_fn(|i| vld1_u8(row(i)));
    let c = transpose8(rows);
    let [p0, q0] = chroma8([c[2], c[3], c[4], c[5]], l);
    // Only p0 and q0 change: their eight rows go back one byte each.
    let (mut np0, mut nq0) = ([0u8; 8], [0u8; 8]);
    vst1_u8(np0.as_mut_ptr(), p0);
    vst1_u8(nq0.as_mut_ptr(), q0);
    for i in 0..8 {
        let p = d.add(o + i * stride);
        *p.sub(1) = np0[i];
        *p = nq0[i];
    }
}

// ---- the inverse transforms

#[inline(always)]
unsafe fn transpose4(r: [int32x4_t; 4]) -> [int32x4_t; 4] {
    let t0 = vtrnq_s32(r[0], r[1]);
    let t1 = vtrnq_s32(r[2], r[3]);
    [
        vcombine_s32(vget_low_s32(t0.0), vget_low_s32(t1.0)),
        vcombine_s32(vget_low_s32(t0.1), vget_low_s32(t1.1)),
        vcombine_s32(vget_high_s32(t0.0), vget_high_s32(t1.0)),
        vcombine_s32(vget_high_s32(t0.1), vget_high_s32(t1.1)),
    ]
}

/// The 4-point inverse transform across four vectors (lane `j` of each is one column's input).
#[inline(always)]
unsafe fn butterfly4(a: [int32x4_t; 4]) -> [int32x4_t; 4] {
    let e0 = vaddq_s32(a[0], a[2]);
    let e1 = vsubq_s32(a[0], a[2]);
    let e2 = vsubq_s32(vshrq_n_s32::<1>(a[1]), a[3]);
    let e3 = vaddq_s32(a[1], vshrq_n_s32::<1>(a[3]));
    [
        vaddq_s32(e0, e3),
        vaddq_s32(e1, e2),
        vsubq_s32(e1, e2),
        vsubq_s32(e0, e3),
    ]
}

/// Inverse 4x4 transform of scaled levels (raster order), added to the
/// samples at `dst`, rows `stride` apart.
///
/// # Safety
/// Four rows of four bytes from `dst` are valid for reading and writing.
pub unsafe fn idct4_add(c: &[i32; 16], dst: *mut u8, stride: usize) {
    let r = [0, 4, 8, 12].map(|i| vld1q_s32(c.as_ptr().add(i)));
    // Rows first (across the columns, so transposed), then the columns.
    let rows = transpose4(butterfly4(transpose4(r)));
    let h = butterfly4(rows);
    for (i, v) in h.into_iter().enumerate() {
        let p = dst.add(i * stride);
        let res = vmovn_s32(vrshrq_n_s32::<6>(v));
        let pred = vreinterpretq_s16_u16(vmovl_u8(vreinterpret_u8_u32(vdup_n_u32(
            (p as *const u32).read_unaligned(),
        ))));
        let sum = vqmovun_s16(vaddq_s16(pred, vcombine_s16(res, res)));
        (p as *mut u32).write_unaligned(vget_lane_u32::<0>(vreinterpret_u32_u8(sum)));
    }
}

#[inline(always)]
unsafe fn transpose8_i32(m: [[int32x4_t; 2]; 8]) -> [[int32x4_t; 2]; 8] {
    let a = transpose4([m[0][0], m[1][0], m[2][0], m[3][0]]);
    let b = transpose4([m[0][1], m[1][1], m[2][1], m[3][1]]);
    let c = transpose4([m[4][0], m[5][0], m[6][0], m[7][0]]);
    let d = transpose4([m[4][1], m[5][1], m[6][1], m[7][1]]);
    [
        [a[0], c[0]],
        [a[1], c[1]],
        [a[2], c[2]],
        [a[3], c[3]],
        [b[0], d[0]],
        [b[1], d[1]],
        [b[2], d[2]],
        [b[3], d[3]],
    ]
}

/// The 8-point inverse transform across eight vectors.
#[inline(always)]
unsafe fn butterfly8(d: [int32x4_t; 8]) -> [int32x4_t; 8] {
    let sh1 = |v: int32x4_t| vshrq_n_s32::<1>(v);
    let sh2 = |v: int32x4_t| vshrq_n_s32::<2>(v);
    let a0 = vaddq_s32(d[0], d[4]);
    let a4 = vsubq_s32(d[0], d[4]);
    let a2 = vsubq_s32(sh1(d[2]), d[6]);
    let a6 = vaddq_s32(d[2], sh1(d[6]));
    let b0 = vaddq_s32(a0, a6);
    let b2 = vaddq_s32(a4, a2);
    let b4 = vsubq_s32(a4, a2);
    let b6 = vsubq_s32(a0, a6);
    // a1 = -d3 + d5 - d7 - (d7 >> 1); a3 = d1 + d7 - d3 - (d3 >> 1);
    // a5 = -d1 + d7 + d5 + (d5 >> 1); a7 = d3 + d5 + d1 + (d1 >> 1).
    let a1 = vsubq_s32(vsubq_s32(vaddq_s32(vnegq_s32(d[3]), d[5]), d[7]), sh1(d[7]));
    let a3 = vsubq_s32(vsubq_s32(vaddq_s32(d[1], d[7]), d[3]), sh1(d[3]));
    let a5 = vaddq_s32(vaddq_s32(vaddq_s32(vnegq_s32(d[1]), d[7]), d[5]), sh1(d[5]));
    let a7 = vaddq_s32(vaddq_s32(vaddq_s32(d[3], d[5]), d[1]), sh1(d[1]));
    let b1 = vaddq_s32(a1, sh2(a7));
    let b7 = vsubq_s32(a7, sh2(a1));
    let b3 = vaddq_s32(a3, sh2(a5));
    let b5 = vsubq_s32(sh2(a3), a5);
    [
        vaddq_s32(b0, b7),
        vaddq_s32(b2, b5),
        vaddq_s32(b4, b3),
        vaddq_s32(b6, b1),
        vsubq_s32(b6, b1),
        vsubq_s32(b4, b3),
        vsubq_s32(b2, b5),
        vsubq_s32(b0, b7),
    ]
}

/// Inverse 8x8 transform of scaled levels (raster order), added to the
/// samples at `dst`, rows `stride` apart.
///
/// # Safety
/// Eight rows of eight bytes from `dst` are valid for reading and writing.
pub unsafe fn idct8_add(c: &[i32; 64], dst: *mut u8, stride: usize) {
    let m: [[int32x4_t; 2]; 8] = core::array::from_fn(|i| {
        [
            vld1q_s32(c.as_ptr().add(i * 8)),
            vld1q_s32(c.as_ptr().add(i * 8 + 4)),
        ]
    });
    // Rows first: transposed, one butterfly over the eight rows of the transpose
    // for each half of the lanes, and back; then the same for the columns.
    let t = transpose8_i32(m);
    let lo = butterfly8(core::array::from_fn(|i| t[i][0]));
    let hi = butterfly8(core::array::from_fn(|i| t[i][1]));
    let r = transpose8_i32(core::array::from_fn(|i| [lo[i], hi[i]]));
    let lo = butterfly8(core::array::from_fn(|i| r[i][0]));
    let hi = butterfly8(core::array::from_fn(|i| r[i][1]));
    for i in 0..8 {
        let p = dst.add(i * stride);
        let res = vcombine_s16(
            vmovn_s32(vrshrq_n_s32::<6>(lo[i])),
            vmovn_s32(vrshrq_n_s32::<6>(hi[i])),
        );
        let pred = vreinterpretq_s16_u16(vmovl_u8(vld1_u8(p)));
        vst1_u8(p, vqmovun_s16(vaddq_s16(pred, res)));
    }
}

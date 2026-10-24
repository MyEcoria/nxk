//! Scaling and inverse transforms (8.5) for flat scaling lists, 8 bits.

/// normAdjust4x4: v[qp % 6][class] with class 0 for (even, even), 1 for
/// (odd, odd), 2 otherwise.
const V4: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

/// normAdjust8x8: v[qp % 6][class], six position classes.
const V8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

const fn class4(i: usize) -> usize {
    let (r, c) = (i >> 2, i & 3);
    if r & 1 == 0 && c & 1 == 0 {
        0
    } else if r & 1 == 1 && c & 1 == 1 {
        1
    } else {
        2
    }
}

const fn class8(i: usize) -> usize {
    let (r, c) = (i >> 3, i & 7);
    if r % 4 == 0 && c % 4 == 0 {
        0
    } else if r % 2 == 1 && c % 2 == 1 {
        1
    } else if r % 4 == 2 && c % 4 == 2 {
        2
    } else if (r % 4 == 0 && c % 2 == 1) || (r % 2 == 1 && c % 4 == 0) {
        3
    } else if (r % 4 == 0 && c % 4 == 2) || (r % 4 == 2 && c % 4 == 0) {
        4
    } else {
        5
    }
}

/// `V4[m][class(i)]` for every raster position: one multiplier a position.
static SCALE4: [[i32; 16]; 6] = {
    let mut t = [[0; 16]; 6];
    let mut m = 0;
    while m < 6 {
        let mut i = 0;
        while i < 16 {
            t[m][i] = V4[m][class4(i)];
            i += 1;
        }
        m += 1;
    }
    t
};

static SCALE8: [[i32; 64]; 6] = {
    let mut t = [[0; 64]; 6];
    let mut m = 0;
    while m < 6 {
        let mut i = 0;
        while i < 64 {
            t[m][i] = V8[m][class8(i)];
            i += 1;
        }
        m += 1;
    }
    t
};

/// Scales a 4x4 block of levels (raster order) in place; with `skip_dc` the
/// first coefficient is left alone (it was scaled with the DC transform).
pub fn scale_4x4(c: &mut [i32; 16], qp: i32, skip_dc: bool) {
    let (m, q) = ((qp % 6) as usize, qp / 6);
    let dc = c[0];
    let t = &SCALE4[m];
    for i in 0..16 {
        c[i] = (c[i] * t[i]) << q;
    }
    if skip_dc {
        c[0] = dc;
    }
}

/// Scales an 8x8 block of levels (raster order) in place.
pub fn scale_8x8(c: &mut [i32; 64], qp: i32) {
    let (m, q) = ((qp % 6) as usize, qp / 6);
    let t = &SCALE8[m];
    if q >= 2 {
        for i in 0..64 {
            c[i] = (c[i] * t[i]) << (q - 2);
        }
    } else {
        for i in 0..64 {
            c[i] = (c[i] * t[i] + (1 << (1 - q))) >> (2 - q);
        }
    }
}

/// Inverse 4x4 transform of scaled levels, added to the samples at `o`.
pub fn idct_4x4_add(c: &[i32; 16], dst: &mut [u8], o: usize, stride: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        assert!(o + 3 * stride + 4 <= dst.len());
        // SAFETY: four rows of four bytes from `o` were just checked to be inside `dst`.
        unsafe { crate::neon::idct4_add(c, dst.as_mut_ptr().add(o), stride) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    idct_4x4_add_portable(c, dst, o, stride)
}

/// The portable inverse 4x4 transform (the oracle of the NEON one).
pub fn idct_4x4_add_portable(c: &[i32; 16], dst: &mut [u8], o: usize, stride: usize) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let r = &c[i * 4..i * 4 + 4];
        let e0 = r[0] + r[2];
        let e1 = r[0] - r[2];
        let e2 = (r[1] >> 1) - r[3];
        let e3 = r[1] + (r[3] >> 1);
        t[i * 4] = e0 + e3;
        t[i * 4 + 1] = e1 + e2;
        t[i * 4 + 2] = e1 - e2;
        t[i * 4 + 3] = e0 - e3;
    }
    for j in 0..4 {
        let (a, b, cc, d) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let e0 = a + cc;
        let e1 = a - cc;
        let e2 = (b >> 1) - d;
        let e3 = b + (d >> 1);
        let col = [e0 + e3, e1 + e2, e1 - e2, e0 - e3];
        for (i, v) in col.iter().enumerate() {
            let p = &mut dst[o + i * stride + j];
            *p = (*p as i32 + ((v + 32) >> 6)).clamp(0, 255) as u8;
        }
    }
}

#[inline]
fn idct8_1d(d: [i32; 8]) -> [i32; 8] {
    let a0 = d[0] + d[4];
    let a4 = d[0] - d[4];
    let a2 = (d[2] >> 1) - d[6];
    let a6 = d[2] + (d[6] >> 1);
    let b0 = a0 + a6;
    let b2 = a4 + a2;
    let b4 = a4 - a2;
    let b6 = a0 - a6;
    let a1 = -d[3] + d[5] - d[7] - (d[7] >> 1);
    let a3 = d[1] + d[7] - d[3] - (d[3] >> 1);
    let a5 = -d[1] + d[7] + d[5] + (d[5] >> 1);
    let a7 = d[3] + d[5] + d[1] + (d[1] >> 1);
    let b1 = a1 + (a7 >> 2);
    let b7 = a7 - (a1 >> 2);
    let b3 = a3 + (a5 >> 2);
    let b5 = (a3 >> 2) - a5;
    [
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]
}

/// Inverse 8x8 transform of scaled levels, added to the samples at `o`.
pub fn idct_8x8_add(c: &[i32; 64], dst: &mut [u8], o: usize, stride: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        assert!(o + 7 * stride + 8 <= dst.len());
        // SAFETY: eight rows of eight bytes from `o` were just checked to be inside `dst`.
        unsafe { crate::neon::idct8_add(c, dst.as_mut_ptr().add(o), stride) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    idct_8x8_add_portable(c, dst, o, stride)
}

/// The portable inverse 8x8 transform (the oracle of the NEON one).
pub fn idct_8x8_add_portable(c: &[i32; 64], dst: &mut [u8], o: usize, stride: usize) {
    let mut t = [0i32; 64];
    for i in 0..8 {
        let r: [i32; 8] = c[i * 8..i * 8 + 8].try_into().unwrap();
        t[i * 8..i * 8 + 8].copy_from_slice(&idct8_1d(r));
    }
    for j in 0..8 {
        let col = idct8_1d([
            t[j],
            t[8 + j],
            t[16 + j],
            t[24 + j],
            t[32 + j],
            t[40 + j],
            t[48 + j],
            t[56 + j],
        ]);
        for (i, v) in col.iter().enumerate() {
            let p = &mut dst[o + i * stride + j];
            *p = (*p as i32 + ((v + 32) >> 6)).clamp(0, 255) as u8;
        }
    }
}

/// Intra 16x16 luma DC: the 16 levels (raster order of the DC matrix) go
/// through the 4x4 Hadamard transform and are scaled: the result is the DC of
/// each 4x4 block in raster order of the blocks.
pub fn luma_dc(levels: &[i32; 16], qp: i32) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let r = &levels[i * 4..i * 4 + 4];
        let (a, b, c, d) = (r[0] + r[1], r[0] - r[1], r[2] + r[3], r[2] - r[3]);
        // Row of A = [1 1 1 1; 1 1 -1 -1; 1 -1 -1 1; 1 -1 1 -1] applied to the row.
        t[i * 4] = a + c;
        t[i * 4 + 1] = a - c;
        t[i * 4 + 2] = b - d;
        t[i * 4 + 3] = b + d;
    }
    let mut out = [0i32; 16];
    let ls = 16 * V4[(qp % 6) as usize][0];
    let q = qp / 6;
    for j in 0..4 {
        let (r0, r1, r2, r3) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let (a, b, c, d) = (r0 + r1, r0 - r1, r2 + r3, r2 - r3);
        let f = [a + c, a - c, b - d, b + d];
        for (i, &v) in f.iter().enumerate() {
            out[i * 4 + j] = if q >= 6 {
                (v * ls) << (q - 6)
            } else {
                (v * ls + (1 << (5 - q))) >> (6 - q)
            };
        }
    }
    out
}

/// Chroma DC (2x2): levels in block order, scaled with the chroma QP.
pub fn chroma_dc(levels: &[i32; 4], qpc: i32) -> [i32; 4] {
    let [a, b, c, d] = *levels;
    let f = [a + b + c + d, a - b + c - d, a + b - c - d, a - b - c + d];
    let ls = 16 * V4[(qpc % 6) as usize][0];
    f.map(|v| ((v * ls) << (qpc / 6)) >> 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_dc_makes_a_flat_block() {
        let mut c = [0i32; 16];
        c[0] = 64 * 4; // after scaling: (64*4 + 32) >> 6 = 4
        let mut dst = vec![100u8; 16];
        idct_4x4_add(&c, &mut dst, 0, 4);
        assert!(dst.iter().all(|&v| v == 104));
        let mut c8 = [0i32; 64];
        c8[0] = 64 * 3;
        let mut dst = vec![10u8; 64];
        idct_8x8_add(&c8, &mut dst, 0, 8);
        assert!(dst.iter().all(|&v| v == 13));
    }

    #[test]
    fn the_samples_stay_in_range() {
        let mut c = [0i32; 16];
        c[0] = 1 << 20;
        let mut dst = vec![200u8; 16];
        idct_4x4_add(&c, &mut dst, 0, 4);
        assert!(dst.iter().all(|&v| v == 255));
        c[0] = -(1 << 20);
        idct_4x4_add(&c, &mut dst, 0, 4);
        assert!(dst.iter().all(|&v| v == 0));
    }

    #[test]
    fn scaling_follows_the_qp_classes() {
        let mut c = [1i32; 16];
        scale_4x4(&mut c, 0, false);
        assert_eq!((c[0], c[5], c[1]), (10, 16, 13));
        let mut c = [1i32; 16];
        scale_4x4(&mut c, 12, true);
        assert_eq!((c[0], c[1]), (1, 13 << 2));
        let mut c = [4i32; 64];
        scale_8x8(&mut c, 0);
        // (4 * 20 + 2) >> 2 = 20 at the (0, 0) class
        assert_eq!(c[0], 20);
        let mut c = [1i32; 64];
        scale_8x8(&mut c, 18);
        assert_eq!(c[0], 20 << 1);
    }

    #[test]
    fn the_dc_transforms_of_a_lone_level_spread_it_evenly() {
        let mut l = [0i32; 16];
        l[0] = 8;
        let d = luma_dc(&l, 36);
        assert!(d.iter().all(|&v| v == d[0]));
        assert_eq!(d[0], 8 * 16 * 10);
        let c = chroma_dc(&[8, 0, 0, 0], 12);
        assert!(c.iter().all(|&v| v == c[0]));
    }
}

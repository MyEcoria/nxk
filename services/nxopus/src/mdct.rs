// Ported from libopus 1.6.1, celt/mdct.c and celt/kiss_fft.c (float build).
// Copyright 2003-2004 Mark Borgerding, 2007-2008 CSIRO, 2007-2009 Xiph.Org
// Foundation, 2008 Gregory Maxwell, Jean-Marc Valin. BSD-3-Clause, see
// LICENSE in this crate.

//! The inverse MDCT (sizes 1920 >> shift) over a mixed-radix FFT, with the
//! TDAC windowing done in place in the decoder's history buffer.

use crate::tables::{
    FFT_BITREV120, FFT_BITREV240, FFT_BITREV480, FFT_BITREV60, FFT_TWIDDLES, MDCT_TWIDDLES,
};

#[derive(Clone, Copy, Default)]
struct Cpx {
    r: f32,
    i: f32,
}

fn cmul(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r * b.r - a.i * b.i,
        i: a.r * b.i + a.i * b.r,
    }
}

fn add(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r + b.r,
        i: a.i + b.i,
    }
}

fn sub(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r - b.r,
        i: a.i - b.i,
    }
}

fn twiddle(k: usize) -> Cpx {
    Cpx {
        r: FFT_TWIDDLES[2 * k],
        i: FFT_TWIDDLES[2 * k + 1],
    }
}

/// One FFT size: radices with their sub-lengths, and its bit-reversal.
struct FftState {
    shift: usize,
    factors: &'static [usize],
    bitrev: &'static [i16],
}

static FFTS: [FftState; 4] = [
    FftState {
        shift: 0,
        factors: &[5, 96, 3, 32, 4, 8, 2, 4, 4, 1],
        bitrev: &FFT_BITREV480,
    },
    FftState {
        shift: 1,
        factors: &[5, 48, 3, 16, 4, 4, 4, 1],
        bitrev: &FFT_BITREV240,
    },
    FftState {
        shift: 2,
        factors: &[5, 24, 3, 8, 2, 4, 4, 1],
        bitrev: &FFT_BITREV120,
    },
    FftState {
        shift: 3,
        factors: &[5, 12, 3, 4, 4, 1],
        bitrev: &FFT_BITREV60,
    },
];

fn bfly2(f: &mut [Cpx], n: usize) {
    let tw = 0.707_106_77f32;
    for blk in 0..n {
        let base = blk * 8;
        for k in 0..4 {
            let a = f[base + k];
            let b = f[base + 4 + k];
            let t = match k {
                0 => b,
                1 => Cpx {
                    r: (b.r + b.i) * tw,
                    i: (b.i - b.r) * tw,
                },
                2 => Cpx { r: b.i, i: -b.r },
                _ => Cpx {
                    r: (b.i - b.r) * tw,
                    i: -(b.i + b.r) * tw,
                },
            };
            f[base + 4 + k] = sub(a, t);
            f[base + k] = add(a, t);
        }
    }
}

fn bfly4(f: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    if m == 1 {
        for blk in 0..n {
            let o = blk * 4;
            let scratch0 = sub(f[o], f[o + 2]);
            f[o] = add(f[o], f[o + 2]);
            let mut scratch1 = add(f[o + 1], f[o + 3]);
            f[o + 2] = sub(f[o], scratch1);
            f[o] = add(f[o], scratch1);
            scratch1 = sub(f[o + 1], f[o + 3]);
            f[o + 1] = Cpx {
                r: scratch0.r + scratch1.i,
                i: scratch0.i - scratch1.r,
            };
            f[o + 3] = Cpx {
                r: scratch0.r - scratch1.i,
                i: scratch0.i + scratch1.r,
            };
        }
        return;
    }
    let (m2, m3) = (2 * m, 3 * m);
    for blk in 0..n {
        for j in 0..m {
            let o = blk * mm + j;
            let s0 = cmul(f[o + m], twiddle(j * fstride));
            let s1 = cmul(f[o + m2], twiddle(j * fstride * 2));
            let s2 = cmul(f[o + m3], twiddle(j * fstride * 3));
            let s5 = sub(f[o], s1);
            f[o] = add(f[o], s1);
            let s3 = add(s0, s2);
            let s4 = sub(s0, s2);
            f[o + m2] = sub(f[o], s3);
            f[o] = add(f[o], s3);
            f[o + m] = Cpx {
                r: s5.r + s4.i,
                i: s5.i - s4.r,
            };
            f[o + m3] = Cpx {
                r: s5.r - s4.i,
                i: s5.i + s4.r,
            };
        }
    }
}

fn bfly3(f: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    let m2 = 2 * m;
    let epi3 = twiddle(fstride * m);
    for blk in 0..n {
        for k in 0..m {
            let o = blk * mm + k;
            let s1 = cmul(f[o + m], twiddle(k * fstride));
            let s2 = cmul(f[o + m2], twiddle(k * fstride * 2));
            let s3 = add(s1, s2);
            let mut s0 = sub(s1, s2);
            let fm = Cpx {
                r: f[o].r - s3.r * 0.5,
                i: f[o].i - s3.i * 0.5,
            };
            s0.r *= epi3.i;
            s0.i *= epi3.i;
            f[o] = add(f[o], s3);
            f[o + m2] = Cpx {
                r: fm.r + s0.i,
                i: fm.i - s0.r,
            };
            f[o + m] = Cpx {
                r: fm.r - s0.i,
                i: fm.i + s0.r,
            };
        }
    }
}

fn bfly5(f: &mut [Cpx], fstride: usize, m: usize, n: usize, mm: usize) {
    let ya = twiddle(fstride * m);
    let yb = twiddle(fstride * 2 * m);
    for blk in 0..n {
        let o = blk * mm;
        for u in 0..m {
            let (i0, i1, i2, i3, i4) = (
                o + u,
                o + u + m,
                o + u + 2 * m,
                o + u + 3 * m,
                o + u + 4 * m,
            );
            let s0 = f[i0];
            let s1 = cmul(f[i1], twiddle(u * fstride));
            let s2 = cmul(f[i2], twiddle(2 * u * fstride));
            let s3 = cmul(f[i3], twiddle(3 * u * fstride));
            let s4 = cmul(f[i4], twiddle(4 * u * fstride));
            let s7 = add(s1, s4);
            let s10 = sub(s1, s4);
            let s8 = add(s2, s3);
            let s9 = sub(s2, s3);
            f[i0] = Cpx {
                r: f[i0].r + (s7.r + s8.r),
                i: f[i0].i + (s7.i + s8.i),
            };
            let s5 = Cpx {
                r: s0.r + (s7.r * ya.r + s8.r * yb.r),
                i: s0.i + (s7.i * ya.r + s8.i * yb.r),
            };
            let s6 = Cpx {
                r: s10.i * ya.i + s9.i * yb.i,
                i: -(s10.r * ya.i + s9.r * yb.i),
            };
            f[i1] = sub(s5, s6);
            f[i4] = add(s5, s6);
            let s11 = Cpx {
                r: s0.r + (s7.r * yb.r + s8.r * ya.r),
                i: s0.i + (s7.i * yb.r + s8.i * ya.r),
            };
            let s12 = Cpx {
                r: s9.i * ya.i - s10.i * yb.i,
                i: s10.r * yb.i - s9.r * ya.i,
            };
            f[i2] = add(s11, s12);
            f[i3] = sub(s11, s12);
        }
    }
}

fn fft(st: &FftState, f: &mut [Cpx]) {
    let mut fstride = [1usize; 8];
    let mut l = 0;
    loop {
        let (p, m) = (st.factors[2 * l], st.factors[2 * l + 1]);
        fstride[l + 1] = fstride[l] * p;
        l += 1;
        if m == 1 {
            break;
        }
    }
    let mut m = st.factors[2 * l - 1];
    for i in (0..l).rev() {
        let m2 = if i != 0 { st.factors[2 * i - 1] } else { 1 };
        let tw_stride = fstride[i] << st.shift;
        match st.factors[2 * i] {
            2 => bfly2(f, fstride[i]),
            4 => bfly4(f, tw_stride, m, fstride[i], m2),
            3 => bfly3(f, tw_stride, m, fstride[i], m2),
            _ => bfly5(f, tw_stride, m, fstride[i], m2),
        }
        m = m2;
    }
}

/// `clt_mdct_backward`: coefficients `input[0], input[stride], ...` to
/// `out[..overlap/2 + n/2]`, overlap-added with what `out` already holds.
pub fn imdct(
    input: &[f32],
    out: &mut [f32],
    window: &[f32],
    overlap: usize,
    shift: usize,
    stride: usize,
) {
    let mut n = 1920usize;
    let mut trig = &MDCT_TWIDDLES[..];
    for _ in 0..shift {
        n >>= 1;
        trig = &trig[n..];
    }
    let n2 = n >> 1;
    let n4 = n >> 2;
    let st = &FFTS[shift];
    let mut buf = [Cpx::default(); 480];
    for i in 0..n4 {
        let rev = st.bitrev[i] as usize;
        let x1 = input[2 * stride * i];
        let x2 = input[stride * (n2 - 1) - 2 * stride * i];
        let yr = x2 * trig[i] + x1 * trig[n4 + i];
        let yi = x1 * trig[i] - x2 * trig[n4 + i];
        buf[rev] = Cpx { r: yi, i: yr };
    }
    fft(st, &mut buf[..n4]);
    let y = &mut out[overlap >> 1..];
    for i in 0..(n4 + 1) >> 1 {
        let (a, b) = (buf[i], buf[n4 - 1 - i]);
        let (t0, t1) = (trig[i], trig[n4 + i]);
        let (re, im) = (a.i, a.r);
        let yr = re * t0 + im * t1;
        let yi = re * t1 - im * t0;
        let (re, im) = (b.i, b.r);
        let p0 = 2 * i;
        let p1 = n2 - 2 - 2 * i;
        y[p0] = yr;
        y[p1 + 1] = yi;
        let (t0, t1) = (trig[n4 - i - 1], trig[n2 - i - 1]);
        let yr = re * t0 + im * t1;
        let yi = re * t1 - im * t0;
        y[p1] = yr;
        y[p0 + 1] = yi;
    }
    for i in 0..overlap / 2 {
        let (wp1, wp2) = (window[i], window[overlap - 1 - i]);
        let x1 = out[overlap - 1 - i];
        let x2 = out[i];
        out[i] = x2 * wp2 - x1 * wp1;
        out[overlap - 1 - i] = x2 * wp1 + x1 * wp2;
    }
}

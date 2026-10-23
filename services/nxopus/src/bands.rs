// Ported from libopus 1.6.1, celt/bands.c (float build, decoder side).
// Copyright 2007-2009 Xiph.Org Foundation, 2007-2009 Timothy B. Terriberry,
// 2008-2009 Gregory Maxwell, Jean-Marc Valin. BSD-3-Clause, see LICENSE.

//! Band shapes (RFC 6716 §4.3.4): splitting, stereo, folding, the
//! anti-collapse noise and the scaling back to MDCT coefficients.
//!
//! libopus lets the folding source alias the output buffers; here it is
//! always copied to a local buffer first, which gives the same values.

use crate::mode::{NB_EBANDS, SHORT_MDCT_SIZE};
use crate::range::{ilog, RangeDecoder, BITRES};
use crate::rate::{bits2pulses, get_pulses, pulses2bits, QTHETA_OFFSET, QTHETA_OFFSET_TWOPHASE};
use crate::tables::{CACHE_BITS, CACHE_INDEX, EBANDS, E_MEANS, LOGN};
use crate::vq::{alg_unquant, inner_prod, renormalise_vector};

pub const SPREAD_AGGRESSIVE: i32 = 3;
/// The widest band (22 bins at LM 3).
const MAX_BAND: usize = 176;

pub fn lcg_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

fn bitexact_cos(x: i32) -> i32 {
    let tmp = (4096 + x * x) >> 13;
    let x2 = tmp;
    let x2 = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + x2
}

fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ilog(icos as u32);
    let ls = ilog(isin as u32);
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

fn isqrt32(mut val: u32) -> u32 {
    let mut g = 0u32;
    let mut bshift = (ilog(val) - 1) >> 1;
    let mut b = 1u32 << bshift;
    loop {
        let t = ((g << 1) + b) << bshift;
        if t <= val {
            g += b;
            val -= t;
        }
        b >>= 1;
        bshift -= 1;
        if bshift < 0 {
            return g;
        }
    }
}

/// Band shapes times their energies: the MDCT coefficients of one channel.
pub fn denormalise_bands(
    x: &[f32],
    freq: &mut [f32],
    band_log_e: &[f32],
    start: usize,
    end: usize,
    m: usize,
    silence: bool,
) {
    let n = m * SHORT_MDCT_SIZE;
    let (mut start, mut end) = (start, end);
    let mut bound = m * EBANDS[end] as usize;
    if silence {
        bound = 0;
        start = 0;
        end = 0;
    }
    let first = m * EBANDS[start] as usize;
    if start != 0 {
        freq[..first].fill(0.0);
    }
    for i in start..end {
        let (j, band_end) = (m * EBANDS[i] as usize, m * EBANDS[i + 1] as usize);
        let lg = band_log_e[i] + E_MEANS[i];
        let g = crate::math::exp2(lg.min(32.0));
        for k in j..band_end {
            freq[k] = x[k] * g;
        }
    }
    freq[bound..n].fill(0.0);
}

/// Noise for the short blocks a transient left empty.
#[allow(clippy::too_many_arguments)]
pub fn anti_collapse(
    x: &mut [&mut [f32]],
    collapse_masks: &[u8],
    lm: i32,
    c_count: usize,
    start: usize,
    end: usize,
    log_e: &[f32],
    prev1_log_e: &[f32],
    prev2_log_e: &[f32],
    pulses: &[i32],
    mut seed: u32,
) {
    for i in start..end {
        let n0 = (EBANDS[i + 1] - EBANDS[i]) as usize;
        let depth = (((1 + pulses[i]) as u32 / n0 as u32) as i32) >> lm;
        let thresh = 0.5 * crate::math::exp2(-0.125 * depth as f32);
        let sqrt_1 = 1.0 / ((n0 << lm) as f32).sqrt();
        for c in 0..c_count {
            let mut prev1 = prev1_log_e[c * NB_EBANDS + i];
            let mut prev2 = prev2_log_e[c * NB_EBANDS + i];
            if c_count == 1 {
                prev1 = prev1.max(prev1_log_e[NB_EBANDS + i]);
                prev2 = prev2.max(prev2_log_e[NB_EBANDS + i]);
            }
            let ediff = (log_e[c * NB_EBANDS + i] - prev1.min(prev2)).max(0.0);
            let mut r = 2.0 * crate::math::exp2(-ediff);
            if lm == 3 {
                r *= std::f32::consts::SQRT_2;
            }
            r = thresh.min(r) * sqrt_1;
            let band = &mut x[c][(EBANDS[i] as usize) << lm..];
            let mut renormalize = false;
            for k in 0..1usize << lm {
                if collapse_masks[i * c_count + c] as u32 & 1 << k == 0 {
                    for j in 0..n0 {
                        seed = lcg_rand(seed);
                        band[(j << lm) + k] = if seed & 0x8000 != 0 { r } else { -r };
                    }
                    renormalize = true;
                }
            }
            if renormalize {
                renormalise_vector(band, n0 << lm, 1.0);
            }
        }
    }
}

fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: usize) {
    let xp = inner_prod(&y[..n], &x[..n]) * mid;
    let side = inner_prod(&y[..n], &y[..n]);
    let el = mid * mid + side - 2.0 * xp;
    let er = mid * mid + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y[..n].copy_from_slice(&x[..n]);
        return;
    }
    let lgain = 1.0 / el.sqrt();
    let rgain = 1.0 / er.sqrt();
    for j in 0..n {
        let l = mid * x[j];
        let r = y[j];
        x[j] = lgain * (l - r);
        y[j] = rgain * (l + r);
    }
}

const ORDERY_TABLE: [usize; 30] = [
    1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

fn deinterleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0f32; MAX_BAND];
    for i in 0..stride {
        let row = if hadamard {
            ORDERY_TABLE[stride - 2 + i]
        } else {
            i
        };
        for j in 0..n0 {
            tmp[row * n0 + j] = x[j * stride + i];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn interleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0f32; MAX_BAND];
    for i in 0..stride {
        let row = if hadamard {
            ORDERY_TABLE[stride - 2 + i]
        } else {
            i
        };
        for j in 0..n0 {
            tmp[j * stride + i] = x[row * n0 + j];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    const S: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for i in 0..stride {
        for j in 0..n0 >> 1 {
            let tmp1 = S * x[stride * 2 * j + i];
            let tmp2 = S * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = tmp1 + tmp2;
            x[stride * (2 * j + 1) + i] = tmp1 - tmp2;
        }
    }
}

fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let mut qb = (b + n2 * offset) / n2;
    qb = qb.min(b - pulse_cap - (4 << BITRES));
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 7) as usize] >> (14 - (qb >> BITRES));
        (qn + 1) >> 1 << 1
    }
}

/// What the whole frame shares while its bands are decoded.
pub struct BandCtx<'a, 'b> {
    pub dec: &'a mut RangeDecoder<'b>,
    pub i: usize,
    pub intensity: usize,
    pub spread: i32,
    pub tf_change: i32,
    pub remaining_bits: i32,
    pub seed: u32,
}

struct Split {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

#[allow(clippy::too_many_arguments)]
fn compute_theta(
    ctx: &mut BandCtx,
    n: i32,
    b: &mut i32,
    bb: i32,
    b0: i32,
    lm: i32,
    stereo: bool,
    fill: &mut i32,
) -> Split {
    let i = ctx.i;
    let pulse_cap = LOGN[i] as i32 + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1)
        - if stereo && n == 2 {
            QTHETA_OFFSET_TWOPHASE
        } else {
            QTHETA_OFFSET
        };
    let mut qn = compute_qn(n, *b, offset, pulse_cap, stereo);
    if stereo && i >= ctx.intensity {
        qn = 1;
    }
    let tell = ctx.dec.tell_frac() as i32;
    let mut itheta = 0;
    let mut inv = false;
    let dec = &mut *ctx.dec;
    if qn != 1 {
        if stereo && n > 2 {
            let p0 = 3;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let fs = dec.decode(ft as u32) as i32;
            let x = if fs < (x0 + 1) * p0 {
                fs / p0
            } else {
                x0 + 1 + (fs - (x0 + 1) * p0)
            };
            let (fl, fh) = if x <= x0 {
                (p0 * x, p0 * (x + 1))
            } else {
                ((x - 1 - x0) + (x0 + 1) * p0, (x - x0) + (x0 + 1) * p0)
            };
            dec.update(fl as u32, fh as u32, ft as u32);
            itheta = x;
        } else if b0 > 1 || stereo {
            itheta = dec.uint((qn + 1) as u32) as i32;
        } else {
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let fm = dec.decode(ft as u32) as i32;
            let (fs, fl);
            if fm < (((qn >> 1) * ((qn >> 1) + 1)) >> 1) {
                itheta = ((isqrt32(8 * fm as u32 + 1) - 1) >> 1) as i32;
                fs = itheta + 1;
                fl = (itheta * (itheta + 1)) >> 1;
            } else {
                itheta = (2 * (qn + 1) - isqrt32(8 * (ft - fm - 1) as u32 + 1) as i32) >> 1;
                fs = qn + 1 - itheta;
                fl = ft - (((qn + 1 - itheta) * (qn + 2 - itheta)) >> 1);
            }
            dec.update(fl as u32, (fl + fs) as u32, ft as u32);
        }
        itheta = ((itheta as u32 * 16384) / qn as u32) as i32;
    } else if stereo {
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            inv = dec.bit_logp(2);
        }
        itheta = 0;
    }
    let qalloc = ctx.dec.tell_frac() as i32 - tell;
    *b -= qalloc;
    let (imid, iside, delta);
    if itheta == 0 {
        imid = 32767;
        iside = 0;
        *fill &= (1 << bb) - 1;
        delta = -16384;
    } else if itheta == 16384 {
        imid = 0;
        iside = 32767;
        *fill &= ((1 << bb) - 1) << bb;
        delta = 16384;
    } else {
        imid = bitexact_cos(itheta);
        iside = bitexact_cos(16384 - itheta);
        delta = frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid));
    }
    Split {
        inv,
        imid,
        iside,
        delta,
        itheta,
        qalloc,
    }
}

fn quant_band_n1(
    ctx: &mut BandCtx,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    lowband_out: Option<&mut [f32]>,
) -> u32 {
    let one = |ctx: &mut BandCtx, v: &mut [f32]| {
        let mut sign = 0;
        if ctx.remaining_bits >= 1 << BITRES {
            sign = ctx.dec.bits(1);
            ctx.remaining_bits -= 1 << BITRES;
        }
        v[0] = if sign != 0 { -1.0 } else { 1.0 };
    };
    one(ctx, x);
    if let Some(y) = y {
        one(ctx, y);
    }
    if let Some(out) = lowband_out {
        out[0] = x[0];
    }
    1
}

#[allow(clippy::too_many_arguments)]
fn quant_partition(
    ctx: &mut BandCtx,
    x: &mut [f32],
    n: usize,
    b: i32,
    bb: i32,
    lowband: Option<&[f32]>,
    lm: i32,
    gain: f32,
    fill: i32,
) -> u32 {
    let i = ctx.i;
    let cache = &CACHE_BITS[CACHE_INDEX[((lm + 1) as usize) * NB_EBANDS + i] as usize..];
    let b0 = bb;
    if lm != -1 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
        let n = n >> 1;
        let (xs, ys) = x.split_at_mut(n);
        let lm = lm - 1;
        let mut fill = fill;
        if bb == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        let bb = (bb + 1) >> 1;
        let mut b = b;
        let s = compute_theta(ctx, n as i32, &mut b, bb, b0, lm, false, &mut fill);
        let mid = (1.0 / 32768.0) * s.imid as f32;
        let side = (1.0 / 32768.0) * s.iside as f32;
        let mut delta = s.delta;
        if b0 > 1 && s.itheta & 0x3fff != 0 {
            if s.itheta > 8192 {
                delta -= delta >> (4 - lm);
            } else {
                delta = 0.min(delta + ((n as i32) << BITRES >> (5 - lm)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= s.qalloc;
        let next_lowband2 = lowband.map(|l| &l[n..]);
        let mut rebalance = ctx.remaining_bits;
        let mut cm;
        if mbits >= sbits {
            cm = quant_partition(ctx, xs, n, mbits, bb, lowband, lm, gain * mid, fill);
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && s.itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            cm |= quant_partition(
                ctx,
                ys,
                n,
                sbits,
                bb,
                next_lowband2,
                lm,
                gain * side,
                fill >> bb,
            ) << (b0 >> 1);
        } else {
            cm = quant_partition(
                ctx,
                ys,
                n,
                sbits,
                bb,
                next_lowband2,
                lm,
                gain * side,
                fill >> bb,
            ) << (b0 >> 1);
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && s.itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_partition(ctx, xs, n, mbits, bb, lowband, lm, gain * mid, fill);
        }
        return cm;
    }
    let mut q = bits2pulses(i, lm, b);
    let mut curr_bits = pulses2bits(i, lm, q);
    ctx.remaining_bits -= curr_bits;
    while ctx.remaining_bits < 0 && q > 0 {
        ctx.remaining_bits += curr_bits;
        q -= 1;
        curr_bits = pulses2bits(i, lm, q);
        ctx.remaining_bits -= curr_bits;
    }
    if q != 0 {
        return alg_unquant(x, n, get_pulses(q), ctx.spread, bb as usize, ctx.dec, gain);
    }
    let cm_mask = ((1u64 << bb) - 1) as u32;
    let fill = fill as u32 & cm_mask;
    if fill == 0 {
        x[..n].fill(0.0);
        return 0;
    }
    let cm = match lowband {
        None => {
            for v in &mut x[..n] {
                ctx.seed = lcg_rand(ctx.seed);
                *v = (ctx.seed as i32 >> 20) as f32;
            }
            cm_mask
        }
        Some(lowband) => {
            for j in 0..n {
                ctx.seed = lcg_rand(ctx.seed);
                let tmp = if ctx.seed & 0x8000 != 0 {
                    1.0 / 256.0
                } else {
                    -1.0 / 256.0
                };
                x[j] = lowband[j] + tmp;
            }
            fill
        }
    };
    renormalise_vector(x, n, gain);
    cm
}

#[allow(clippy::too_many_arguments)]
fn quant_band(
    ctx: &mut BandCtx,
    x: &mut [f32],
    n: usize,
    b: i32,
    bb: i32,
    lowband: Option<&[f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    gain: f32,
    fill: i32,
) -> u32 {
    let n0 = n;
    let mut n_b = n;
    let mut b0 = bb;
    let mut bb = bb;
    let mut time_divide = 0;
    let long_blocks = b0 == 1;
    let mut tf_change = ctx.tf_change;
    let mut fill = fill;
    n_b /= bb as usize;
    if n == 1 {
        return quant_band_n1(ctx, x, None, lowband_out);
    }
    let recombine = tf_change.max(0);
    let mut scratch = [0f32; MAX_BAND];
    let lowband: Option<&mut [f32]> = lowband.map(|l| {
        scratch[..n].copy_from_slice(&l[..n]);
        &mut scratch[..n]
    });
    let mut lowband = lowband;
    for k in 0..recombine {
        const BIT_INTERLEAVE_TABLE: [i32; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
        if let Some(l) = lowband.as_deref_mut() {
            haar1(l, n >> k, 1 << k);
        }
        fill = BIT_INTERLEAVE_TABLE[(fill & 0xF) as usize]
            | BIT_INTERLEAVE_TABLE[(fill >> 4) as usize] << 2;
    }
    bb >>= recombine;
    n_b <<= recombine;
    while n_b & 1 == 0 && tf_change < 0 {
        if let Some(l) = lowband.as_deref_mut() {
            haar1(l, n_b, bb as usize);
        }
        fill |= fill << bb;
        bb <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change += 1;
    }
    b0 = bb;
    let n_b0 = n_b;
    if b0 > 1 {
        if let Some(l) = lowband.as_deref_mut() {
            deinterleave_hadamard(l, n_b >> recombine, (b0 << recombine) as usize, long_blocks);
        }
    }
    let mut cm = quant_partition(ctx, x, n, b, bb, lowband.as_deref(), lm, gain, fill);
    if b0 > 1 {
        interleave_hadamard(x, n_b >> recombine, (b0 << recombine) as usize, long_blocks);
    }
    n_b = n_b0;
    bb = b0;
    for _ in 0..time_divide {
        bb >>= 1;
        n_b <<= 1;
        cm |= cm >> bb;
        haar1(x, n_b, bb as usize);
    }
    for k in 0..recombine {
        const BIT_DEINTERLEAVE_TABLE: [u32; 16] = [
            0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3,
            0xFC, 0xFF,
        ];
        cm = BIT_DEINTERLEAVE_TABLE[cm as usize];
        haar1(x, n0 >> k, 1 << k);
    }
    bb <<= recombine;
    if let Some(out) = lowband_out {
        let g = (n0 as f32).sqrt();
        for j in 0..n0 {
            out[j] = g * x[j];
        }
    }
    cm & ((1u32 << bb) - 1)
}

#[allow(clippy::too_many_arguments)]
fn quant_band_stereo(
    ctx: &mut BandCtx,
    x: &mut [f32],
    y: &mut [f32],
    n: usize,
    b: i32,
    bb: i32,
    lowband: Option<&[f32]>,
    lm: i32,
    lowband_out: Option<&mut [f32]>,
    fill: i32,
) -> u32 {
    if n == 1 {
        return quant_band_n1(ctx, x, Some(y), lowband_out);
    }
    let orig_fill = fill;
    let mut fill = fill;
    let mut b = b;
    let s = compute_theta(ctx, n as i32, &mut b, bb, bb, lm, true, &mut fill);
    let mid = (1.0 / 32768.0) * s.imid as f32;
    let side = (1.0 / 32768.0) * s.iside as f32;
    let mut cm;
    if n == 2 {
        let mut mbits = b;
        let mut sbits = 0;
        if s.itheta != 0 && s.itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let c = s.itheta > 8192;
        ctx.remaining_bits -= s.qalloc + sbits;
        let mut sign = 0;
        if sbits != 0 {
            sign = ctx.dec.bits(1) as i32;
        }
        let sign = (1 - 2 * sign) as f32;
        let (x2, y2) = if c {
            (&mut *y, &mut *x)
        } else {
            (&mut *x, &mut *y)
        };
        cm = quant_band(
            ctx,
            x2,
            n,
            mbits,
            bb,
            lowband,
            lm,
            lowband_out,
            1.0,
            orig_fill,
        );
        y2[0] = -sign * x2[1];
        y2[1] = sign * x2[0];
        x[0] *= mid;
        x[1] *= mid;
        y[0] *= side;
        y[1] *= side;
        let tmp = x[0];
        x[0] = tmp - y[0];
        y[0] += tmp;
        let tmp = x[1];
        x[1] = tmp - y[1];
        y[1] += tmp;
    } else {
        let mut mbits = 0.max(b.min((b - s.delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= s.qalloc;
        let mut rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            cm = quant_band(ctx, x, n, mbits, bb, lowband, lm, lowband_out, 1.0, fill);
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && s.itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(ctx, y, n, sbits, bb, None, lm, None, side, fill >> bb);
        } else {
            cm = quant_band(ctx, y, n, sbits, bb, None, lm, None, side, fill >> bb);
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && s.itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(ctx, x, n, mbits, bb, lowband, lm, lowband_out, 1.0, fill);
        }
    }
    if n != 2 {
        stereo_merge(x, y, mid, n);
    }
    if s.inv {
        for v in &mut y[..n] {
            *v = -*v;
        }
    }
    cm
}

/// What `quant_all_bands` needs beyond the band context.
pub struct AllBands<'a> {
    pub start: usize,
    pub end: usize,
    pub pulses: &'a [i32; NB_EBANDS],
    pub short_blocks: bool,
    pub spread: i32,
    pub dual_stereo: bool,
    pub intensity: usize,
    pub tf_res: &'a [i32; NB_EBANDS],
    pub total_bits: i32,
    pub balance: i32,
    pub lm: i32,
    pub coded_bands: usize,
}

/// Decodes every band's shape into `x` (and `y` for stereo); returns the
/// updated noise seed.
pub fn quant_all_bands(
    p: &AllBands,
    x_all: &mut [f32],
    mut y_all: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    dec: &mut RangeDecoder,
    seed: u32,
) -> u32 {
    const NORM_LEN: usize = 8 * 78;
    let (start, end, lm) = (p.start, p.end, p.lm);
    let m = 1usize << lm;
    let bb: i32 = if p.short_blocks { m as i32 } else { 1 };
    let c_count = if y_all.is_some() { 2 } else { 1 };
    let norm_offset = m * EBANDS[start] as usize;
    let mut norm = [0f32; NORM_LEN];
    let mut norm2 = [0f32; NORM_LEN];
    let eb = |k: usize| m * EBANDS[k] as usize;
    let mut lowband_offset = 0usize;
    let mut update_lowband = true;
    let mut balance = p.balance;
    let mut dual_stereo = p.dual_stereo;
    let mut ctx = BandCtx {
        dec,
        i: 0,
        intensity: p.intensity,
        spread: p.spread,
        tf_change: 0,
        remaining_bits: 0,
        seed,
    };
    for i in start..end {
        ctx.i = i;
        let last = i == end - 1;
        let n = eb(i + 1) - eb(i);
        let tell = ctx.dec.tell_frac() as i32;
        if i != start {
            balance -= tell;
        }
        let remaining_bits = p.total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if i < p.coded_bands {
            let curr_balance = balance / 3.min(p.coded_bands as i32 - i as i32);
            0.max(16383.min((remaining_bits + 1).min(p.pulses[i] + curr_balance)))
        } else {
            0
        };
        if (eb(i) as isize - n as isize >= eb(start) as isize || i == start + 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }
        // special_hybrid_folding copies nothing when start is 0 (CELT only).
        ctx.tf_change = p.tf_res[i];
        let mut effective_lowband: Option<usize> = None;
        let (mut x_cm, mut y_cm): (u32, u32);
        if lowband_offset != 0 && (p.spread != SPREAD_AGGRESSIVE || bb > 1 || ctx.tf_change < 0) {
            let eff =
                (eb(lowband_offset) as isize - norm_offset as isize - n as isize).max(0) as usize;
            effective_lowband = Some(eff);
            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if eb(fold_start) <= eff + norm_offset {
                    break;
                }
            }
            let mut fold_end = lowband_offset - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i && eb(fold_end) < eff + norm_offset + n) {
                    break;
                }
            }
            x_cm = 0;
            y_cm = 0;
            let mut fold_i = fold_start;
            loop {
                x_cm |= collapse_masks[fold_i * c_count] as u32;
                y_cm |= collapse_masks[fold_i * c_count + c_count - 1] as u32;
                fold_i += 1;
                if fold_i >= fold_end {
                    break;
                }
            }
        } else {
            x_cm = (1u32 << bb) - 1;
            y_cm = x_cm;
        }
        if dual_stereo && i == p.intensity {
            dual_stereo = false;
            for j in 0..eb(i) - norm_offset {
                norm[j] = 0.5 * (norm[j] + norm2[j]);
            }
        }
        let out_at = eb(i) - norm_offset;
        let x = &mut x_all[eb(i)..eb(i + 1)];
        if dual_stereo {
            let y = &mut y_all.as_deref_mut().unwrap()[eb(i)..eb(i + 1)];
            for (shape, nm, cm) in [(x, &mut norm, &mut x_cm), (y, &mut norm2, &mut y_cm)] {
                let mut lb_buf = [0f32; MAX_BAND];
                let lb = effective_lowband.map(|e| {
                    lb_buf[..n].copy_from_slice(&nm[e..e + n]);
                    &lb_buf[..n]
                });
                let out = if last {
                    None
                } else {
                    Some(&mut nm[out_at..out_at + n])
                };
                *cm = quant_band(&mut ctx, shape, n, b / 2, bb, lb, lm, out, 1.0, *cm as i32);
            }
        } else {
            let mut lb_buf = [0f32; MAX_BAND];
            let lb = effective_lowband.map(|e| {
                lb_buf[..n].copy_from_slice(&norm[e..e + n]);
                &lb_buf[..n]
            });
            let out = if last {
                None
            } else {
                Some(&mut norm[out_at..out_at + n])
            };
            let fill = (x_cm | y_cm) as i32;
            x_cm = match y_all.as_deref_mut() {
                Some(y_all) => {
                    let y = &mut y_all[eb(i)..eb(i + 1)];
                    quant_band_stereo(&mut ctx, x, y, n, b, bb, lb, lm, out, fill)
                }
                None => quant_band(&mut ctx, x, n, b, bb, lb, lm, out, 1.0, fill),
            };
            y_cm = x_cm;
        }
        collapse_masks[i * c_count] = x_cm as u8;
        collapse_masks[i * c_count + c_count - 1] = y_cm as u8;
        balance += p.pulses[i] + tell;
        update_lowband = b > (n as i32) << BITRES;
    }
    ctx.seed
}

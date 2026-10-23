// Ported from libopus 1.6.1, celt/vq.c (float build). Copyright 2007-2009
// Xiph.Org Foundation, 2007-2009 Timothy B. Terriberry, Jean-Marc Valin.
// BSD-3-Clause, see LICENSE in this crate.

//! Pulse vectors to unit-norm band shapes (RFC 6716 §4.3.4.3): spreading
//! rotation and renormalisation.

use crate::cwrs::decode_pulses;
use crate::range::RangeDecoder;

const SPREAD_NONE: i32 = 0;

fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    let ms = -s;
    for i in 0..len.saturating_sub(stride) {
        let (x1, x2) = (x[i], x[i + stride]);
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 + ms * x2;
    }
    let mut i = len as isize - 2 * stride as isize - 1;
    while i >= 0 {
        let k = i as usize;
        let (x1, x2) = (x[k], x[k + stride]);
        x[k + stride] = c * x2 + s * x1;
        x[k] = c * x1 + ms * x2;
        i -= 1;
    }
}

pub fn exp_rotation(x: &mut [f32], len: usize, dir: i32, stride: usize, k: i32, spread: i32) {
    const SPREAD_FACTOR: [i32; 3] = [15, 10, 5];
    if 2 * k >= len as i32 || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[spread as usize - 1];
    let gain = len as f32 / (len as i32 + factor * k) as f32;
    let theta = 0.5 * (gain * gain);
    let c = (0.5 * std::f32::consts::PI * theta).cos();
    let s = (0.5 * std::f32::consts::PI * (1.0 - theta)).cos();
    let mut stride2 = 0;
    if len >= 8 * stride {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let x = &mut x[i * len..(i + 1) * len];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(x, len, stride2, s, c);
            }
            exp_rotation1(x, len, 1, c, s);
        } else {
            exp_rotation1(x, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(x, len, stride2, s, -c);
            }
        }
    }
}

fn extract_collapse_mask(iy: &[i32], n: usize, b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut mask = 0;
    for i in 0..b {
        let any = iy[i * n0..(i + 1) * n0].iter().any(|&v| v != 0);
        mask |= (any as u32) << i;
    }
    mask
}

/// Decodes `k` pulses over `x[..n]` with `gain`; returns the collapse mask.
pub fn alg_unquant(
    x: &mut [f32],
    n: usize,
    k: i32,
    spread: i32,
    b: usize,
    dec: &mut RangeDecoder,
    gain: f32,
) -> u32 {
    let mut iy = [0i32; 176];
    let ryy = decode_pulses(&mut iy[..n], n, k as usize, dec);
    let g = (1.0 / ryy.sqrt()) * gain;
    for i in 0..n {
        x[i] = iy[i] as f32 * g;
    }
    exp_rotation(x, n, -1, b, k, spread);
    extract_collapse_mask(&iy[..n], n, b)
}

pub fn renormalise_vector(x: &mut [f32], n: usize, gain: f32) {
    let e = 1e-15f32 + inner_prod(&x[..n], &x[..n]);
    let g = (1.0 / e.sqrt()) * gain;
    for v in &mut x[..n] {
        *v *= g;
    }
}

pub fn inner_prod(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

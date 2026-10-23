// Ported from libopus 1.6.1, celt/pitch.[ch] and celt/celt_lpc.c (float
// build, plain C kernels). Copyright 2007-2009 Xiph.Org Foundation,
// 2007-2008 CSIRO, 2009-2010 Jean-Marc Valin. BSD-3-Clause, see LICENSE.

//! What the packet-loss concealment needs: the pitch of the last decoded
//! audio, and an LPC model of it to extrapolate the excitation.

pub const LPC_ORDER: usize = 24;

fn xcorr_kernel(x: &[f32], y: &[f32], sum: &mut [f32; 4], len: usize) {
    let (mut y0, mut y1, mut y2) = (y[0], y[1], y[2]);
    let mut y3 = 0.0;
    let (mut xi, mut yi) = (0, 3);
    let mut j = 0;
    while j + 3 < len {
        let t = x[xi];
        y3 = y[yi];
        sum[0] += t * y0;
        sum[1] += t * y1;
        sum[2] += t * y2;
        sum[3] += t * y3;
        let t = x[xi + 1];
        y0 = y[yi + 1];
        sum[0] += t * y1;
        sum[1] += t * y2;
        sum[2] += t * y3;
        sum[3] += t * y0;
        let t = x[xi + 2];
        y1 = y[yi + 2];
        sum[0] += t * y2;
        sum[1] += t * y3;
        sum[2] += t * y0;
        sum[3] += t * y1;
        let t = x[xi + 3];
        y2 = y[yi + 3];
        sum[0] += t * y3;
        sum[1] += t * y0;
        sum[2] += t * y1;
        sum[3] += t * y2;
        xi += 4;
        yi += 4;
        j += 4;
    }
    if j < len {
        let t = x[xi];
        y3 = y[yi];
        sum[0] += t * y0;
        sum[1] += t * y1;
        sum[2] += t * y2;
        sum[3] += t * y3;
        xi += 1;
        yi += 1;
    }
    if j + 1 < len {
        let t = x[xi];
        y0 = y[yi];
        sum[0] += t * y1;
        sum[1] += t * y2;
        sum[2] += t * y3;
        sum[3] += t * y0;
        xi += 1;
        yi += 1;
    }
    if j + 2 < len {
        let t = x[xi];
        y1 = y[yi];
        sum[0] += t * y2;
        sum[1] += t * y3;
        sum[2] += t * y0;
        sum[3] += t * y1;
    }
}

fn inner_prod(x: &[f32], y: &[f32], n: usize) -> f32 {
    let mut s = 0f32;
    for i in 0..n {
        s += x[i] * y[i];
    }
    s
}

fn pitch_xcorr(x: &[f32], y: &[f32], xcorr: &mut [f32], len: usize, max_pitch: usize) {
    let mut i = 0;
    while i + 3 < max_pitch {
        let mut sum = [0f32; 4];
        xcorr_kernel(x, &y[i..], &mut sum, len);
        xcorr[i..i + 4].copy_from_slice(&sum);
        i += 4;
    }
    while i < max_pitch {
        xcorr[i] = inner_prod(x, &y[i..], len);
        i += 1;
    }
}

/// `_celt_autocorr`: `ac[0..=lag]` of `x[..n]`, its ends windowed over
/// `window` (empty for none).
pub fn autocorr(x: &[f32], ac: &mut [f32], window: &[f32], lag: usize, n: usize) {
    let overlap = window.len();
    let mut xx = vec![0f32; n];
    xx.copy_from_slice(&x[..n]);
    for i in 0..overlap {
        xx[i] = x[i] * window[i];
        xx[n - i - 1] = x[n - i - 1] * window[i];
    }
    let fast_n = n - lag;
    pitch_xcorr(&xx, &xx, ac, fast_n, lag + 1);
    for k in 0..=lag {
        let mut d = 0f32;
        for i in k + fast_n..n {
            d += xx[i] * xx[i - k];
        }
        ac[k] += d;
    }
}

/// `_celt_lpc`: Levinson-Durbin, `p` coefficients from `ac[0..=p]`.
pub fn lpc(out: &mut [f32], ac: &[f32], p: usize) {
    let mut error = ac[0];
    out[..p].fill(0.0);
    if ac[0] <= 1e-10 {
        return;
    }
    for i in 0..p {
        let mut rr = 0f32;
        for j in 0..i {
            rr += out[j] * ac[i - j];
        }
        rr += ac[i + 1];
        let r = -rr / error;
        out[i] = r;
        for j in 0..(i + 1) >> 1 {
            let (t1, t2) = (out[j], out[i - 1 - j]);
            out[j] = t1 + r * t2;
            out[i - 1 - j] = t2 + r * t1;
        }
        error -= r * r * error;
        if error <= 0.001 * ac[0] {
            break;
        }
    }
}

/// `celt_fir`: `y[i] = x[i] + sum num[j] * x[i - 1 - j]`, `x` given with
/// `ord` samples of history before `at`.
pub fn fir(x: &[f32], at: usize, num: &[f32], y: &mut [f32], n: usize, ord: usize) {
    let mut rnum = [0f32; LPC_ORDER];
    for i in 0..ord {
        rnum[i] = num[ord - i - 1];
    }
    let mut i = 0;
    while i + 3 < n {
        let mut sum = [x[at + i], x[at + i + 1], x[at + i + 2], x[at + i + 3]];
        xcorr_kernel(&rnum, &x[at + i - ord..], &mut sum, ord);
        y[i..i + 4].copy_from_slice(&sum);
        i += 4;
    }
    while i < n {
        let mut sum = x[at + i];
        for j in 0..ord {
            sum += rnum[j] * x[at + i + j - ord];
        }
        y[i] = sum;
        i += 1;
    }
}

/// `celt_iir` in place over `x[..n]` (`mem`: the last outputs before it,
/// newest first).
pub fn iir(x: &mut [f32], den: &[f32], n: usize, ord: usize, mem: &mut [f32]) {
    let mut rden = [0f32; LPC_ORDER];
    for i in 0..ord {
        rden[i] = den[ord - i - 1];
    }
    let mut y = vec![0f32; n + ord];
    for i in 0..ord {
        y[i] = -mem[ord - i - 1];
    }
    let mut i = 0;
    while i + 3 < n {
        let mut sum = [x[i], x[i + 1], x[i + 2], x[i + 3]];
        xcorr_kernel(&rden, &y[i..], &mut sum, ord);
        y[i + ord] = -sum[0];
        x[i] = sum[0];
        sum[1] += y[i + ord] * den[0];
        y[i + ord + 1] = -sum[1];
        x[i + 1] = sum[1];
        sum[2] += y[i + ord + 1] * den[0];
        sum[2] += y[i + ord] * den[1];
        y[i + ord + 2] = -sum[2];
        x[i + 2] = sum[2];
        sum[3] += y[i + ord + 2] * den[0];
        sum[3] += y[i + ord + 1] * den[1];
        sum[3] += y[i + ord] * den[2];
        y[i + ord + 3] = -sum[3];
        x[i + 3] = sum[3];
        i += 4;
    }
    while i < n {
        let mut sum = x[i];
        for j in 0..ord {
            sum -= rden[j] * y[i + j];
        }
        y[i + ord] = sum;
        x[i] = sum;
        i += 1;
    }
    for i in 0..ord {
        mem[i] = x[n - i - 1];
    }
}

fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize, max_pitch: usize) -> [usize; 2] {
    let mut syy = 1f32;
    let mut best_num = [-1f32; 2];
    let mut best_den = [0f32; 2];
    let mut best = [0usize, 1];
    for v in &y[..len] {
        syy += v * v;
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let x16 = xcorr[i] * 1e-12;
            let num = x16 * x16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best[1] = best[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best[0] = i;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best[1] = i;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
    best
}

fn fir5(x: &mut [f32], num: &[f32; 5]) {
    let mut mem = [0f32; 5];
    for v in x.iter_mut() {
        let mut sum = *v;
        sum += num[0] * mem[0];
        sum += num[1] * mem[1];
        sum += num[2] * mem[2];
        sum += num[3] * mem[3];
        sum += num[4] * mem[4];
        mem = [*v, mem[0], mem[1], mem[2], mem[3]];
        *v = sum;
    }
}

/// `pitch_downsample` by 2 of both channels' history into `x_lp`.
pub fn pitch_downsample(x: [&[f32]; 2], x_lp: &mut [f32], len: usize) {
    for i in 1..len {
        x_lp[i] = 0.25 * x[0][2 * i - 1] + 0.25 * x[0][2 * i + 1] + 0.5 * x[0][2 * i];
    }
    x_lp[0] = 0.25 * x[0][1] + 0.5 * x[0][0];
    for i in 1..len {
        x_lp[i] += 0.25 * x[1][2 * i - 1] + 0.25 * x[1][2 * i + 1] + 0.5 * x[1][2 * i];
    }
    x_lp[0] += 0.25 * x[1][1] + 0.5 * x[1][0];
    let mut ac = [0f32; 5];
    autocorr(x_lp, &mut ac, &[], 4, len);
    ac[0] *= 1.0001;
    for (i, a) in ac.iter_mut().enumerate().skip(1) {
        *a -= *a * (0.008 * i as f32) * (0.008 * i as f32);
    }
    let mut l = [0f32; 4];
    lpc(&mut l, &ac, 4);
    let mut tmp = 1f32;
    for v in &mut l {
        tmp *= 0.9;
        *v *= tmp;
    }
    let c1 = 0.8f32;
    let lpc2 = [
        l[0] + 0.8,
        l[1] + c1 * l[0],
        l[2] + c1 * l[1],
        l[3] + c1 * l[2],
        c1 * l[3],
    ];
    fir5(&mut x_lp[..len], &lpc2);
}

/// `pitch_search`: the lag (in `y`'s samples) that best repeats `x_lp`.
pub fn pitch_search(x_lp: &[f32], y: &[f32], len: usize, max_pitch: usize) -> usize {
    let lag = len + max_pitch;
    let x_lp4: Vec<f32> = (0..len >> 2).map(|j| x_lp[2 * j]).collect();
    let y_lp4: Vec<f32> = (0..lag >> 2).map(|j| y[2 * j]).collect();
    let mut xcorr = vec![0f32; max_pitch >> 1];
    pitch_xcorr(&x_lp4, &y_lp4, &mut xcorr, len >> 2, max_pitch >> 2);
    let best = find_best_pitch(&xcorr, &y_lp4, len >> 2, max_pitch >> 2);
    for i in 0..max_pitch >> 1 {
        xcorr[i] = 0.0;
        if (i as isize - 2 * best[0] as isize).abs() > 2
            && (i as isize - 2 * best[1] as isize).abs() > 2
        {
            continue;
        }
        xcorr[i] = inner_prod(x_lp, &y[i..], len >> 1).max(-1.0);
    }
    let best = find_best_pitch(&xcorr, y, len >> 1, max_pitch >> 1);
    let offset: isize = if best[0] > 0 && best[0] < (max_pitch >> 1) - 1 {
        let (a, b, c) = (xcorr[best[0] - 1], xcorr[best[0]], xcorr[best[0] + 1]);
        if c - a > 0.7 * (b - a) {
            1
        } else if a - c > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    (2 * best[0] as isize - offset) as usize
}

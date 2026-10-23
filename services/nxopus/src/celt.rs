// Ported from libopus 1.6.1, celt/celt_decoder.c, celt/celt.c and
// celt/quant_bands.c (float build). Copyright 2007-2010 Xiph.Org
// Foundation, 2007-2008 CSIRO, 2008 Gregory Maxwell, Jean-Marc Valin.
// BSD-3-Clause, see LICENSE in this crate.

//! One CELT frame (RFC 6716 §4.3): header flags, energies, allocation,
//! band shapes, then the inverse MDCT, the pitch post-filter and the
//! de-emphasis.

use crate::bands::lcg_rand;
use crate::bands::{anti_collapse, denormalise_bands, quant_all_bands, AllBands};
use crate::mdct::imdct;
use crate::mode::{MAX_LM, NB_EBANDS, OVERLAP, PREEMPH, SHORT_MDCT_SIZE};
use crate::plc::{self, LPC_ORDER};
use crate::range::{RangeDecoder, BITRES};
use crate::rate::{compute_allocation, init_caps, MAX_FINE_BITS};
use crate::tables::{EBANDS, E_PROB_MODEL, WINDOW};
use crate::vq::renormalise_vector;

/// Samples of history per channel (`DECODE_BUFFER_SIZE`).
pub const BUFFER: usize = 2048;
const COMBFILTER_MINPERIOD: i32 = 15;
const MAX_PERIOD: usize = 1024;
const PLC_PITCH_LAG_MAX: usize = 720;
const PLC_PITCH_LAG_MIN: usize = 100;
const SPREAD_NORMAL: i32 = 2;

const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TAPSET_ICDF: [u8; 3] = [2, 1, 0];
const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];
const TF_SELECT_TABLE: [[i8; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1],
    [0, -1, 0, -2, 1, 0, 1, -1],
    [0, -2, 0, -3, 2, 0, 1, -1],
    [0, -2, 0, -3, 3, 0, 1, -1],
];
const PRED_COEF: [f32; 4] = [
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];
const BETA_COEF: [f32; 4] = [
    30147.0 / 32768.0,
    22282.0 / 32768.0,
    12124.0 / 32768.0,
    6554.0 / 32768.0,
];
const BETA_INTRA: f32 = 4915.0 / 32768.0;

/// The CELT decoder's state for a stereo output.
pub struct Celt {
    mem: [Vec<f32>; 2],
    old_band_e: [f32; 2 * NB_EBANDS],
    old_log_e: [f32; 2 * NB_EBANDS],
    old_log_e2: [f32; 2 * NB_EBANDS],
    background_log_e: [f32; 2 * NB_EBANDS],
    preemph_mem: [f32; 2],
    postfilter_period: i32,
    postfilter_period_old: i32,
    postfilter_gain: f32,
    postfilter_gain_old: f32,
    postfilter_tapset: usize,
    postfilter_tapset_old: usize,
    pub rng: u32,
    pub loss_duration: i32,
    plc_duration: i32,
    last_frame: Frame,
    skip_plc: bool,
    last_pitch_index: usize,
    /// The next good frame must fold the concealed overlap first.
    prefilter_and_fold: bool,
    lpc: [[f32; LPC_ORDER]; 2],
    /// The coded bands of the last packet (its bandwidth).
    end: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Frame {
    None,
    Normal,
    PlcNoise,
    PlcPeriodic,
}

impl Default for Celt {
    fn default() -> Self {
        Self {
            mem: [vec![0.0; BUFFER + OVERLAP], vec![0.0; BUFFER + OVERLAP]],
            old_band_e: [0.0; 2 * NB_EBANDS],
            old_log_e: [-28.0; 2 * NB_EBANDS],
            old_log_e2: [-28.0; 2 * NB_EBANDS],
            background_log_e: [0.0; 2 * NB_EBANDS],
            preemph_mem: [0.0; 2],
            postfilter_period: 0,
            postfilter_period_old: 0,
            postfilter_gain: 0.0,
            postfilter_gain_old: 0.0,
            postfilter_tapset: 0,
            postfilter_tapset_old: 0,
            rng: 0,
            loss_duration: 0,
            plc_duration: 0,
            last_frame: Frame::None,
            skip_plc: true,
            last_pitch_index: 0,
            prefilter_and_fold: false,
            lpc: [[0.0; LPC_ORDER]; 2],
            end: NB_EBANDS,
        }
    }
}

fn unquant_coarse_energy(
    old: &mut [f32; 2 * NB_EBANDS],
    start: usize,
    end: usize,
    intra: bool,
    dec: &mut RangeDecoder,
    c_count: usize,
    lm: usize,
) {
    let prob = &E_PROB_MODEL[(lm * 2 + intra as usize) * 42..][..42];
    let (coef, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm], BETA_COEF[lm])
    };
    let mut prev = [0f32; 2];
    let budget = dec.storage() as i32 * 8;
    for i in start..end {
        for c in 0..c_count {
            let tell = dec.tell();
            let qi = if budget - tell >= 15 {
                let pi = 2 * i.min(20);
                dec.laplace((prob[pi] as u32) << 7, (prob[pi + 1] as i32) << 6)
            } else if budget - tell >= 2 {
                let qi = dec.icdf(&SMALL_ENERGY_ICDF, 2) as i32;
                (qi >> 1) ^ -(qi & 1)
            } else if budget - tell >= 1 {
                -(dec.bit_logp(1) as i32)
            } else {
                -1
            };
            let q = qi as f32;
            let e = &mut old[i + c * NB_EBANDS];
            *e = e.max(-9.0);
            let tmp = coef * *e + prev[c] + q;
            *e = tmp;
            prev[c] = prev[c] + q - beta * q;
        }
    }
}

fn unquant_fine_energy(
    old: &mut [f32; 2 * NB_EBANDS],
    start: usize,
    end: usize,
    fine_quant: &[i32; NB_EBANDS],
    dec: &mut RangeDecoder,
    c_count: usize,
) {
    for i in start..end {
        let extra = fine_quant[i];
        if extra <= 0 {
            continue;
        }
        if dec.tell() + c_count as i32 * extra > dec.storage() as i32 * 8 {
            continue;
        }
        for c in 0..c_count {
            let q2 = dec.bits(extra as u32);
            let offset = (q2 as f32 + 0.5) * (1 << (14 - extra)) as f32 * (1.0 / 16384.0) - 0.5;
            old[i + c * NB_EBANDS] += offset;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn unquant_energy_finalise(
    old: &mut [f32; 2 * NB_EBANDS],
    start: usize,
    end: usize,
    fine_quant: &[i32; NB_EBANDS],
    fine_priority: &[i32; NB_EBANDS],
    mut bits_left: i32,
    dec: &mut RangeDecoder,
    c_count: usize,
) {
    for prio in 0..2 {
        let mut i = start;
        while i < end && bits_left >= c_count as i32 {
            if fine_quant[i] < MAX_FINE_BITS && fine_priority[i] == prio {
                for c in 0..c_count {
                    let q2 = dec.bits(1);
                    let offset = (q2 as f32 - 0.5)
                        * (1 << (14 - fine_quant[i] - 1)) as f32
                        * (1.0 / 16384.0);
                    old[i + c * NB_EBANDS] += offset;
                    bits_left -= 1;
                }
            }
            i += 1;
        }
    }
}

fn tf_decode(
    start: usize,
    end: usize,
    transient: bool,
    tf_res: &mut [i32; NB_EBANDS],
    lm: usize,
    dec: &mut RangeDecoder,
) {
    let mut budget = dec.storage() * 8;
    let mut tell = dec.tell() as u32;
    let mut logp = if transient { 2 } else { 4 };
    let tf_select_rsv = lm > 0 && tell + logp < budget;
    budget -= tf_select_rsv as u32;
    let (mut tf_changed, mut curr) = (0, 0);
    for r in tf_res.iter_mut().take(end).skip(start) {
        if tell + logp <= budget {
            curr ^= dec.bit_logp(logp) as i32;
            tell = dec.tell() as u32;
            tf_changed |= curr;
        }
        *r = curr;
        logp = if transient { 4 } else { 5 };
    }
    let t = 4 * transient as usize;
    let mut tf_select = 0;
    if tf_select_rsv
        && TF_SELECT_TABLE[lm][t + tf_changed as usize]
            != TF_SELECT_TABLE[lm][t + 2 + tf_changed as usize]
    {
        tf_select = dec.bit_logp(1) as usize;
    }
    for r in tf_res.iter_mut().take(end).skip(start) {
        *r = TF_SELECT_TABLE[lm][t + 2 * tf_select + *r as usize] as i32;
    }
}

/// The pitch post-filter over `buf[at..at + n]`, in place (it reads its
/// own output `t` samples back).
#[allow(clippy::too_many_arguments)]
fn comb_filter(
    buf: &mut [f32],
    at: usize,
    t0: i32,
    t1: i32,
    n: usize,
    g0: f32,
    g1: f32,
    tapset0: usize,
    tapset1: usize,
    overlap: usize,
) {
    const GAINS: [[f32; 3]; 3] = [
        [0.306_640_63, 0.217_041_02, 0.129_638_67],
        [0.463_867_2, 0.268_066_4, 0.0],
        [0.799_804_7, 0.100_097_66, 0.0],
    ];
    if g0 == 0.0 && g1 == 0.0 {
        return;
    }
    let t0 = t0.max(COMBFILTER_MINPERIOD) as usize;
    let t1 = t1.max(COMBFILTER_MINPERIOD) as usize;
    let (g00, g01, g02) = (
        g0 * GAINS[tapset0][0],
        g0 * GAINS[tapset0][1],
        g0 * GAINS[tapset0][2],
    );
    let (g10, g11, g12) = (
        g1 * GAINS[tapset1][0],
        g1 * GAINS[tapset1][1],
        g1 * GAINS[tapset1][2],
    );
    let x = |buf: &[f32], k: isize| buf[(at as isize + k) as usize];
    let mut x1 = x(buf, -(t1 as isize) + 1);
    let mut x2 = x(buf, -(t1 as isize));
    let mut x3 = x(buf, -(t1 as isize) - 1);
    let mut x4 = x(buf, -(t1 as isize) - 2);
    let overlap = if g0 == g1 && t0 == t1 && tapset0 == tapset1 {
        0
    } else {
        overlap
    };
    let (t0i, t1i) = (t0 as isize, t1 as isize);
    for i in 0..overlap {
        let ii = i as isize;
        let x0 = x(buf, ii - t1i + 2);
        let f = WINDOW[i] * WINDOW[i];
        let y = x(buf, ii)
            + ((1.0 - f) * g00) * x(buf, ii - t0i)
            + ((1.0 - f) * g01) * (x(buf, ii - t0i + 1) + x(buf, ii - t0i - 1))
            + ((1.0 - f) * g02) * (x(buf, ii - t0i + 2) + x(buf, ii - t0i - 2))
            + (f * g10) * x2
            + (f * g11) * (x1 + x3)
            + (f * g12) * (x0 + x4);
        buf[at + i] = y;
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
    if g1 == 0.0 {
        return;
    }
    for i in overlap..n {
        let ii = i as isize;
        let x0 = x(buf, ii - t1i + 2);
        buf[at + i] = x(buf, ii) + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

/// `comb_filter` with no overlap into a separate output: `y[i] = x[i] +`
/// the taps on `x` around `x[i - t]`, with `x` at `buf[at..]`.
fn comb_filter_into(
    y: &mut [f32],
    buf: &[f32],
    at: usize,
    t: i32,
    n: usize,
    g: f32,
    tapset: usize,
) {
    const GAINS: [[f32; 3]; 3] = [
        [0.306_640_63, 0.217_041_02, 0.129_638_67],
        [0.463_867_2, 0.268_066_4, 0.0],
        [0.799_804_7, 0.100_097_66, 0.0],
    ];
    if g == 0.0 {
        y[..n].copy_from_slice(&buf[at..at + n]);
        return;
    }
    let t = t.max(COMBFILTER_MINPERIOD) as usize;
    let (g10, g11, g12) = (
        g * GAINS[tapset][0],
        g * GAINS[tapset][1],
        g * GAINS[tapset][2],
    );
    let (mut x4, mut x3, mut x2, mut x1) = (
        buf[at - t - 2],
        buf[at - t - 1],
        buf[at - t],
        buf[at - t + 1],
    );
    for i in 0..n {
        let x0 = buf[at + i + 2 - t];
        y[i] = buf[at + i] + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

impl Celt {
    /// Decodes one frame of `n` = 120 << lm samples per channel (`c_count`
    /// coded channels, 1 or 2) into `pcm` (stereo interleaved, ±1.0).
    pub fn decode(
        &mut self,
        data: &[u8],
        lm: usize,
        c_count: usize,
        end: usize,
        pcm: &mut [f32],
    ) -> Result<(), ()> {
        let start = 0usize;
        let m = 1usize << lm;
        let n = m * SHORT_MDCT_SIZE;
        let len = data.len();
        if len > 1275 || lm > MAX_LM as usize {
            return Err(());
        }
        let eff_end = end.min(NB_EBANDS);
        self.end = end;
        let mut dec = RangeDecoder::new(data);
        if self.loss_duration == 0 {
            self.skip_plc = false;
        }
        if c_count == 1 {
            for i in 0..NB_EBANDS {
                self.old_band_e[i] = self.old_band_e[i].max(self.old_band_e[NB_EBANDS + i]);
            }
        }
        let mut total_bits = len as i32 * 8;
        let mut tell = dec.tell();
        let silence = if tell >= total_bits {
            true
        } else if tell == 1 {
            dec.bit_logp(15)
        } else {
            false
        };
        if silence {
            tell = len as i32 * 8;
            dec.skip_to(tell);
        }
        let (mut pf_gain, mut pf_pitch, mut pf_tapset) = (0f32, 0i32, 0usize);
        if start == 0 && tell + 16 <= total_bits {
            if dec.bit_logp(1) {
                let octave = dec.uint(6);
                pf_pitch = (16 << octave) + dec.bits(4 + octave) as i32 - 1;
                let qg = dec.bits(3);
                if dec.tell() + 2 <= total_bits {
                    pf_tapset = dec.icdf(&TAPSET_ICDF, 2);
                }
                pf_gain = 0.093_75 * (qg + 1) as f32;
            }
            tell = dec.tell();
        }
        let transient = if lm > 0 && tell + 3 <= total_bits {
            let t = dec.bit_logp(3);
            tell = dec.tell();
            t
        } else {
            false
        };
        let intra = if tell + 3 <= total_bits {
            dec.bit_logp(3)
        } else {
            false
        };
        if !intra && self.loss_duration != 0 {
            self.safe_energies(start, end, lm);
        }
        unquant_coarse_energy(
            &mut self.old_band_e,
            start,
            end,
            intra,
            &mut dec,
            c_count,
            lm,
        );
        let mut tf_res = [0i32; NB_EBANDS];
        tf_decode(start, end, transient, &mut tf_res, lm, &mut dec);
        tell = dec.tell();
        let spread = if tell + 4 <= total_bits {
            dec.icdf(&SPREAD_ICDF, 5) as i32
        } else {
            SPREAD_NORMAL
        };
        let mut cap = [0i32; NB_EBANDS];
        init_caps(&mut cap, lm as i32, c_count as i32);
        let mut offsets = [0i32; NB_EBANDS];
        let mut dynalloc_logp = 6;
        total_bits <<= BITRES;
        let mut tellf = dec.tell_frac() as i32;
        for i in start..end {
            let width = (c_count as i32 * (EBANDS[i + 1] - EBANDS[i]) as i32) << lm;
            let quanta = (width << BITRES).min((6 << BITRES).max(width));
            let mut loop_logp = dynalloc_logp;
            let mut boost = 0;
            while tellf + (loop_logp << BITRES) < total_bits && boost < cap[i] {
                let flag = dec.bit_logp(loop_logp as u32);
                tellf = dec.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_bits -= quanta;
                loop_logp = 1;
            }
            offsets[i] = boost;
            if boost > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
        }
        let alloc_trim = if tellf + (6 << BITRES) <= total_bits {
            dec.icdf(&TRIM_ICDF, 7) as i32
        } else {
            5
        };
        let mut bits = ((len as i32 * 8) << BITRES) - dec.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if transient && lm >= 2 && bits >= ((lm as i32 + 2) << BITRES) {
            1 << BITRES
        } else {
            0
        };
        bits -= anti_collapse_rsv;
        let a = compute_allocation(
            start,
            end,
            &offsets,
            &cap,
            alloc_trim,
            bits,
            c_count as i32,
            lm as i32,
            &mut dec,
        );
        unquant_fine_energy(
            &mut self.old_band_e,
            start,
            end,
            &a.fine_quant,
            &mut dec,
            c_count,
        );
        for ch in &mut self.mem {
            ch.copy_within(n.., 0);
        }
        let mut x = [0f32; 960];
        let mut y = [0f32; 960];
        let mut collapse_masks = [0u8; 2 * NB_EBANDS];
        let p = AllBands {
            start,
            end,
            pulses: &a.pulses,
            short_blocks: transient,
            spread,
            dual_stereo: a.dual_stereo,
            intensity: a.intensity,
            tf_res: &tf_res,
            total_bits: len as i32 * (8 << BITRES) - anti_collapse_rsv,
            balance: a.balance,
            lm: lm as i32,
            coded_bands: a.coded_bands,
        };
        let y_opt = if c_count == 2 {
            Some(&mut y[..n])
        } else {
            None
        };
        self.rng = quant_all_bands(
            &p,
            &mut x[..n],
            y_opt,
            &mut collapse_masks,
            &mut dec,
            self.rng,
        );
        let anti_collapse_on = anti_collapse_rsv > 0 && dec.bits(1) != 0;
        let left = len as i32 * 8 - dec.tell();
        unquant_energy_finalise(
            &mut self.old_band_e,
            start,
            end,
            &a.fine_quant,
            &a.fine_priority,
            left,
            &mut dec,
            c_count,
        );
        if anti_collapse_on {
            let mut chans: [&mut [f32]; 2] = [&mut x[..n], &mut y[..n]];
            anti_collapse(
                &mut chans[..c_count],
                &collapse_masks,
                lm as i32,
                c_count,
                start,
                end,
                &self.old_band_e,
                &self.old_log_e,
                &self.old_log_e2,
                &a.pulses,
                self.rng,
            );
        }
        if silence {
            self.old_band_e = [-28.0; 2 * NB_EBANDS];
        }
        if self.prefilter_and_fold {
            self.prefilter_and_fold(n);
        }
        self.synthesis(&x, &y, start, eff_end, c_count, transient, lm, silence);
        self.post_filter(lm, n, pf_pitch, pf_gain, pf_tapset);
        if c_count == 1 {
            self.old_band_e.copy_within(..NB_EBANDS, NB_EBANDS);
        }
        if !transient {
            self.old_log_e2 = self.old_log_e;
            self.old_log_e = self.old_band_e;
        } else {
            for i in 0..2 * NB_EBANDS {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_band_e[i]);
            }
        }
        let max_background_increase = (160.min(self.loss_duration + m as i32)) as f32 * 0.001;
        for i in 0..2 * NB_EBANDS {
            self.background_log_e[i] =
                (self.background_log_e[i] + max_background_increase).min(self.old_band_e[i]);
        }
        for c in 0..2 {
            for i in (0..start).chain(end..NB_EBANDS) {
                self.old_band_e[c * NB_EBANDS + i] = 0.0;
                self.old_log_e[c * NB_EBANDS + i] = -28.0;
                self.old_log_e2[c * NB_EBANDS + i] = -28.0;
            }
        }
        self.rng = dec.rng();
        self.deemphasis(n, pcm);
        self.loss_duration = 0;
        self.plc_duration = 0;
        self.last_frame = Frame::Normal;
        self.prefilter_and_fold = false;
        if dec.tell() > 8 * len as i32 || dec.error {
            return Err(());
        }
        Ok(())
    }

    /// A frame that never came (`celt_decode_lost`): the last pitch period
    /// repeated through an LPC model while losses are short, then noise
    /// shaped by the decaying band energies.
    pub fn conceal(&mut self, lm: usize, pcm: &mut [f32]) {
        let n = SHORT_MDCT_SIZE << lm;
        let periodic = self.plc_duration < 40 && !self.skip_plc;
        if periodic {
            self.conceal_periodic(n);
            self.last_frame = Frame::PlcPeriodic;
        } else {
            self.conceal_noise(lm, n);
            self.last_frame = Frame::PlcNoise;
        }
        self.loss_duration = (self.loss_duration + (1 << lm)).min(10000);
        self.plc_duration = (self.plc_duration + (1 << lm)).min(10000);
        self.deemphasis(n, pcm);
    }

    fn conceal_noise(&mut self, lm: usize, n: usize) {
        let (start, end) = (0usize, self.end);
        for ch in &mut self.mem {
            ch.copy_within(n.., 0);
        }
        if self.prefilter_and_fold {
            self.prefilter_and_fold(n);
        }
        let decay = if self.loss_duration == 0 { 1.5 } else { 0.5 };
        for c in 0..2 {
            for i in start..end {
                let k = c * NB_EBANDS + i;
                self.old_band_e[k] = self.background_log_e[k].max(self.old_band_e[k] - decay);
            }
        }
        let mut seed = self.rng;
        let mut x = [0f32; 960];
        let mut y = [0f32; 960];
        for shape in [&mut x, &mut y] {
            for i in start..end {
                let boffs = (EBANDS[i] as usize) << lm;
                let blen = ((EBANDS[i + 1] - EBANDS[i]) as usize) << lm;
                for v in &mut shape[boffs..boffs + blen] {
                    seed = lcg_rand(seed);
                    *v = (seed as i32 >> 20) as f32;
                }
                renormalise_vector(&mut shape[boffs..], blen, 1.0);
            }
        }
        self.rng = seed;
        self.synthesis(&x, &y, start, end, 2, false, lm, false);
        let (pitch, gain, tapset) = (
            self.postfilter_period,
            self.postfilter_gain,
            self.postfilter_tapset,
        );
        self.post_filter(lm, n, pitch, gain, tapset);
        self.prefilter_and_fold = false;
        self.skip_plc = true;
    }

    fn conceal_periodic(&mut self, n: usize) {
        let mut fade = 1f32;
        let new_pitch = self.last_frame != Frame::PlcPeriodic;
        let pitch_index = if new_pitch {
            let mut lp = vec![0f32; BUFFER >> 1];
            plc::pitch_downsample([&self.mem[0], &self.mem[1]], &mut lp, BUFFER >> 1);
            let p = plc::pitch_search(
                &lp[PLC_PITCH_LAG_MAX >> 1..],
                &lp,
                BUFFER - PLC_PITCH_LAG_MAX,
                PLC_PITCH_LAG_MAX - PLC_PITCH_LAG_MIN,
            );
            self.last_pitch_index = PLC_PITCH_LAG_MAX - p;
            self.last_pitch_index
        } else {
            fade = 0.8;
            self.last_pitch_index
        };
        let exc_length = (2 * pitch_index).min(MAX_PERIOD);
        let mut exc_buf = vec![0f32; MAX_PERIOD + LPC_ORDER];
        let mut fir_tmp = vec![0f32; exc_length];
        let extrapolation_len = n + OVERLAP;
        for c in 0..2 {
            let buf = &mut self.mem[c];
            // exc_buf[LPC_ORDER + i] is libopus's exc[i].
            exc_buf.copy_from_slice(&buf[BUFFER - MAX_PERIOD - LPC_ORDER..BUFFER]);
            if new_pitch {
                let mut ac = [0f32; LPC_ORDER + 1];
                plc::autocorr(
                    &exc_buf[LPC_ORDER..],
                    &mut ac,
                    &WINDOW,
                    LPC_ORDER,
                    MAX_PERIOD,
                );
                ac[0] *= 1.0001;
                for (i, a) in ac.iter_mut().enumerate().skip(1) {
                    *a -= *a * (0.008 * 0.008) * (i * i) as f32;
                }
                plc::lpc(&mut self.lpc[c], &ac, LPC_ORDER);
            }
            let at = LPC_ORDER + MAX_PERIOD - exc_length;
            plc::fir(
                &exc_buf,
                at,
                &self.lpc[c],
                &mut fir_tmp,
                exc_length,
                LPC_ORDER,
            );
            exc_buf[at..at + exc_length].copy_from_slice(&fir_tmp);
            let exc = &exc_buf[LPC_ORDER..];
            let decay_length = exc_length >> 1;
            let (mut e1, mut e2) = (1f32, 1f32);
            for i in 0..decay_length {
                let e = exc[MAX_PERIOD - decay_length + i];
                e1 += e * e;
                let e = exc[MAX_PERIOD - 2 * decay_length + i];
                e2 += e * e;
            }
            let e1 = e1.min(e2);
            let decay = (e1 / e2).sqrt();
            buf.copy_within(n..BUFFER, 0);
            let extrapolation_offset = MAX_PERIOD - pitch_index;
            let mut attenuation = fade * decay;
            let mut s1 = 0f32;
            let mut j = 0;
            for i in 0..extrapolation_len {
                if j >= pitch_index {
                    j -= pitch_index;
                    attenuation *= decay;
                }
                buf[BUFFER - n + i] = attenuation * exc[extrapolation_offset + j];
                let tmp = buf[BUFFER - MAX_PERIOD - n + extrapolation_offset + j];
                s1 += tmp * tmp;
                j += 1;
            }
            let mut lpc_mem = [0f32; LPC_ORDER];
            for (i, m) in lpc_mem.iter_mut().enumerate() {
                *m = buf[BUFFER - n - 1 - i];
            }
            plc::iir(
                &mut buf[BUFFER - n..],
                &self.lpc[c],
                extrapolation_len,
                LPC_ORDER,
                &mut lpc_mem,
            );
            let s2: f32 = buf[BUFFER - n..BUFFER - n + extrapolation_len]
                .iter()
                .map(|v| v * v)
                .sum();
            // Written to catch a NaN out of the IIR as well (libopus).
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let exploded = !(s1 > 0.2 * s2);
            if exploded {
                buf[BUFFER - n..BUFFER - n + extrapolation_len].fill(0.0);
            } else if s1 < s2 {
                let ratio = ((s1 + 1.0) / (s2 + 1.0)).sqrt();
                for i in 0..OVERLAP {
                    let g = 1.0 - WINDOW[i] * (1.0 - ratio);
                    buf[BUFFER - n + i] *= g;
                }
                for v in &mut buf[BUFFER - n + OVERLAP..BUFFER - n + extrapolation_len] {
                    *v *= ratio;
                }
            }
        }
        self.prefilter_and_fold = true;
    }

    /// The concealed overlap made to look like an MDCT's, with the
    /// post-filter undone, so the next decoded frame blends with it.
    fn prefilter_and_fold(&mut self, n: usize) {
        for ch in &mut self.mem {
            let mut etmp = [0f32; OVERLAP];
            comb_filter_into(
                &mut etmp,
                ch,
                BUFFER - n,
                self.postfilter_period,
                OVERLAP,
                -self.postfilter_gain,
                self.postfilter_tapset,
            );
            for i in 0..OVERLAP / 2 {
                ch[BUFFER - n + i] =
                    WINDOW[i] * etmp[OVERLAP - 1 - i] + WINDOW[OVERLAP - i - 1] * etmp[i];
            }
        }
    }

    /// After a loss, an energy prediction that cannot blow up.
    fn safe_energies(&mut self, start: usize, end: usize, lm: usize) {
        let missing = 10.min(self.loss_duration >> lm);
        let safety = match lm {
            0 => 1.5,
            1 => 0.5,
            _ => 0.0,
        };
        for c in 0..2 {
            for i in start..end {
                let k = c * NB_EBANDS + i;
                let (e0, e1, e2) = (self.old_band_e[k], self.old_log_e[k], self.old_log_e2[k]);
                if e0 < e1.max(e2) {
                    let slope = (e1 - e0).max(0.5 * (e2 - e0)).min(2.0);
                    let e0 = e0 - (0f32).max((1 + missing) as f32 * slope);
                    self.old_band_e[k] = (-20f32).max(e0);
                } else {
                    self.old_band_e[k] = e0.min(e1).min(e2);
                }
                self.old_band_e[k] -= safety;
            }
        }
    }

    /// The band shapes, denormalised and through the inverse MDCT into the
    /// last `n` samples of each channel's history.
    #[allow(clippy::too_many_arguments)]
    fn synthesis(
        &mut self,
        x: &[f32],
        y: &[f32],
        start: usize,
        eff_end: usize,
        c_count: usize,
        transient: bool,
        lm: usize,
        silence: bool,
    ) {
        let m = 1usize << lm;
        let n = m * SHORT_MDCT_SIZE;
        let (b_count, nb, shift) = if transient {
            (m, SHORT_MDCT_SIZE, MAX_LM as usize)
        } else {
            (1, n, MAX_LM as usize - lm)
        };
        let out_at = BUFFER - n;
        let mut freq = [0f32; 960];
        if c_count == 1 {
            denormalise_bands(
                x,
                &mut freq,
                &self.old_band_e[..NB_EBANDS],
                start,
                eff_end,
                m,
                silence,
            );
            for ch in &mut self.mem {
                for b in 0..b_count {
                    imdct(
                        &freq[b..],
                        &mut ch[out_at + nb * b..],
                        &WINDOW,
                        OVERLAP,
                        shift,
                        b_count,
                    );
                }
            }
        } else {
            for (c, ch) in self.mem.iter_mut().enumerate() {
                let (shape, e) = if c == 0 {
                    (x, &self.old_band_e[..NB_EBANDS])
                } else {
                    (y, &self.old_band_e[NB_EBANDS..])
                };
                denormalise_bands(shape, &mut freq, e, start, eff_end, m, silence);
                for b in 0..b_count {
                    imdct(
                        &freq[b..],
                        &mut ch[out_at + nb * b..],
                        &WINDOW,
                        OVERLAP,
                        shift,
                        b_count,
                    );
                }
            }
        }
    }

    fn post_filter(&mut self, lm: usize, n: usize, pitch: i32, gain: f32, tapset: usize) {
        let out_at = BUFFER - n;
        self.postfilter_period = self.postfilter_period.max(COMBFILTER_MINPERIOD);
        self.postfilter_period_old = self.postfilter_period_old.max(COMBFILTER_MINPERIOD);
        for ch in &mut self.mem {
            comb_filter(
                ch,
                out_at,
                self.postfilter_period_old,
                self.postfilter_period,
                SHORT_MDCT_SIZE,
                self.postfilter_gain_old,
                self.postfilter_gain,
                self.postfilter_tapset_old,
                self.postfilter_tapset,
                OVERLAP,
            );
            if lm != 0 {
                comb_filter(
                    ch,
                    out_at + SHORT_MDCT_SIZE,
                    self.postfilter_period,
                    pitch,
                    n - SHORT_MDCT_SIZE,
                    self.postfilter_gain,
                    gain,
                    self.postfilter_tapset,
                    tapset,
                    OVERLAP,
                );
            }
        }
        self.postfilter_period_old = self.postfilter_period;
        self.postfilter_gain_old = self.postfilter_gain;
        self.postfilter_tapset_old = self.postfilter_tapset;
        self.postfilter_period = pitch;
        self.postfilter_gain = gain;
        self.postfilter_tapset = tapset;
        if lm != 0 {
            self.postfilter_period_old = self.postfilter_period;
            self.postfilter_gain_old = self.postfilter_gain;
            self.postfilter_tapset_old = self.postfilter_tapset;
        }
    }

    fn deemphasis(&mut self, n: usize, pcm: &mut [f32]) {
        let out_at = BUFFER - n;
        for c in 0..2 {
            let mut m = self.preemph_mem[c];
            let x = &self.mem[c][out_at..out_at + n];
            for j in 0..n {
                let tmp = x[j] + 1e-30 + m;
                m = PREEMPH * tmp;
                pcm[2 * j + c] = tmp * (1.0 / 32768.0);
            }
            self.preemph_mem[c] = m;
        }
    }
}

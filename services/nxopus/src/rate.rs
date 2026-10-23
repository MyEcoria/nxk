// Ported from libopus 1.6.1, celt/rate.[ch] and celt/celt.c (init_caps).
// Copyright 2007-2009 Xiph.Org Foundation, Gregory Maxwell, Jean-Marc Valin,
// Timothy B. Terriberry. BSD-3-Clause, see LICENSE in this crate.

//! Bit allocation (RFC 6716 §4.3.3): what each band gets for fine energy
//! and for its PVQ pulses. Every step reads the same symbols the encoder
//! wrote, so it must match libopus to the bit.

use crate::mode::{NB_ALLOC_VECTORS, NB_EBANDS};
use crate::range::{RangeDecoder, BITRES};
use crate::tables::{BAND_ALLOCATION, CACHE_BITS, CACHE_CAPS, CACHE_INDEX, EBANDS, LOGN};

pub const MAX_FINE_BITS: i32 = 8;
const FINE_OFFSET: i32 = 21;
const LOG_MAX_PSEUDO: usize = 6;
const ALLOC_STEPS: i32 = 6;
pub const QTHETA_OFFSET: i32 = 4;
pub const QTHETA_OFFSET_TWOPHASE: i32 = 16;

const LOG2_FRAC_TABLE: [u8; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

fn width(j: usize) -> i32 {
    (EBANDS[j + 1] - EBANDS[j]) as i32
}

fn pulse_cache(band: usize, lm: i32) -> &'static [u8] {
    let at = CACHE_INDEX[(lm + 1) as usize * NB_EBANDS + band] as usize;
    &CACHE_BITS[at..]
}

pub fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

pub fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    let cache = pulse_cache(band, lm);
    let (mut lo, mut hi) = (0usize, cache[0] as usize);
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid] as i32 >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let below = if lo == 0 { -1 } else { cache[lo] as i32 };
    if bits - below <= cache[hi] as i32 - bits {
        lo as i32
    } else {
        hi as i32
    }
}

pub fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    if pulses == 0 {
        0
    } else {
        pulse_cache(band, lm)[pulses as usize] as i32 + 1
    }
}

/// The most bits each band can use.
pub fn init_caps(cap: &mut [i32; NB_EBANDS], lm: i32, c: i32) {
    for (i, cap) in cap.iter_mut().enumerate() {
        let n = width(i) << lm;
        let row = NB_EBANDS * (2 * lm + c - 1) as usize;
        *cap = ((CACHE_CAPS[row + i] as i32 + 64) * c * n) >> 2;
    }
}

/// What the allocation decided for one frame.
pub struct Allocation {
    pub coded_bands: usize,
    pub intensity: usize,
    pub dual_stereo: bool,
    pub balance: i32,
    pub pulses: [i32; NB_EBANDS],
    pub fine_quant: [i32; NB_EBANDS],
    pub fine_priority: [i32; NB_EBANDS],
}

/// `clt_compute_allocation`, decoder side.
#[allow(clippy::too_many_arguments)]
pub fn compute_allocation(
    start: usize,
    end: usize,
    offsets: &[i32; NB_EBANDS],
    cap: &[i32; NB_EBANDS],
    alloc_trim: i32,
    total: i32,
    c: i32,
    lm: i32,
    dec: &mut RangeDecoder,
) -> Allocation {
    let mut total = total.max(0);
    let mut skip_start = start;
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    let (mut intensity_rsv, mut dual_stereo_rsv) = (0, 0);
    if c == 2 {
        intensity_rsv = LOG2_FRAC_TABLE[end - start] as i32;
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }
    let mut bits1 = [0i32; NB_EBANDS];
    let mut bits2 = [0i32; NB_EBANDS];
    let mut thresh = [0i32; NB_EBANDS];
    let mut trim_offset = [0i32; NB_EBANDS];
    for j in start..end {
        let n = width(j);
        thresh[j] = (c << BITRES).max(((3 * n) << lm << BITRES) >> 4);
        trim_offset[j] =
            (c * n * (alloc_trim - 5 - lm) * (end as i32 - j as i32 - 1) * (1 << (lm + BITRES)))
                >> 6;
        if n << lm == 1 {
            trim_offset[j] -= c << BITRES;
        }
    }
    let alloc = |v: usize, j: usize| BAND_ALLOCATION[v * NB_EBANDS + j] as i32;
    let (mut lo, mut hi) = (1i32, NB_ALLOC_VECTORS as i32 - 1);
    loop {
        let mut done = false;
        let mut psum = 0;
        let mid = (lo + hi) >> 1;
        for j in (start..end).rev() {
            let mut bitsj = (c * width(j) * alloc(mid as usize, j)) << lm >> 2;
            if bitsj > 0 {
                bitsj = (bitsj + trim_offset[j]).max(0);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= c << BITRES {
                psum += c << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;
    for j in start..end {
        let n = width(j);
        let mut bits1j = (c * n * alloc(lo as usize, j)) << lm >> 2;
        let mut bits2j = if hi >= NB_ALLOC_VECTORS as i32 {
            cap[j]
        } else {
            (c * n * alloc(hi as usize, j)) << lm >> 2
        };
        if bits1j > 0 {
            bits1j = (bits1j + trim_offset[j]).max(0);
        }
        if bits2j > 0 {
            bits2j = (bits2j + trim_offset[j]).max(0);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j;
        }
        bits1[j] = bits1j;
        bits2[j] = (bits2j - bits1j).max(0);
    }
    let mut a = Allocation {
        coded_bands: 0,
        intensity: 0,
        dual_stereo: false,
        balance: 0,
        pulses: [0; NB_EBANDS],
        fine_quant: [0; NB_EBANDS],
        fine_priority: [0; NB_EBANDS],
    };
    let r = Interp {
        start,
        end,
        skip_start,
        bits1: &bits1,
        bits2: &bits2,
        thresh: &thresh,
        cap,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        c,
        lm,
    };
    r.run(&mut a, dec);
    a
}

struct Interp<'a> {
    start: usize,
    end: usize,
    skip_start: usize,
    bits1: &'a [i32; NB_EBANDS],
    bits2: &'a [i32; NB_EBANDS],
    thresh: &'a [i32; NB_EBANDS],
    cap: &'a [i32; NB_EBANDS],
    total: i32,
    skip_rsv: i32,
    intensity_rsv: i32,
    dual_stereo_rsv: i32,
    c: i32,
    lm: i32,
}

impl Interp<'_> {
    /// `interp_bits2pulses`, decoder side.
    fn run(&self, a: &mut Allocation, dec: &mut RangeDecoder) {
        let (start, end, c, lm) = (self.start, self.end, self.c, self.lm);
        let (bits1, bits2, thresh, cap) = (self.bits1, self.bits2, self.thresh, self.cap);
        let mut total = self.total;
        let mut intensity_rsv = self.intensity_rsv;
        let mut dual_stereo_rsv = self.dual_stereo_rsv;
        let alloc_floor = c << BITRES;
        let stereo = (c > 1) as i32;
        let log_m = lm << BITRES;
        let (mut lo, mut hi) = (0i32, 1i32 << ALLOC_STEPS);
        for _ in 0..ALLOC_STEPS {
            let mid = (lo + hi) >> 1;
            let mut psum = 0;
            let mut done = false;
            for j in (start..end).rev() {
                let tmp = bits1[j] + ((mid * bits2[j]) >> ALLOC_STEPS);
                if tmp >= thresh[j] || done {
                    done = true;
                    psum += tmp.min(cap[j]);
                } else if tmp >= alloc_floor {
                    psum += alloc_floor;
                }
            }
            if psum > total {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        let bits = &mut a.pulses;
        let ebits = &mut a.fine_quant;
        let fine_priority = &mut a.fine_priority;
        let mut psum = 0;
        let mut done = false;
        for j in (start..end).rev() {
            let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
            if tmp < thresh[j] && !done {
                tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
            } else {
                done = true;
            }
            tmp = tmp.min(cap[j]);
            bits[j] = tmp;
            psum += tmp;
        }
        let eb = |k: usize| EBANDS[k] as i32;
        let mut coded_bands = end;
        loop {
            let j = coded_bands - 1;
            if j <= self.skip_start {
                total += self.skip_rsv;
                break;
            }
            let mut left = total - psum;
            let span = eb(coded_bands) - eb(start);
            let percoeff = (left as u32 / span as u32) as i32;
            left -= span * percoeff;
            let rem = (left - (eb(j) - eb(start))).max(0);
            let band_width = eb(coded_bands) - eb(j);
            let mut band_bits = bits[j] + percoeff * band_width + rem;
            if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
                if dec.bit_logp(1) {
                    break;
                }
                psum += 1 << BITRES;
                band_bits -= 1 << BITRES;
            }
            psum -= bits[j] + intensity_rsv;
            if intensity_rsv > 0 {
                intensity_rsv = LOG2_FRAC_TABLE[j - start] as i32;
            }
            psum += intensity_rsv;
            if band_bits >= alloc_floor {
                psum += alloc_floor;
                bits[j] = alloc_floor;
            } else {
                bits[j] = 0;
            }
            coded_bands -= 1;
        }
        a.intensity = if intensity_rsv > 0 {
            start + dec.uint((coded_bands + 1 - start) as u32) as usize
        } else {
            0
        };
        if a.intensity <= start {
            total += dual_stereo_rsv;
            dual_stereo_rsv = 0;
        }
        a.dual_stereo = dual_stereo_rsv > 0 && dec.bit_logp(1);
        let mut left = total - psum;
        let span = eb(coded_bands) - eb(start);
        let percoeff = (left as u32 / span as u32) as i32;
        left -= span * percoeff;
        for (j, b) in bits.iter_mut().enumerate().take(coded_bands).skip(start) {
            *b += percoeff * width(j);
        }
        for (j, b) in bits.iter_mut().enumerate().take(coded_bands).skip(start) {
            let tmp = left.min(width(j));
            *b += tmp;
            left -= tmp;
        }
        let mut balance = 0;
        let mut j = start;
        while j < coded_bands {
            let n0 = width(j);
            let n = n0 << lm;
            let bit = bits[j] + balance;
            let mut excess;
            if n > 1 {
                excess = (bit - cap[j]).max(0);
                bits[j] = bit - excess;
                let den = c * n + (c == 2 && n > 2 && !a.dual_stereo && j < a.intensity) as i32;
                let nclogn = den * (LOGN[j] as i32 + log_m);
                let mut offset = (nclogn >> 1) - den * FINE_OFFSET;
                if n == 2 {
                    offset += den << BITRES >> 2;
                }
                if bits[j] + offset < (den * 2) << BITRES {
                    offset += nclogn >> 2;
                } else if bits[j] + offset < (den * 3) << BITRES {
                    offset += nclogn >> 3;
                }
                ebits[j] = (bits[j] + offset + (den << (BITRES - 1))).max(0);
                ebits[j] = ((ebits[j] as u32 / den as u32) as i32) >> BITRES;
                if c * ebits[j] > bits[j] >> BITRES {
                    ebits[j] = bits[j] >> stereo >> BITRES;
                }
                ebits[j] = ebits[j].min(MAX_FINE_BITS);
                fine_priority[j] = (ebits[j] * (den << BITRES) >= bits[j] + offset) as i32;
                bits[j] -= (c * ebits[j]) << BITRES;
            } else {
                excess = (bit - (c << BITRES)).max(0);
                bits[j] = bit - excess;
                ebits[j] = 0;
                fine_priority[j] = 1;
            }
            if excess > 0 {
                let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
                ebits[j] += extra_fine;
                let extra_bits = (extra_fine * c) << BITRES;
                fine_priority[j] = (extra_bits >= excess - balance) as i32;
                excess -= extra_bits;
            }
            balance = excess;
            j += 1;
        }
        a.balance = balance;
        while j < end {
            ebits[j] = bits[j] >> stereo >> BITRES;
            bits[j] = 0;
            fine_priority[j] = (ebits[j] < 1) as i32;
            j += 1;
        }
        a.coded_bands = coded_bands;
    }
}

// Ported from libopus 1.6.1, celt/entdec.c, celt/entcode.[ch] and
// celt/laplace.c. Copyright 2001-2011 Xiph.Org, Skype Limited, Octasic,
// Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark
// Borgerding, Erik de Castro Lopo. BSD-3-Clause, see LICENSE in this crate.

//! The range decoder (RFC 6716 §4.1): symbols from the front of the
//! packet, raw bits from its end.

const SYM_BITS: u32 = 8;
const CODE_BITS: u32 = 32;
const SYM_MAX: u32 = (1 << SYM_BITS) - 1;
const CODE_TOP: u32 = 1 << (CODE_BITS - 1);
const CODE_BOT: u32 = CODE_TOP >> SYM_BITS;
const CODE_EXTRA: u32 = (CODE_BITS - 2) % SYM_BITS + 1;
const UINT_BITS: u32 = 8;
const WINDOW_SIZE: i32 = 32;
/// Fractional bits of `tell_frac`.
pub const BITRES: i32 = 3;

/// The number of bits of `x` (0 for 0).
pub fn ilog(x: u32) -> i32 {
    32 - x.leading_zeros() as i32
}

pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: u32,
    end_offs: u32,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    offs: u32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    pub error: bool,
}

impl<'a> RangeDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        let mut d = Self {
            buf,
            storage: buf.len() as u32,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: (CODE_BITS + 1 - ((CODE_BITS - CODE_EXTRA) / SYM_BITS) * SYM_BITS) as i32,
            offs: 0,
            rng: 1 << CODE_EXTRA,
            val: 0,
            ext: 0,
            rem: 0,
            error: false,
        };
        d.rem = d.read_byte();
        d.val = d.rng - 1 - (d.rem as u32 >> (SYM_BITS - CODE_EXTRA));
        d.normalize();
        d
    }

    /// The bytes the decoder works on.
    pub fn storage(&self) -> u32 {
        self.storage
    }

    /// The final range (what libopus reports as `OPUS_GET_FINAL_RANGE`).
    pub fn rng(&self) -> u32 {
        self.rng
    }

    fn read_byte(&mut self) -> i32 {
        if self.offs < self.storage {
            let b = self.buf[self.offs as usize];
            self.offs += 1;
            b as i32
        } else {
            0
        }
    }

    fn read_byte_from_end(&mut self) -> i32 {
        if self.end_offs < self.storage {
            self.end_offs += 1;
            self.buf[(self.storage - self.end_offs) as usize] as i32
        } else {
            0
        }
    }

    fn normalize(&mut self) {
        while self.rng <= CODE_BOT {
            self.nbits_total += SYM_BITS as i32;
            self.rng <<= SYM_BITS;
            let mut sym = self.rem;
            self.rem = self.read_byte();
            sym = (sym << SYM_BITS | self.rem) >> (SYM_BITS - CODE_EXTRA);
            self.val =
                (self.val << SYM_BITS).wrapping_add(SYM_MAX & !(sym as u32)) & (CODE_TOP - 1);
        }
    }

    /// Counts the frame as read up to bit `tell` (a silent frame).
    pub fn skip_to(&mut self, tell: i32) {
        self.nbits_total += tell - self.tell();
    }

    /// Bits used so far, rounded up.
    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng)
    }

    /// Bits used so far, in 1/8 bits.
    pub fn tell_frac(&self) -> u32 {
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = (self.nbits_total << BITRES) as u32;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12) - 8;
        b += (r > CORRECTION[b as usize]) as u32;
        let l = ((l << 3) as u32) + b;
        nbits.wrapping_sub(l)
    }

    pub fn decode(&mut self, ft: u32) -> u32 {
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        ft - (s + 1).min(ft)
    }

    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        self.ext = self.rng >> bits;
        let s = self.val / self.ext;
        (1 << bits) - (s + 1).min(1 << bits)
    }

    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext.wrapping_mul(ft - fh);
        self.val = self.val.wrapping_sub(s);
        self.rng = if fl > 0 {
            self.ext.wrapping_mul(fh - fl)
        } else {
            self.rng.wrapping_sub(s)
        };
        self.normalize();
    }

    /// A bit whose probability of being 1 is 1/2^`logp`.
    pub fn bit_logp(&mut self, logp: u32) -> bool {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = d < s;
        if !ret {
            self.val = d - s;
        }
        self.rng = if ret { s } else { r - s };
        self.normalize();
        ret
    }

    /// A symbol by an inverse cumulative table (`ftb` bits of total).
    pub fn icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        let mut s = self.rng;
        let d = self.val;
        let r = s >> ftb;
        let mut ret = 0usize;
        let mut t;
        loop {
            t = s;
            s = r.wrapping_mul(icdf[ret] as u32);
            if d >= s {
                break;
            }
            ret += 1;
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret
    }

    /// A value uniformly in `0..ft`.
    pub fn uint(&mut self, ft: u32) -> u32 {
        let ft1 = ft - 1;
        let mut ftb = ilog(ft1) as u32;
        if ftb > UINT_BITS {
            ftb -= UINT_BITS;
            let f = (ft1 >> ftb) + 1;
            let s = self.decode(f);
            self.update(s, s + 1, f);
            let t = s << ftb | self.bits(ftb);
            if t <= ft1 {
                return t;
            }
            self.error = true;
            ft1
        } else {
            let s = self.decode(ft);
            self.update(s, s + 1, ft);
            s
        }
    }

    /// `bits` raw bits from the end of the packet.
    pub fn bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if (available as u32) < bits {
            loop {
                window |= (self.read_byte_from_end() as u32) << available;
                available += SYM_BITS as i32;
                if available > WINDOW_SIZE - SYM_BITS as i32 {
                    break;
                }
            }
        }
        let ret = window & ((1u64 << bits) - 1) as u32;
        window = if bits >= 32 { 0 } else { window >> bits };
        available -= bits as i32;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;
        ret
    }

    /// A Laplace-distributed value (`fs` the probability of 0, `decay`).
    pub fn laplace(&mut self, mut fs: u32, decay: i32) -> i32 {
        const MINP: u32 = 1;
        const NMIN: u32 = 16;
        let mut val = 0i32;
        let fm = self.decode_bin(15);
        let mut fl = 0u32;
        if fm >= fs {
            val += 1;
            fl = fs;
            let ft = 32768 - MINP * (2 * NMIN) - fs;
            fs = ((ft as i32 * (16384 - decay)) >> 15) as u32 + MINP;
            while fs > MINP && fm >= fl + 2 * fs {
                fs *= 2;
                fl += fs;
                fs = (((fs - 2 * MINP) as i32 * decay) >> 15) as u32;
                fs += MINP;
                val += 1;
            }
            if fs <= MINP {
                let di = (fm - fl) >> 1;
                val += di as i32;
                fl += 2 * di * MINP;
            }
            if fm < fl + fs {
                val = -val;
            } else {
                fl += fs;
            }
        }
        self.update(fl, (fl + fs).min(32768), 32768);
        val
    }
}

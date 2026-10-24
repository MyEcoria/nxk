//! The CABAC arithmetic decoder (9.3.1.2 and 9.3.3.2) and the context
//! variables of a slice (9.3.1.1).

use crate::tables::{CABAC_INIT, CTX_COUNT, RANGE_LPS, STATE_TRANS};

/// Context variables: pStateIdx in the high bits, valMPS in bit 0.
pub struct Contexts([u8; CTX_COUNT]);

impl Contexts {
    /// Initialised for a slice: `init_idc` is `cabac_init_idc` for P slices
    /// and `None` for I slices.
    pub fn new(init_idc: Option<u32>, slice_qp: i32) -> Self {
        let model = init_idc.map_or(0, |i| i as usize + 1);
        let qp = slice_qp.clamp(0, 51);
        let mut c = [0u8; CTX_COUNT];
        for (i, v) in c.iter_mut().enumerate() {
            let [m, n] = CABAC_INIT[i][model];
            let pre = (((m as i32 * qp) >> 4) + n as i32).clamp(1, 126);
            *v = if pre <= 63 {
                ((63 - pre) as u8) << 1
            } else {
                (((pre - 64) as u8) << 1) | 1
            };
        }
        Self(c)
    }
}

pub struct Cabac<'a> {
    data: &'a [u8],
    pos: usize,
    cache: u64,
    nbits: u32,
    range: u32,
    offset: u32,
}

impl<'a> Cabac<'a> {
    /// Starts the decoding engine at the first byte of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        let mut c = Self {
            data,
            pos: 0,
            cache: 0,
            nbits: 0,
            range: 510,
            offset: 0,
        };
        c.offset = c.read_bits(9);
        c
    }

    fn refill(&mut self) {
        while self.nbits <= 56 {
            let b = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.cache = self.cache << 8 | b as u64;
            self.nbits += 8;
        }
    }

    #[inline]
    fn read_bits(&mut self, n: u32) -> u32 {
        if self.nbits < n {
            self.refill();
        }
        self.nbits -= n;
        ((self.cache >> self.nbits) & ((1u64 << n) - 1)) as u32
    }

    /// Bytes read past the end of the data (a damaged slice).
    pub fn overrun(&self) -> bool {
        self.pos.saturating_sub((self.nbits / 8) as usize) > self.data.len() + 2
    }

    /// One bin with its context.
    #[inline]
    pub fn decision(&mut self, ctx: &mut Contexts, idx: usize) -> u32 {
        let v = ctx.0[idx];
        let (state, mps) = ((v >> 1) as usize, (v & 1) as u32);
        let lps = RANGE_LPS[state][((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        let bin;
        if self.offset >= self.range {
            self.offset -= self.range;
            self.range = lps;
            bin = mps ^ 1;
            let next_mps = if state == 0 { bin } else { mps };
            ctx.0[idx] = (STATE_TRANS[state][0] << 1) | next_mps as u8;
        } else {
            bin = mps;
            ctx.0[idx] = (STATE_TRANS[state][1] << 1) | mps as u8;
        }
        if self.range < 256 {
            let n = self.range.leading_zeros() - 23;
            self.range <<= n;
            self.offset = (self.offset << n) | self.read_bits(n);
        }
        bin
    }

    #[inline]
    pub fn bypass(&mut self) -> u32 {
        self.offset = (self.offset << 1) | self.read_bits(1);
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    /// end_of_slice_flag and the I_PCM bin.
    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        if self.offset >= self.range {
            return 1;
        }
        if self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bits(1);
        }
        0
    }

    /// After a terminating bin of 1: the byte the next syntax elements
    /// (I_PCM samples) start at, the bits read so far ending with the
    /// last bit the encoder's flush wrote.
    pub fn next_byte(&self) -> usize {
        let consumed_bits = self.pos * 8 - self.nbits as usize;
        consumed_bits.div_ceil(8)
    }

    /// The engine started again at byte `at` (after PCM samples).
    pub fn restart(&mut self, at: usize) {
        self.pos = at;
        self.cache = 0;
        self.nbits = 0;
        self.range = 510;
        self.offset = self.read_bits(9);
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_start_from_the_tables() {
        // ctxIdx 0 (mb_type SI prefix) is (20, -15) for every model:
        // at qp 26: ((20 * 26) >> 4) - 15 = 17, state 63 - 17 = 46, mps 0.
        let c = Contexts::new(None, 26);
        assert_eq!(c.0[0], 46 << 1);
        // ctxIdx 6 is (-28, 127): at qp 26 preCtxState is 81, state 17, MPS 1.
        assert_eq!(c.0[6], 17 << 1 | 1);
    }

    #[test]
    fn bypass_bins_follow_the_offset() {
        // An engine on all-zero data decodes bypass 0s forever.
        let d = [0u8; 16];
        let mut c = Cabac::new(&d);
        assert!((0..40).all(|_| c.bypass() == 0));
        // On all-ones data the offset is at its maximum: terminate says stop.
        let d = [0xffu8; 16];
        let mut c = Cabac::new(&d);
        assert_eq!(c.terminate(), 1);
    }
}

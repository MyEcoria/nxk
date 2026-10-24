//! Bit reading of an RBSP (the bytes of a NAL unit once its emulation
//! prevention bytes are out), Exp-Golomb codes included.

/// Bytes of a NAL unit's payload without the `00 00 03` escapes.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Why a bitstream could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eof;

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    pub fn bit(&mut self) -> Result<u32, Eof> {
        let byte = *self.data.get(self.pos >> 3).ok_or(Eof)?;
        let v = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(v as u32)
    }

    pub fn flag(&mut self) -> Result<bool, Eof> {
        Ok(self.bit()? != 0)
    }

    /// `n` bits (at most 32), most significant first.
    pub fn u(&mut self, n: u32) -> Result<u32, Eof> {
        if self.bits_left() < n as usize {
            return Err(Eof);
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = v << 1 | self.bit()? as u64;
        }
        Ok(v as u32)
    }

    pub fn ue(&mut self) -> Result<u32, Eof> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return Err(Eof);
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.u(zeros)? as u64;
        Ok(((1u64 << zeros) - 1 + rest) as u32)
    }

    pub fn se(&mut self) -> Result<i32, Eof> {
        let k = self.ue()?;
        Ok(if k & 1 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }

    /// True while data other than the stop bit and its padding remains.
    pub fn more_rbsp_data(&self) -> bool {
        let mut last = self.data.len();
        while last > 0 && self.data[last - 1] == 0 {
            last -= 1;
        }
        if last == 0 {
            return false;
        }
        let byte = self.data[last - 1];
        let stop = (last - 1) * 8 + (7 - byte.trailing_zeros() as usize);
        self.pos < stop
    }

    /// Skips to the next byte boundary.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// The bytes from the current (aligned) position on.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[(self.pos + 7) / 8..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_removed() {
        assert_eq!(unescape(&[1, 0, 0, 3, 1, 0, 0, 3]), [1, 0, 0, 1, 0, 0]);
        assert_eq!(unescape(&[0, 0, 3, 0, 0, 3, 5]), [0, 0, 0, 0, 5]);
        assert_eq!(unescape(&[0, 3, 7]), [0, 3, 7]);
    }

    #[test]
    fn exp_golomb_codes() {
        // 1 | 010 | 011 | 00100 | 00101 | 00110 -> ue 0, 1, 2, 3, 4, 5
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0010, 0b1001_1000]);
        let got: Vec<u32> = (0..6).map(|_| b.ue().unwrap()).collect();
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
        // se: 1 -> 0, 010 -> 1, 011 -> -1, 00100 -> 2, 00101 -> -2
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        let got: Vec<i32> = (0..5).map(|_| b.se().unwrap()).collect();
        assert_eq!(got, [0, 1, -1, 2, -2]);
    }

    #[test]
    fn reading_past_the_end_fails() {
        let mut b = Bits::new(&[0xff]);
        assert_eq!(b.u(8), Ok(255));
        assert_eq!(b.bit(), Err(Eof));
        assert_eq!(Bits::new(&[0]).ue(), Err(Eof));
    }

    #[test]
    fn the_stop_bit_ends_the_data() {
        // payload bit 1, stop bit, padding
        let mut b = Bits::new(&[0b1100_0000]);
        assert!(b.more_rbsp_data());
        b.bit().unwrap();
        assert!(!b.more_rbsp_data());
    }
}

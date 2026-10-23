//! nxopus: an Opus decoder (RFC 6716) for the CELT-only packets cloud
//! game streams send, ported from libopus 1.6.1's float build. SILK and
//! hybrid packets are refused.
//!
//! ```
//! let mut dec = nxopus::Decoder::new();
//! let mut pcm = [0i16; 2 * nxopus::MAX_FRAME];
//! // A 20 ms fullband stereo silence frame: TOC 0xfc, one byte of payload.
//! let n = dec.decode(&[0xfc, 0xff], &mut pcm).unwrap();
//! assert_eq!(n, 960);
//! ```

mod bands;
mod celt;
mod cwrs;
mod math;
mod mdct;
mod mode;
mod plc;
mod range;
mod rate;
#[allow(clippy::all)]
#[rustfmt::skip]
mod tables;
mod vq;

use mode::SHORT_MDCT_SIZE;

/// The most samples per channel one packet decodes to (120 ms).
pub const MAX_FRAME: usize = 5760;
const MAX_FRAME_BYTES: usize = 1275;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The packet does not parse (RFC 6716 §3.4).
    InvalidPacket,
    /// A SILK or hybrid packet: only CELT is decoded.
    Unsupported,
    /// `pcm` cannot hold the packet's samples.
    BufferTooSmall,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Error::InvalidPacket => "invalid Opus packet",
            Error::Unsupported => "SILK or hybrid Opus packet",
            Error::BufferTooSmall => "output buffer too small",
        })
    }
}

impl std::error::Error for Error {}

/// A packet's frames (RFC 6716 §3.2), as slices of it.
pub fn frames(packet: &[u8]) -> Result<(u8, Vec<&[u8]>), Error> {
    let toc = *packet.first().ok_or(Error::InvalidPacket)?;
    let mut rest = &packet[1..];
    let size = |rest: &mut &[u8]| -> Result<usize, Error> {
        let b0 = *rest.first().ok_or(Error::InvalidPacket)? as usize;
        if b0 < 252 {
            *rest = &rest[1..];
            Ok(b0)
        } else {
            let b1 = *rest.get(1).ok_or(Error::InvalidPacket)? as usize;
            *rest = &rest[2..];
            Ok(b1 * 4 + b0)
        }
    };
    let out: Vec<&[u8]> = match toc & 3 {
        0 => vec![rest],
        1 => {
            if !rest.len().is_multiple_of(2) {
                return Err(Error::InvalidPacket);
            }
            let (a, b) = rest.split_at(rest.len() / 2);
            vec![a, b]
        }
        2 => {
            let n = size(&mut rest)?;
            if n > rest.len() {
                return Err(Error::InvalidPacket);
            }
            let (a, b) = rest.split_at(n);
            vec![a, b]
        }
        _ => {
            let fc = *rest.first().ok_or(Error::InvalidPacket)?;
            rest = &rest[1..];
            let count = (fc & 0x3f) as usize;
            if count == 0 {
                return Err(Error::InvalidPacket);
            }
            if fc & 0x40 != 0 {
                let mut padding = 0usize;
                loop {
                    let p = *rest.first().ok_or(Error::InvalidPacket)? as usize;
                    rest = &rest[1..];
                    padding += if p == 255 { 254 } else { p };
                    if p != 255 {
                        break;
                    }
                }
                if padding > rest.len() {
                    return Err(Error::InvalidPacket);
                }
                rest = &rest[..rest.len() - padding];
            }
            let mut sizes = Vec::with_capacity(count);
            if fc & 0x80 != 0 {
                for _ in 0..count - 1 {
                    sizes.push(size(&mut rest)?);
                }
                let used: usize = sizes.iter().sum();
                if used > rest.len() {
                    return Err(Error::InvalidPacket);
                }
                sizes.push(rest.len() - used);
            } else {
                if !rest.len().is_multiple_of(count) {
                    return Err(Error::InvalidPacket);
                }
                sizes.resize(count, rest.len() / count);
            }
            let mut out = Vec::with_capacity(count);
            for s in sizes {
                let (a, b) = rest.split_at(s);
                out.push(a);
                rest = b;
            }
            out
        }
    };
    if out.iter().any(|f| f.len() > MAX_FRAME_BYTES) {
        return Err(Error::InvalidPacket);
    }
    Ok((toc, out))
}

/// A decoder for a 48 kHz stereo output (a mono stream is played on both
/// channels).
pub struct Decoder {
    celt: celt::Celt,
    softclip_mem: [f32; 2],
    final_range: u32,
    buf: Vec<f32>,
    /// Samples per channel of the last packet, the length a loss repeats.
    last_frame: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            celt: celt::Celt::default(),
            softclip_mem: [0.0; 2],
            final_range: 0,
            buf: vec![0.0; 2 * MAX_FRAME],
            last_frame: 960,
        }
    }

    /// The range coder's final state after the last packet, as libopus's
    /// `OPUS_GET_FINAL_RANGE` reports it (equal means decoded identically).
    pub fn final_range(&self) -> u32 {
        self.final_range
    }

    /// Decodes `packet` into `pcm` (interleaved stereo, ±1.0, soft-clipped);
    /// returns the samples per channel.
    pub fn decode_float(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, Error> {
        let (toc, frames) = frames(packet)?;
        let config = toc >> 3;
        if config < 16 {
            return Err(Error::Unsupported);
        }
        let lm = (config & 3) as usize;
        let end = match (config - 16) >> 2 {
            0 => 13,
            1 => 17,
            2 => 19,
            _ => 21,
        };
        let c_count = if toc & 4 != 0 { 2 } else { 1 };
        let n = SHORT_MDCT_SIZE << lm;
        let total = n * frames.len();
        if total > MAX_FRAME {
            return Err(Error::InvalidPacket);
        }
        if pcm.len() < 2 * total {
            return Err(Error::BufferTooSmall);
        }
        for (k, f) in frames.iter().enumerate() {
            let out = &mut pcm[2 * n * k..2 * n * (k + 1)];
            if f.len() <= 1 {
                self.celt.conceal(lm, out);
                self.final_range = 0;
            } else {
                self.celt
                    .decode(f, lm, c_count, end, out)
                    .map_err(|_| Error::InvalidPacket)?;
                self.final_range = self.celt.rng;
            }
        }
        soft_clip(&mut pcm[..2 * total], &mut self.softclip_mem);
        self.last_frame = total;
        Ok(total)
    }

    /// Like [`Decoder::decode_float`], to 16-bit samples.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, Error> {
        let mut buf = std::mem::take(&mut self.buf);
        let len = pcm.len().min(buf.len());
        let r = self.decode_float(packet, &mut buf[..len]);
        if let Ok(n) = r {
            to_i16(&buf[..2 * n], pcm);
        }
        self.buf = buf;
        r
    }

    /// Fills `pcm` (interleaved stereo, ±1.0) for a packet that never
    /// came, as long as the last one was; returns the samples per channel.
    pub fn conceal_float(&mut self, pcm: &mut [f32]) -> usize {
        let lm = match self.last_frame {
            120 => 0,
            240 => 1,
            480 => 2,
            _ => 3,
        };
        let frame = SHORT_MDCT_SIZE << lm;
        let n = self.last_frame.min(pcm.len() / 2) / frame * frame;
        let mut at = 0;
        while at < n {
            self.celt.conceal(lm, &mut pcm[2 * at..2 * (at + frame)]);
            at += frame;
        }
        soft_clip(&mut pcm[..2 * n], &mut self.softclip_mem);
        self.final_range = 0;
        n
    }

    /// Like [`Decoder::conceal_float`], to 16-bit samples.
    pub fn conceal(&mut self, pcm: &mut [i16]) -> usize {
        let mut buf = std::mem::take(&mut self.buf);
        let len = pcm.len().min(buf.len());
        let n = self.conceal_float(&mut buf[..len]);
        to_i16(&buf[..2 * n], pcm);
        self.buf = buf;
        n
    }
}

fn to_i16(x: &[f32], out: &mut [i16]) {
    for (o, &v) in out.iter_mut().zip(x) {
        *o = (v * 32768.0).clamp(-32768.0, 32767.0).round_ties_even() as i16;
    }
}

/// `opus_pcm_soft_clip`: a smooth curve instead of a hard clip past ±1.
fn soft_clip(x: &mut [f32], mem: &mut [f32; 2]) {
    let n = x.len() / 2;
    for v in x.iter_mut() {
        *v = v.clamp(-2.0, 2.0);
    }
    for (c, m) in mem.iter_mut().enumerate() {
        let at = |i: usize| 2 * i + c;
        let mut a = *m;
        for i in 0..n {
            if x[at(i)] * a >= 0.0 {
                break;
            }
            x[at(i)] += a * x[at(i)] * x[at(i)];
        }
        let mut curr = 0;
        let x0 = x[at(0)];
        loop {
            let mut i = curr;
            while i < n && x[at(i)] <= 1.0 && x[at(i)] >= -1.0 {
                i += 1;
            }
            if i == n {
                a = 0.0;
                break;
            }
            let mut peak_pos = i;
            let (mut start, mut end) = (i, i);
            let mut maxval = x[at(i)].abs();
            while start > 0 && x[at(i)] * x[at(start - 1)] >= 0.0 {
                start -= 1;
            }
            while end < n && x[at(i)] * x[at(end)] >= 0.0 {
                if x[at(end)].abs() > maxval {
                    maxval = x[at(end)].abs();
                    peak_pos = end;
                }
                end += 1;
            }
            let special = start == 0 && x[at(i)] * x[at(0)] >= 0.0;
            a = (maxval - 1.0) / (maxval * maxval);
            a += a * 2.4e-7;
            if x[at(i)] > 0.0 {
                a = -a;
            }
            for k in start..end {
                x[at(k)] += a * x[at(k)] * x[at(k)];
            }
            if special && peak_pos >= 2 {
                let mut offset = x0 - x[at(0)];
                let delta = offset / peak_pos as f32;
                for k in curr..peak_pos {
                    offset -= delta;
                    x[at(k)] += offset;
                    x[at(k)] = x[at(k)].clamp(-1.0, 1.0);
                }
            }
            curr = end;
            if curr == n {
                break;
            }
        }
        *m = a;
    }
}

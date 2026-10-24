//! Slice data: the macroblock layer read with CABAC (7.3.5, 9.3) and the
//! reconstruction of each macroblock (8.3 intra, 8.4 inter, 8.5 residual)
//! as it is read. Neighbours count only inside the same slice.

use crate::cabac::{Cabac, Contexts};
use crate::inter;
use crate::intra::{self, Avail};
use crate::params::{Pps, SliceHeader, SliceType, Sps};
use crate::pic::{Band, Kind, MbInfo, Picture};
use crate::tables::{LAST_CTX_8X8, SIG_CTX_8X8, ZIGZAG_4X4, ZIGZAG_8X8};
use crate::transform;
use crate::Error;

/// What a slice is decoded from.
pub struct SliceInput<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub hdr: &'a SliceHeader,
    /// Reference list 0.
    pub refs: &'a [&'a Picture],
    /// 1-based number of the slice in its picture.
    pub slice_id: u16,
}

const CBF_OFF: [usize; 5] = [0, 4, 8, 12, 16];
const SIG_OFF: [usize; 5] = [0, 15, 29, 44, 47];
const ABS_OFF: [usize; 5] = [0, 10, 20, 30, 39];

/// Chroma QP for an index (Table 8-15).
fn chroma_qp(qpi: i32) -> i32 {
    const T: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    let q = qpi.clamp(0, 51);
    if q < 30 {
        q
    } else {
        T[(q - 30) as usize]
    }
}

/// Position of 4x4 block `i` in z-order, in 4x4 units.
#[inline]
fn blk_xy(i: usize) -> (usize, usize) {
    (
        ((i >> 2) & 1) * 2 + (i & 1),
        ((i >> 3) & 1) * 2 + ((i >> 1) & 1),
    )
}

/// z-order index of the 4x4 block at (bx, by).
#[inline]
fn blk_idx(bx: usize, by: usize) -> usize {
    ((by >> 1) << 3) | ((bx >> 1) << 2) | ((by & 1) << 1) | (bx & 1)
}

struct Residual {
    luma_dc: [i32; 16],
    luma4: [[i32; 16]; 16],
    luma8: [[i32; 64]; 4],
    chroma_dc: [[i32; 4]; 2],
    chroma_ac: [[[i32; 16]; 4]; 2],
}

impl Default for Residual {
    fn default() -> Self {
        Self {
            luma_dc: [0; 16],
            luma4: [[0; 16]; 16],
            luma8: [[0; 64]; 4],
            chroma_dc: [[0; 4]; 2],
            chroma_ac: [[[0; 16]; 4]; 2],
        }
    }
}

struct Dec<'a, 'p> {
    inp: &'a SliceInput<'a>,
    cabac: Cabac<'a>,
    ctx: Contexts,
    ybuf: &'p mut [u8],
    ubuf: &'p mut [u8],
    vbuf: &'p mut [u8],
    mbs: &'p mut [MbInfo],
    /// First macroblock and first macroblock row of the band.
    mb0: usize,
    row0: usize,
    ys: usize,
    cs: usize,
    slice_map: &'a [u16],
    cqo: [i32; 2],
    mb_w: usize,
    idx: usize,
    mx: usize,
    my: usize,
    /// `my` within the band.
    ly: usize,
    qp: i32,
    last_dqp_nonzero: bool,
    left: bool,
    top: bool,
    top_left: bool,
    top_right: bool,
    res: Box<Residual>,
}

/// Decodes the slice data of `rbsp` (from the header's end) into `band`.
/// `slice_map` tells the slice of each macroblock of the picture: a
/// neighbour counts when it is in the same slice.
pub fn decode_slice(
    inp: &SliceInput,
    rbsp: &[u8],
    band: &mut Band,
    slice_map: &[u16],
    chroma_qp_offset: [i32; 2],
) -> Result<(), Error> {
    let data = rbsp.get(inp.hdr.data_start..).ok_or(Error::Truncated)?;
    let init = (inp.hdr.slice_type == SliceType::P).then_some(inp.hdr.cabac_init_idc);
    let mb_w = inp.sps.mb_width as usize;
    let mut d = Dec {
        inp,
        cabac: Cabac::new(data),
        ctx: Contexts::new(init, inp.hdr.qp),
        ybuf: &mut *band.y,
        ubuf: &mut *band.u,
        vbuf: &mut *band.v,
        mbs: &mut *band.mbs,
        mb0: band.row0 * mb_w,
        row0: band.row0,
        ys: band.ys,
        cs: band.cs,
        slice_map,
        cqo: chroma_qp_offset,
        mb_w,
        idx: inp.hdr.first_mb as usize,
        mx: 0,
        my: 0,
        ly: 0,
        qp: inp.hdr.qp,
        last_dqp_nonzero: false,
        left: false,
        top: false,
        top_left: false,
        top_right: false,
        res: Box::default(),
    };
    let end = d.mb0 + d.mbs.len();
    loop {
        if d.idx >= end || d.idx < d.mb0 {
            return Err(Error::Invalid("slice runs past its band"));
        }
        d.mx = d.idx % d.mb_w;
        d.my = d.idx / d.mb_w;
        d.ly = d.my - d.row0;
        d.neighbours();
        d.macroblock()?;
        if d.cabac.overrun() {
            return Err(Error::Truncated);
        }
        d.idx += 1;
        if d.cabac.terminate() == 1 {
            return Ok(());
        }
    }
}

impl<'a, 'p> Dec<'a, 'p> {
    fn avail(&self, idx: usize) -> bool {
        self.slice_map[idx] == self.inp.slice_id
    }

    fn neighbours(&mut self) {
        let (w, i) = (self.mb_w, self.idx);
        self.left = self.mx > 0 && self.avail(i - 1);
        self.top = self.my > 0 && self.avail(i - w);
        self.top_left = self.mx > 0 && self.my > 0 && self.avail(i - w - 1);
        self.top_right = self.my > 0 && self.mx + 1 < w && self.avail(i - w + 1);
    }

    fn mb(&self) -> &MbInfo {
        &self.mbs[self.idx - self.mb0]
    }

    fn mb_mut(&mut self) -> &mut MbInfo {
        let i = self.idx;
        &mut self.mbs[i - self.mb0]
    }

    fn left_mb(&self) -> Option<&MbInfo> {
        self.left.then(|| &self.mbs[self.idx - 1 - self.mb0])
    }

    fn top_mb(&self) -> Option<&MbInfo> {
        self.top.then(|| &self.mbs[self.idx - self.mb_w - self.mb0])
    }

    fn is_p(&self) -> bool {
        self.inp.hdr.slice_type == SliceType::P
    }

    // ---- the macroblock layer

    fn macroblock(&mut self) -> Result<(), Error> {
        let slice_id = self.inp.slice_id;
        let qp = self.qp;
        *self.mb_mut() = MbInfo {
            slice: slice_id,
            qp: qp as i8,
            ..MbInfo::default()
        };
        if self.is_p() {
            let inc = self.left_mb().is_some_and(|m| m.kind != Kind::Skip) as usize
                + self.top_mb().is_some_and(|m| m.kind != Kind::Skip) as usize;
            if self.cabac.decision(&mut self.ctx, 11 + inc) == 1 {
                self.last_dqp_nonzero = false;
                return self.p_skip();
            }
        }
        let mb_type = if self.is_p() {
            self.p_mb_type()
        } else {
            5 + self.i_mb_type_slice_i()
        };
        if mb_type >= 5 {
            self.intra_mb(mb_type - 5)
        } else {
            self.inter_mb(mb_type)
        }
    }

    /// mb_type of a P slice: 0 to 3 for the inter types, 5 + the I type otherwise.
    fn p_mb_type(&mut self) -> u32 {
        let c = &mut self.cabac;
        if c.decision(&mut self.ctx, 14) == 0 {
            if c.decision(&mut self.ctx, 15) == 0 {
                3 * c.decision(&mut self.ctx, 16)
            } else {
                2 - c.decision(&mut self.ctx, 17)
            }
        } else if c.decision(&mut self.ctx, 17) == 0 {
            5
        } else {
            5 + self.i_type_bins(17, false)
        }
    }

    fn i_mb_type_slice_i(&mut self) -> u32 {
        let cond = |m: Option<&MbInfo>| {
            m.is_some_and(|m| !matches!(m.kind, Kind::I4x4 | Kind::I8x8)) as usize
        };
        let inc = cond(self.left_mb()) + cond(self.top_mb());
        if self.cabac.decision(&mut self.ctx, 3 + inc) == 0 {
            return 0;
        }
        self.i_type_bins(3 + 2, true)
    }

    /// The bins of an I mb_type after its first, `base` the context of the
    /// terminating bin's successor (3 + 2 in I slices, 17 in P slices).
    fn i_type_bins(&mut self, base: usize, intra_slice: bool) -> u32 {
        let c = &mut self.cabac;
        if c.terminate() == 1 {
            return 25;
        }
        let s = intra_slice as usize;
        let mut t = 1;
        t += 12 * c.decision(&mut self.ctx, base + 1);
        if c.decision(&mut self.ctx, base + 2) == 1 {
            t += 4 + 4 * c.decision(&mut self.ctx, base + 2 + s);
        }
        t += 2 * c.decision(&mut self.ctx, base + 3 + s);
        t += c.decision(&mut self.ctx, base + 3 + 2 * s);
        t
    }

    // ---- intra macroblocks

    fn intra_mb(&mut self, t: u32) -> Result<(), Error> {
        if t == 25 {
            return self.pcm();
        }
        let pps_t8 = self.inp.pps.transform_8x8;
        let mut kind = Kind::I4x4;
        let (mut i16_mode, mut cbp) = (0u8, 0u8);
        if t == 0 {
            if pps_t8 && self.transform_flag() {
                kind = Kind::I8x8;
            }
        } else {
            kind = Kind::I16x16;
            i16_mode = ((t - 1) % 4) as u8;
            cbp = ((((t - 1) / 4) % 3) as u8) << 4 | if t >= 13 { 15 } else { 0 };
        }
        {
            let m = self.mb_mut();
            m.kind = kind;
            m.transform_8x8 = kind == Kind::I8x8;
        }
        match kind {
            Kind::I4x4 => self.parse_modes(false),
            Kind::I8x8 => self.parse_modes(true),
            _ => {}
        }
        let cm = self.chroma_mode();
        self.mb_mut().chroma_mode = cm;
        if kind != Kind::I16x16 {
            cbp = self.cbp();
        }
        self.mb_mut().cbp = cbp;
        self.finish_mb(kind, i16_mode, cbp)
    }

    fn transform_flag(&mut self) -> bool {
        let inc = self.left_mb().is_some_and(|m| m.transform_8x8) as usize
            + self.top_mb().is_some_and(|m| m.transform_8x8) as usize;
        self.cabac.decision(&mut self.ctx, 399 + inc) == 1
    }

    fn chroma_mode(&mut self) -> u8 {
        let cond = |m: Option<&MbInfo>| {
            m.is_some_and(|m| m.kind.is_intra() && m.chroma_mode != 0) as usize
        };
        let inc = cond(self.left_mb()) + cond(self.top_mb());
        let c = &mut self.cabac;
        if c.decision(&mut self.ctx, 64 + inc) == 0 {
            0
        } else if c.decision(&mut self.ctx, 64 + 3) == 0 {
            1
        } else if c.decision(&mut self.ctx, 64 + 3) == 0 {
            2
        } else {
            3
        }
    }

    /// Mode of the 4x4 block at (bx, by) (units of 4x4, may be -1) of this
    /// macroblock or its neighbours, `None` when the macroblock is not available.
    fn mode_at(&self, bx: i32, by: i32) -> Option<i8> {
        let (m, cx, cy) = if bx < 0 {
            (self.left_mb()?, 3, by)
        } else if by < 0 {
            (self.top_mb()?, bx, 3)
        } else {
            (self.mb(), bx, by)
        };
        Some(if matches!(m.kind, Kind::I4x4 | Kind::I8x8) {
            m.modes[(cy * 4 + cx) as usize]
        } else {
            2
        })
    }

    fn parse_modes(&mut self, is8: bool) {
        let n = if is8 { 4 } else { 16 };
        for i in 0..n {
            let (bx, by) = if is8 {
                ((i & 1) * 2, (i >> 1) * 2)
            } else {
                blk_xy(i)
            };
            let (a, b) = (
                self.mode_at(bx as i32 - 1, by as i32),
                self.mode_at(bx as i32, by as i32 - 1),
            );
            let pred = match (a, b) {
                (Some(a), Some(b)) => a.min(b),
                _ => 2,
            };
            let mode = if self.cabac.decision(&mut self.ctx, 68) == 1 {
                pred
            } else {
                let rem = self.cabac.decision(&mut self.ctx, 69) as i8
                    | (self.cabac.decision(&mut self.ctx, 69) as i8) << 1
                    | (self.cabac.decision(&mut self.ctx, 69) as i8) << 2;
                if rem < pred {
                    rem
                } else {
                    rem + 1
                }
            };
            let m = self.mb_mut();
            if is8 {
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    m.modes[(by + dy) * 4 + bx + dx] = mode;
                }
            } else {
                m.modes[by * 4 + bx] = mode;
            }
        }
    }

    fn cbp(&mut self) -> u8 {
        let mut luma = 0u8;
        for b8 in 0..4 {
            let (bx, by) = (b8 & 1, b8 >> 1);
            // condTermFlagN: the 8x8 block's bit is 0 in an available macroblock.
            let a = if bx == 1 {
                (luma >> (b8 - 1)) & 1 == 0
            } else {
                self.left_mb().is_some_and(|m| (m.cbp >> (b8 + 1)) & 1 == 0)
            } as usize;
            let b = if by == 1 {
                (luma >> (b8 - 2)) & 1 == 0
            } else {
                self.top_mb().is_some_and(|m| (m.cbp >> (b8 + 2)) & 1 == 0)
            } as usize;
            luma |= (self.cabac.decision(&mut self.ctx, 73 + a + 2 * b) as u8) << b8;
        }
        let cond = |m: Option<&MbInfo>, two: bool| {
            m.is_some_and(|m| {
                if two {
                    m.cbp >> 4 == 2
                } else {
                    m.cbp >> 4 != 0
                }
            }) as usize
        };
        let (l, t) = (self.left_mb().copied(), self.top_mb().copied());
        let chroma = if self.cabac.decision(
            &mut self.ctx,
            77 + cond(l.as_ref(), false) + 2 * cond(t.as_ref(), false),
        ) == 0
        {
            0
        } else {
            1 + self.cabac.decision(
                &mut self.ctx,
                77 + 4 + cond(l.as_ref(), true) + 2 * cond(t.as_ref(), true),
            ) as u8
        };
        luma | chroma << 4
    }

    fn qp_delta(&mut self) -> Result<(), Error> {
        let inc = self.last_dqp_nonzero as usize;
        let c = &mut self.cabac;
        let mut k = 0i32;
        if c.decision(&mut self.ctx, 60 + inc) == 1 {
            k = 1;
            if c.decision(&mut self.ctx, 60 + 2) == 1 {
                k = 2;
                while c.decision(&mut self.ctx, 60 + 3) == 1 {
                    k += 1;
                    if k > 104 {
                        return Err(Error::Invalid("mb_qp_delta"));
                    }
                }
            }
        }
        let delta = if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) };
        self.last_dqp_nonzero = delta != 0;
        self.qp = (self.qp + delta + 52) % 52;
        self.mb_mut().qp = self.qp as i8;
        Ok(())
    }

    fn pcm(&mut self) -> Result<(), Error> {
        let start = self.cabac.next_byte();
        let data = self.cabac.data();
        let bytes = data.get(start..start + 384).ok_or(Error::Truncated)?;
        let (x, y) = (self.mx * 16, self.ly * 16);
        for r in 0..16 {
            let o = (y + r) * self.ys + x;
            self.ybuf[o..o + 16].copy_from_slice(&bytes[r * 16..r * 16 + 16]);
        }
        for k in 0..2 {
            let plane = if k == 0 {
                &mut *self.ubuf
            } else {
                &mut *self.vbuf
            };
            for r in 0..8 {
                let o = (y / 2 + r) * self.cs + x / 2;
                plane[o..o + 8]
                    .copy_from_slice(&bytes[256 + k * 64 + r * 8..256 + k * 64 + r * 8 + 8]);
            }
        }
        self.cabac.restart(start + 384);
        self.last_dqp_nonzero = false;
        let m = self.mb_mut();
        m.kind = Kind::Pcm;
        m.qp = 0;
        m.cbp = 0x2f;
        m.cbf_luma = 0xffff;
        m.cbf_cb = 0xf;
        m.cbf_cr = 0xf;
        m.cbf_dc = 7;
        Ok(())
    }

    // ---- residual

    /// One residual block (7.3.5.3.3); the levels go to `out` through `scan`.
    /// Returns the number of non-zero levels.
    fn block(
        &mut self,
        cat: usize,
        cbf_inc: Option<usize>,
        max: usize,
        scan: &[u8],
        skip: usize,
        out: &mut [i32],
    ) -> usize {
        let (cbf, sig, last, abs);
        if cat == 5 {
            (cbf, sig, last, abs) = (0, 402, 417, 426);
        } else {
            (cbf, sig, last, abs) = (
                85 + CBF_OFF[cat],
                105 + SIG_OFF[cat],
                166 + SIG_OFF[cat],
                227 + ABS_OFF[cat],
            );
        }
        if let Some(inc) = cbf_inc {
            if self.cabac.decision(&mut self.ctx, cbf + inc) == 0 {
                return 0;
            }
        }
        // Significance map: positions of the non-zero levels.
        let mut pos = [0u8; 64];
        let mut n = 0;
        let mut i = 0;
        while i < max - 1 {
            let (si, li) = match cat {
                5 => (SIG_CTX_8X8[i] as usize, LAST_CTX_8X8[i] as usize),
                3 => (i.min(2), i.min(2)),
                _ => (i, i),
            };
            if self.cabac.decision(&mut self.ctx, sig + si) == 1 {
                pos[n] = i as u8;
                n += 1;
                if self.cabac.decision(&mut self.ctx, last + li) == 1 {
                    break;
                }
            }
            i += 1;
        }
        if i == max - 1 {
            pos[n] = (max - 1) as u8;
            n += 1;
        }
        // Levels, last to first.
        let (mut eq1, mut gt1) = (0usize, 0usize);
        for k in (0..n).rev() {
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let mut level = 1;
            if self.cabac.decision(&mut self.ctx, abs + inc0) == 1 {
                let inc = 5 + gt1.min(4 - (cat == 3) as usize);
                level = 2;
                while level < 15 && self.cabac.decision(&mut self.ctx, abs + inc) == 1 {
                    level += 1;
                }
                if level == 15 {
                    // Exp-Golomb suffix of order 0.
                    let mut e = 0;
                    while self.cabac.bypass() == 1 && e < 24 {
                        level += 1 << e;
                        e += 1;
                    }
                    while e > 0 {
                        e -= 1;
                        level += (self.cabac.bypass() as i32) << e;
                    }
                }
            }
            if level == 1 {
                eq1 += 1;
            } else {
                gt1 += 1;
            }
            let v = if self.cabac.bypass() == 1 {
                -level
            } else {
                level
            };
            let p = if cat == 3 {
                pos[k] as usize
            } else {
                scan[skip + pos[k] as usize] as usize
            };
            out[p] = v;
        }
        n
    }

    /// condTermFlagN of a coded_block_flag for the neighbour macroblock
    /// `m` (None: not available), `bit` telling its flag.
    fn cond(&self, m: Option<&MbInfo>, bit: impl Fn(&MbInfo) -> bool) -> usize {
        match m {
            Some(m) => bit(m) as usize,
            None => self.mb().kind.is_intra() as usize,
        }
    }

    fn luma_cbf_inc(&self, bx: usize, by: usize) -> usize {
        let cur = self.mb().cbf_luma;
        let a = if bx > 0 {
            ((cur >> (by * 4 + bx - 1)) & 1) as usize
        } else {
            self.cond(self.left_mb(), |m| (m.cbf_luma >> (by * 4 + 3)) & 1 == 1)
        };
        let b = if by > 0 {
            ((cur >> ((by - 1) * 4 + bx)) & 1) as usize
        } else {
            self.cond(self.top_mb(), |m| (m.cbf_luma >> (12 + bx)) & 1 == 1)
        };
        a + 2 * b
    }

    fn chroma_cbf_inc(&self, comp: usize, bx: usize, by: usize) -> usize {
        let get = |m: &MbInfo| if comp == 0 { m.cbf_cb } else { m.cbf_cr };
        let cur = get(self.mb());
        let a = if bx > 0 {
            ((cur >> (by * 2)) & 1) as usize
        } else {
            self.cond(self.left_mb(), |m| (get(m) >> (by * 2 + 1)) & 1 == 1)
        };
        let b = if by > 0 {
            ((cur >> bx) & 1) as usize
        } else {
            self.cond(self.top_mb(), |m| (get(m) >> (2 + bx)) & 1 == 1)
        };
        a + 2 * b
    }

    fn dc_cbf_inc(&self, bit: u8) -> usize {
        let f = |m: &MbInfo| (m.cbf_dc >> bit) & 1 == 1;
        self.cond(self.left_mb(), f) + 2 * self.cond(self.top_mb(), f)
    }

    /// Reads all the residual of the macroblock into `self.res`.
    fn residual(&mut self, kind: Kind, cbp: u8) {
        let t8 = self.mb().transform_8x8;
        let mut res = std::mem::take(&mut self.res);
        if kind == Kind::I16x16 {
            res.luma_dc = [0; 16];
            let inc = self.dc_cbf_inc(0);
            if self.block(0, Some(inc), 16, &ZIGZAG_4X4, 0, &mut res.luma_dc) > 0 {
                self.mb_mut().cbf_dc |= 1;
            }
        }
        for b8 in 0..4 {
            if (cbp >> b8) & 1 == 0 {
                continue;
            }
            if t8 {
                res.luma8[b8] = [0; 64];
                self.block(5, None, 64, &ZIGZAG_8X8, 0, &mut res.luma8[b8]);
                // The four 4x4 blocks count as coded (the flag is inferred).
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let m = self.mb_mut();
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    m.cbf_luma |= 1 << ((by + dy) * 4 + bx + dx);
                }
                continue;
            }
            for k in 0..4 {
                let blk = b8 * 4 + k;
                let (bx, by) = blk_xy(blk);
                let r = by * 4 + bx;
                res.luma4[r] = [0; 16];
                let inc = self.luma_cbf_inc(bx, by);
                let n = if kind == Kind::I16x16 {
                    self.block(1, Some(inc), 15, &ZIGZAG_4X4, 1, &mut res.luma4[r])
                } else {
                    self.block(2, Some(inc), 16, &ZIGZAG_4X4, 0, &mut res.luma4[r])
                };
                if n > 0 {
                    self.mb_mut().cbf_luma |= 1 << r;
                }
            }
        }
        if cbp >> 4 != 0 {
            for comp in 0..2 {
                res.chroma_dc[comp] = [0; 4];
                let inc = self.dc_cbf_inc(1 + comp as u8);
                if self.block(3, Some(inc), 4, &ZIGZAG_4X4, 0, &mut res.chroma_dc[comp]) > 0 {
                    self.mb_mut().cbf_dc |= 2 << comp;
                }
            }
        }
        if cbp >> 4 == 2 {
            for comp in 0..2 {
                for blk in 0..4 {
                    let (bx, by) = (blk & 1, blk >> 1);
                    res.chroma_ac[comp][blk] = [0; 16];
                    let inc = self.chroma_cbf_inc(comp, bx, by);
                    let n = self.block(
                        4,
                        Some(inc),
                        15,
                        &ZIGZAG_4X4,
                        1,
                        &mut res.chroma_ac[comp][blk],
                    );
                    if n > 0 {
                        let m = self.mb_mut();
                        let bits = if comp == 0 {
                            &mut m.cbf_cb
                        } else {
                            &mut m.cbf_cr
                        };
                        *bits |= 1 << blk;
                    }
                }
            }
        }
        self.res = res;
    }

    /// The tail shared by intra and inter macroblocks: qp delta, residual,
    /// prediction and reconstruction.
    fn finish_mb(&mut self, kind: Kind, i16_mode: u8, cbp: u8) -> Result<(), Error> {
        if cbp != 0 || kind == Kind::I16x16 {
            self.qp_delta()?;
        } else {
            self.last_dqp_nonzero = false;
        }
        self.residual(kind, cbp);
        if kind.is_intra() {
            self.recon_intra(kind, i16_mode);
        } else {
            self.recon_inter_residual(cbp);
        }
        self.recon_chroma(cbp, kind.is_intra());
        Ok(())
    }

    // ---- reconstruction

    fn luma_avail(&self, bx: usize, by: usize) -> Avail {
        let (l, t) = (self.left, self.top);
        Avail {
            left: bx > 0 || l,
            top: by > 0 || t,
            top_left: match (bx > 0, by > 0) {
                (true, true) => true,
                (false, true) => l,
                (true, false) => t,
                (false, false) => self.top_left,
            },
            top_right: if by == 0 {
                if bx < 3 {
                    t
                } else {
                    self.top_right
                }
            } else {
                bx < 3 && blk_idx(bx + 1, by - 1) < blk_idx(bx, by)
            },
        }
    }

    fn add_4x4(&mut self, r: usize, qp: i32, bx: usize, by: usize, dc: Option<i32>) {
        let mut c = self.res.luma4[r];
        let skip = dc.is_some();
        transform::scale_4x4(&mut c, qp, skip);
        if let Some(d) = dc {
            c[0] = d;
        }
        let stride = self.ys;
        let o = (self.ly * 16 + by * 4) * stride + self.mx * 16 + bx * 4;
        transform::idct_4x4_add(&c, self.ybuf, o, stride);
    }

    fn recon_intra(&mut self, kind: Kind, i16_mode: u8) {
        let (x0, y0) = (self.mx * 16, self.ly * 16);
        let qp = self.qp;
        let stride = self.ys;
        match kind {
            Kind::I16x16 => {
                let a = Avail {
                    left: self.left,
                    top: self.top,
                    top_left: self.top_left,
                    top_right: false,
                };
                intra::predict_16x16(self.ybuf, stride, x0, y0, i16_mode, a);
                let dc = transform::luma_dc(&self.res.luma_dc, qp);
                let cbf = self.mb().cbf_luma;
                let dc_coded = self.mb().cbf_dc & 1 != 0;
                for by in 0..4 {
                    for bx in 0..4 {
                        let r = by * 4 + bx;
                        let has_ac = (cbf >> r) & 1 != 0;
                        if !has_ac && !dc_coded {
                            continue;
                        }
                        if !has_ac {
                            self.res.luma4[r] = [0; 16];
                        }
                        self.add_4x4(r, qp, bx, by, Some(dc[r]));
                    }
                }
            }
            Kind::I8x8 => {
                for b8 in 0..4 {
                    let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                    let a = self.avail_8x8(b8);
                    let mode = self.mb().modes[by * 4 + bx] as u8;
                    intra::predict_8x8(self.ybuf, stride, x0 + bx * 4, y0 + by * 4, mode, a);
                    if (self.mb().cbp >> b8) & 1 != 0 {
                        let mut c = self.res.luma8[b8];
                        transform::scale_8x8(&mut c, qp);
                        let o = (y0 + by * 4) * stride + x0 + bx * 4;
                        transform::idct_8x8_add(&c, self.ybuf, o, stride);
                    }
                }
            }
            _ => {
                for blk in 0..16 {
                    let (bx, by) = blk_xy(blk);
                    let a = self.luma_avail(bx, by);
                    let mode = self.mb().modes[by * 4 + bx] as u8;
                    intra::predict_4x4(self.ybuf, stride, x0 + bx * 4, y0 + by * 4, mode, a);
                    let r = by * 4 + bx;
                    if (self.mb().cbf_luma >> r) & 1 != 0 {
                        self.add_4x4(r, qp, bx, by, None);
                    }
                }
            }
        }
    }

    fn avail_8x8(&self, b8: usize) -> Avail {
        let (bx, by) = (b8 & 1, b8 >> 1);
        Avail {
            left: bx > 0 || self.left,
            top: by > 0 || self.top,
            top_left: match (bx > 0, by > 0) {
                (true, true) => true,
                (false, true) => self.left,
                (true, false) => self.top,
                (false, false) => self.top_left,
            },
            top_right: match b8 {
                0 => self.top,
                1 => self.top_right,
                2 => true,
                _ => false,
            },
        }
    }

    fn recon_chroma(&mut self, cbp: u8, intra: bool) {
        let (x0, y0) = (self.mx * 8, self.ly * 8);
        let mode = self.mb().chroma_mode;
        let a = Avail {
            left: self.left,
            top: self.top,
            top_left: self.top_left,
            top_right: false,
        };
        let qpy = self.mb().qp as i32;
        for comp in 0..2 {
            let qpc = chroma_qp(qpy + self.cqo[comp]);
            let stride = self.cs;
            let plane = if comp == 0 {
                &mut *self.ubuf
            } else {
                &mut *self.vbuf
            };
            if intra {
                intra::predict_chroma(plane, stride, x0, y0, mode, a);
            }
            if cbp >> 4 == 0 {
                continue;
            }
            let dc = transform::chroma_dc(&self.res.chroma_dc[comp], qpc);
            let (cbf_ac, ac_present) = (
                if comp == 0 {
                    self.mbs[self.idx - self.mb0].cbf_cb
                } else {
                    self.mbs[self.idx - self.mb0].cbf_cr
                },
                cbp >> 4 == 2,
            );
            for blk in 0..4 {
                let has_ac = ac_present && (cbf_ac >> blk) & 1 != 0;
                if !has_ac && dc[blk] == 0 {
                    continue;
                }
                let mut c = if has_ac {
                    self.res.chroma_ac[comp][blk]
                } else {
                    [0; 16]
                };
                transform::scale_4x4(&mut c, qpc, true);
                c[0] = dc[blk];
                let o = (y0 + (blk >> 1) * 4) * stride + x0 + (blk & 1) * 4;
                let plane = if comp == 0 {
                    &mut *self.ubuf
                } else {
                    &mut *self.vbuf
                };
                transform::idct_4x4_add(&c, plane, o, stride);
            }
        }
    }

    fn recon_inter_residual(&mut self, cbp: u8) {
        let qp = self.qp;
        let (x0, y0) = (self.mx * 16, self.ly * 16);
        let stride = self.ys;
        for b8 in 0..4 {
            if (cbp >> b8) & 1 == 0 {
                continue;
            }
            if self.mb().transform_8x8 {
                let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                let mut c = self.res.luma8[b8];
                transform::scale_8x8(&mut c, qp);
                let o = (y0 + by * 4) * stride + x0 + bx * 4;
                transform::idct_8x8_add(&c, self.ybuf, o, stride);
                continue;
            }
            for k in 0..4 {
                let (bx, by) = blk_xy(b8 * 4 + k);
                let r = by * 4 + bx;
                if (self.mb().cbf_luma >> r) & 1 != 0 {
                    self.add_4x4(r, qp, bx, by, None);
                }
            }
        }
    }

    // ---- inter macroblocks

    fn p_skip(&mut self) -> Result<(), Error> {
        let mv = self.skip_mv();
        let m = self.mb_mut();
        m.kind = Kind::Skip;
        m.ref_idx = [0; 4];
        m.mv = [mv; 16];
        let refs = self.inp.refs;
        let r0 = refs
            .first()
            .ok_or(Error::Invalid("P slice without a reference"))?;
        let id = r0.id;
        self.mb_mut().ref_pic = [id; 4];
        self.predict_block(0, 0, 4, 4, 0, mv)?;
        Ok(())
    }

    /// 4x4 block (bx, by) of this macroblock or a neighbour: (reference
    /// index, vector) if available; `bx`/`by` may be -1 or 4.
    fn motion_at(&self, bx: i32, by: i32, done: u16) -> Option<(i8, [i16; 2])> {
        let w = self.mb_w;
        let (m, cx, cy) = if by >= 4 {
            return None;
        } else if bx < 0 && by < 0 {
            if !self.top_left {
                return None;
            }
            (&self.mbs[self.idx - w - 1 - self.mb0], 3, 3)
        } else if bx < 0 {
            if !self.left {
                return None;
            }
            (&self.mbs[self.idx - 1 - self.mb0], 3, by)
        } else if by < 0 {
            if bx > 3 {
                if !self.top_right {
                    return None;
                }
                (&self.mbs[self.idx - w + 1 - self.mb0], 0, 3)
            } else {
                if !self.top {
                    return None;
                }
                (&self.mbs[self.idx - w - self.mb0], bx, 3)
            }
        } else if bx > 3 {
            return None;
        } else {
            if (done >> (by * 4 + bx)) & 1 == 0 {
                return None;
            }
            (self.mb(), bx, by)
        };
        let r = m.ref_idx[((cy >> 1) * 2 + (cx >> 1)) as usize];
        Some(if r < 0 {
            (-1, [0, 0])
        } else {
            (r, m.mv[(cy * 4 + cx) as usize])
        })
    }

    /// The vector predicted for a partition (8.4.1.3) at (bx, by) of `w`
    /// 4x4 units, with reference `r`; `shape` is 1 for a 16x8 and 2 for an 8x16 partition, `part` its index.
    fn predict_mv(
        &self,
        bx: i32,
        by: i32,
        w: i32,
        r: i8,
        shape: u8,
        part: usize,
        done: u16,
    ) -> [i16; 2] {
        let a = self.motion_at(bx - 1, by, done);
        let b = self.motion_at(bx, by - 1, done);
        let mut c = self.motion_at(bx + w, by - 1, done);
        if c.is_none() {
            c = self.motion_at(bx - 1, by - 1, done);
        }
        // Directional prediction.
        match (shape, part) {
            (1, 0) if b.is_some_and(|b| b.0 == r) => return b.unwrap().1,
            (1, 1) if a.is_some_and(|a| a.0 == r) => return a.unwrap().1,
            (2, 0) if a.is_some_and(|a| a.0 == r) => return a.unwrap().1,
            (2, 1) if c.is_some_and(|c| c.0 == r) => return c.unwrap().1,
            _ => {}
        }
        // Only A available: it stands for B and C.
        let (b, c) = if b.is_none() && c.is_none() && a.is_some() {
            (a, a)
        } else {
            (b, c)
        };
        let get = |n: Option<(i8, [i16; 2])>| n.unwrap_or((-1, [0, 0]));
        let (a, b, c) = (get(a), get(b), get(c));
        let same = [a.0 == r, b.0 == r, c.0 == r];
        if same.iter().filter(|&&s| s).count() == 1 {
            return if same[0] {
                a.1
            } else if same[1] {
                b.1
            } else {
                c.1
            };
        }
        let med = |x: i16, y: i16, z: i16| x.max(y).min(x.min(y).max(z));
        [med(a.1[0], b.1[0], c.1[0]), med(a.1[1], b.1[1], c.1[1])]
    }

    fn skip_mv(&self) -> [i16; 2] {
        let a = self.motion_at(-1, 0, 0);
        let b = self.motion_at(0, -1, 0);
        let zero =
            |n: Option<(i8, [i16; 2])>| n.is_none() || n.is_some_and(|n| n.0 == 0 && n.1 == [0, 0]);
        if zero(a) || zero(b) {
            return [0, 0];
        }
        self.predict_mv(0, 0, 4, 0, 0, 0, 0)
    }

    /// Stores the motion of a partition for the blocks it covers.
    fn set_motion(
        &mut self,
        bx: usize,
        by: usize,
        w: usize,
        h: usize,
        mv: [i16; 2],
        mvd: [u8; 2],
        done: &mut u16,
    ) {
        let m = self.mb_mut();
        for y in by..by + h {
            for x in bx..bx + w {
                m.mv[y * 4 + x] = mv;
                m.mvd[y * 4 + x] = mvd;
                *done |= 1 << (y * 4 + x);
            }
        }
    }

    /// Motion compensation of a partition at (bx, by) of `w` x `h` blocks (in 4x4 units).
    fn predict_block(
        &mut self,
        bx: usize,
        by: usize,
        w: usize,
        h: usize,
        ref_idx: usize,
        mv: [i16; 2],
    ) -> Result<(), Error> {
        let r = *self
            .inp
            .refs
            .get(ref_idx)
            .ok_or(Error::Invalid("reference index beyond the list"))?;
        let (x, y) = (self.mx * 16 + bx * 4, self.my * 16 + by * 4);
        let ly = self.ly * 16 + by * 4;
        let (pw, ph) = (w * 4, h * 4);
        let ys = self.ys;
        inter::luma(&r.y, x, y, pw, ph, mv, self.ybuf, ly * ys + x, ys);
        let cs = self.cs;
        let (cx, cy) = (x / 2, y / 2);
        let lcy = ly / 2;
        inter::chroma(
            &r.u,
            cx,
            cy,
            pw / 2,
            ph / 2,
            mv,
            self.ubuf,
            lcy * cs + cx,
            cs,
        );
        inter::chroma(
            &r.v,
            cx,
            cy,
            pw / 2,
            ph / 2,
            mv,
            self.vbuf,
            lcy * cs + cx,
            cs,
        );
        Ok(())
    }

    /// condTermFlagN of ref_idx: the block's 8x8 partition has a reference index above 0.
    fn ref_idx_cond(&self, bx: i32, by: i32) -> usize {
        let (m, b8) = if bx < 0 {
            if !self.left {
                return 0;
            }
            (&self.mbs[self.idx - 1 - self.mb0], (by >> 1) * 2 + 1)
        } else if by < 0 {
            if !self.top {
                return 0;
            }
            (&self.mbs[self.idx - self.mb_w - self.mb0], 2 + (bx >> 1))
        } else {
            (self.mb(), (by >> 1) * 2 + (bx >> 1))
        };
        (m.ref_idx[b8 as usize] > 0) as usize
    }

    fn read_ref_idx(&mut self, bx: usize, by: usize) -> Result<usize, Error> {
        let inc = self.ref_idx_cond(bx as i32 - 1, by as i32)
            + 2 * self.ref_idx_cond(bx as i32, by as i32 - 1);
        let c = &mut self.cabac;
        if c.decision(&mut self.ctx, 54 + inc) == 0 {
            return Ok(0);
        }
        let mut r = 1;
        if c.decision(&mut self.ctx, 54 + 4) == 1 {
            r = 2;
            while c.decision(&mut self.ctx, 54 + 5) == 1 {
                r += 1;
                if r > 32 {
                    return Err(Error::Invalid("ref_idx"));
                }
            }
        }
        Ok(r)
    }

    /// Absolute mvd component of the neighbour 4x4 block, as the context sum wants it.
    fn mvd_at(&self, bx: i32, by: i32, comp: usize, done: u16) -> usize {
        let w = self.mb_w;
        let (m, cx, cy) = if bx < 0 {
            if !self.left {
                return 0;
            }
            (&self.mbs[self.idx - 1 - self.mb0], 3, by)
        } else if by < 0 {
            if !self.top {
                return 0;
            }
            (&self.mbs[self.idx - w - self.mb0], bx, 3)
        } else {
            if (done >> (by * 4 + bx)) & 1 == 0 {
                return 0;
            }
            (self.mb(), bx, by)
        };
        m.mvd[(cy * 4 + cx) as usize][comp] as usize
    }

    fn read_mvd(&mut self, bx: usize, by: usize, comp: usize, done: u16) -> i32 {
        let sum = self.mvd_at(bx as i32 - 1, by as i32, comp, done)
            + self.mvd_at(bx as i32, by as i32 - 1, comp, done);
        let inc = if sum < 3 {
            0
        } else if sum > 32 {
            2
        } else {
            1
        };
        let base = if comp == 0 { 40 } else { 47 };
        let c = &mut self.cabac;
        if c.decision(&mut self.ctx, base + inc) == 0 {
            return 0;
        }
        let mut abs = 1;
        let mut n = 3;
        while abs < 9 && c.decision(&mut self.ctx, base + n) == 1 {
            abs += 1;
            if n < 6 {
                n += 1;
            }
        }
        if abs == 9 {
            let mut k = 3;
            while c.bypass() == 1 && k < 24 {
                abs += 1 << k;
                k += 1;
            }
            while k > 0 {
                k -= 1;
                abs += (c.bypass() as i32) << k;
            }
        }
        if c.bypass() == 1 {
            -abs
        } else {
            abs
        }
    }

    /// Reads and applies one partition's vector difference and predicts its block.
    #[allow(clippy::too_many_arguments)]
    fn partition_mv(
        &mut self,
        bx: usize,
        by: usize,
        w: usize,
        h: usize,
        r: usize,
        shape: u8,
        part: usize,
        done: &mut u16,
    ) -> Result<(), Error> {
        let pred = self.predict_mv(bx as i32, by as i32, w as i32, r as i8, shape, part, *done);
        let dx = self.read_mvd(bx, by, 0, *done);
        let dy = self.read_mvd(bx, by, 1, *done);
        let mv = [(pred[0] as i32 + dx) as i16, (pred[1] as i32 + dy) as i16];
        let mvd = [
            dx.unsigned_abs().min(255) as u8,
            dy.unsigned_abs().min(255) as u8,
        ];
        self.set_motion(bx, by, w, h, mv, mvd, done);
        self.predict_block(bx, by, w, h, r, mv)
    }

    fn inter_mb(&mut self, t: u32) -> Result<(), Error> {
        let nref = self.inp.hdr.num_ref_idx as usize;
        self.mb_mut().kind = Kind::Inter;
        let mut done = 0u16;
        let mut no_sub8 = true;
        match t {
            0..=2 => {
                // 16x16, 16x8, 8x16: (x, y, w, h) in 4x4 units.
                let parts: &[(usize, usize, usize, usize)] = match t {
                    0 => &[(0, 0, 4, 4)],
                    1 => &[(0, 0, 4, 2), (0, 2, 4, 2)],
                    _ => &[(0, 0, 2, 4), (2, 0, 2, 4)],
                };
                let mut refs = [0usize; 2];
                for (i, &(bx, by, w, h)) in parts.iter().enumerate() {
                    refs[i] = if nref > 1 {
                        self.read_ref_idx(bx, by)?
                    } else {
                        0
                    };
                    let rid = self.inp.refs.get(refs[i]).map(|p| p.id).unwrap_or(u32::MAX);
                    let m = self.mb_mut();
                    for y in (by / 2)..((by + h) / 2) {
                        for x in (bx / 2)..((bx + w) / 2) {
                            m.ref_idx[y * 2 + x] = refs[i] as i8;
                            m.ref_pic[y * 2 + x] = rid;
                        }
                    }
                }
                for (i, &(bx, by, w, h)) in parts.iter().enumerate() {
                    let shape = if t == 0 { 0 } else { t as u8 };
                    self.partition_mv(bx, by, w, h, refs[i], shape, i, &mut done)?;
                }
            }
            _ => {
                let mut sub = [0u32; 4];
                for s in sub.iter_mut() {
                    *s = self.sub_mb_type();
                    no_sub8 &= *s == 0;
                }
                let mut refs = [0usize; 4];
                for b8 in 0..4 {
                    let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                    refs[b8] = if nref > 1 {
                        self.read_ref_idx(bx, by)?
                    } else {
                        0
                    };
                    let rid = self
                        .inp
                        .refs
                        .get(refs[b8])
                        .map(|p| p.id)
                        .unwrap_or(u32::MAX);
                    let m = self.mb_mut();
                    m.ref_idx[b8] = refs[b8] as i8;
                    m.ref_pic[b8] = rid;
                }
                for b8 in 0..4 {
                    let (bx, by) = ((b8 & 1) * 2, (b8 >> 1) * 2);
                    let parts: &[(usize, usize, usize, usize)] = match sub[b8] {
                        0 => &[(0, 0, 2, 2)],
                        1 => &[(0, 0, 2, 1), (0, 1, 2, 1)],
                        2 => &[(0, 0, 1, 2), (1, 0, 1, 2)],
                        _ => &[(0, 0, 1, 1), (1, 0, 1, 1), (0, 1, 1, 1), (1, 1, 1, 1)],
                    };
                    for &(dx, dy, w, h) in parts {
                        self.partition_mv(bx + dx, by + dy, w, h, refs[b8], 0, 0, &mut done)?;
                    }
                }
            }
        }
        let cbp = self.cbp();
        self.mb_mut().cbp = cbp;
        if cbp & 15 != 0 && self.inp.pps.transform_8x8 && no_sub8 && self.transform_flag() {
            self.mb_mut().transform_8x8 = true;
        }
        self.finish_mb(Kind::Inter, 0, cbp)
    }

    fn sub_mb_type(&mut self) -> u32 {
        let c = &mut self.cabac;
        if c.decision(&mut self.ctx, 21) == 1 {
            0
        } else if c.decision(&mut self.ctx, 22) == 0 {
            1
        } else if c.decision(&mut self.ctx, 23) == 1 {
            2
        } else {
            3
        }
    }
}

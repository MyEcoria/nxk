//! Execution of the SIMD instructions: xmm registers, and the upper ymm halves for AVX1.
//!
//! Legacy SSE writes keep a register's upper ymm half; VEX.128 writes clear it; VEX.256
//! (moves, logic, vinsertf128) writes both halves.

use super::{flag, mask, sext, Cpu, Mem, Stop, CF, FLAG_MASK, ZF};
use crate::decode::{CvtOp, Inst, LhOp, Operand, SseOp, Ty, VEX, VEX_L};

/// Apply `f` to each `e`-byte lane of `a` and `b`.
fn lanes(a: u128, b: u128, e: u8, f: impl Fn(u64, u64) -> u64) -> u128 {
    let (bits, m) = (8 * e as u32, mask(e));
    let mut r = 0u128;
    for i in 0..(16 / e as u32) {
        let sh = i * bits;
        let (x, y) = ((a >> sh) as u64 & m, (b >> sh) as u64 & m);
        r |= ((f(x, y) & m) as u128) << sh;
    }
    r
}

/// Interleave the `e`-byte lanes of the low (or high) halves of `a` and `b`.
fn unpack(a: u128, b: u128, e: u8, high: bool) -> u128 {
    let (bits, m) = (8 * e as u32, mask(e) as u128);
    let (a, b) = if high { (a >> 64, b >> 64) } else { (a, b) };
    let mut r = 0u128;
    for i in 0..(8 / e as u32) {
        r |= ((a >> (i * bits)) & m) << (2 * i * bits);
        r |= ((b >> (i * bits)) & m) << ((2 * i + 1) * bits);
    }
    r
}

/// Clamp a signed value to an `e`-byte signed element.
fn sat_s(v: i64, e: u8) -> u64 {
    let max = (1i64 << (8 * e as u32 - 1)) - 1;
    v.clamp(-max - 1, max) as u64
}

/// Clamp a signed value to an `e`-byte unsigned element.
fn sat_u(v: i64, e: u8) -> u64 {
    v.clamp(0, mask(e) as i64) as u64
}

/// Narrow the `e`-byte elements of `a` (low half) then `b` (high half) with `sat`.
fn pack(a: u128, b: u128, e: u8, sat: fn(i64, u8) -> u64) -> u128 {
    let (n, out) = (16 / e as u32, e / 2);
    let mut r = 0u128;
    for (k, src) in [a, b].into_iter().enumerate() {
        for i in 0..n {
            let x = sext((src >> (i * 8 * e as u32)) as u64 & mask(e), e);
            let v = sat(x, out) & mask(out);
            r |= (v as u128) << ((k as u32 * n + i) * 8 * out as u32);
        }
    }
    r
}

/// Shuffle the four 16-bit words of `q` (a quadword) by `imm`.
fn shuf_words(q: u64, imm: i64) -> u64 {
    (0..4).fold(0, |r, k| {
        let sel = (imm >> (2 * k)) & 3;
        r | ((q >> (16 * sel)) & 0xffff) << (16 * k)
    })
}

/// pmovsx / pmovzx of the low elements of `v`.
fn extend(v: u128, from: u8, to: u8, signed: bool) -> u128 {
    let mut r = 0u128;
    for k in 0..(16 / to as u32) {
        let x = (v >> (k * 8 * from as u32)) as u64 & mask(from);
        let x = if signed { sext(x, from) as u64 } else { x };
        r |= ((x & mask(to)) as u128) << (k * 8 * to as u32);
    }
    r
}

impl Cpu {
    /// Bytes `[a, a + n)` of guest memory as a little-endian value (n <= 16).
    pub(super) fn read_n(&self, a: u64, n: u8, mem: &mut dyn Mem) -> Result<u128, Stop> {
        let mut v = 0u128;
        let mut off = 0u8;
        while off < n {
            let step = (n - off).min(8);
            let at = a.wrapping_add(off as u64);
            let part = mem.read(at, step).ok_or(Stop::Fault(at))?;
            v |= (part as u128) << (8 * off as u32);
            off += step;
        }
        Ok(v)
    }

    /// The low `n` bytes of an xmm register or memory operand.
    pub(super) fn get_n(
        &self,
        o: Operand,
        n: u8,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<u128, Stop> {
        match o {
            Operand::Xmm(x) => Ok(self.xmm[x as usize]),
            Operand::Mem(m) => self.read_n(self.addr(&m, next), n, mem),
            _ => unreachable!("decoder: xmm operand is a register or memory"),
        }
    }

    /// Write an xmm result; a VEX.128 write clears the register's upper ymm half.
    pub(super) fn put_v(
        &mut self,
        i: &Inst,
        v: u128,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        self.put_x(i.dst, v, mem, next)?;
        if let Operand::Xmm(x) = i.dst {
            if i.vex & VEX != 0 {
                self.ymm_hi[x as usize] = 0;
            }
        }
        Ok(())
    }

    /// The first source of a two-source op: vvvv under VEX, else the destination.
    fn first(&self, i: &Inst) -> u128 {
        if i.vex & VEX != 0 {
            return self.xmm[i.vvvv as usize];
        }
        match i.dst {
            Operand::Xmm(x) => self.xmm[x as usize],
            _ => unreachable!("decoder: destination of this op is a register"),
        }
    }

    /// #GP on a misaligned access that must be aligned: every legacy 128-bit memory
    /// operand but the unaligned and narrower forms; under VEX only the aligned moves.
    fn check_align(&self, op: SseOp, i: &Inst, next: u64) -> Result<(), Stop> {
        let vex = i.vex & VEX != 0;
        let need = match op {
            SseOp::Mov => i.size as u64,
            _ if vex => return Ok(()),
            SseOp::MovU
            | SseOp::MovQ
            | SseOp::MovGpr
            | SseOp::MovDup
            | SseOp::Movx(..)
            | SseOp::Insert(_)
            | SseOp::Extract(_)
            | SseOp::Fp(_, Ty::Ss | Ty::Sd)
            | SseOp::Comis(..)
            | SseOp::Insertps
            | SseOp::MovS(_)
            | SseOp::Movmsk(_)
            | SseOp::MovLH(
                LhOp::LoadLo
                | LhOp::LoadHi
                | LhOp::StoreLo
                | LhOp::StoreHi
                | LhOp::HiToLo
                | LhOp::LoToHi,
            )
            | SseOp::Cvt(
                CvtOp::Ss2Sd
                | CvtOp::Sd2Ss
                | CvtOp::Si2Ss
                | CvtOp::Si2Sd
                | CvtOp::Ss2Si(_)
                | CvtOp::Sd2Si(_)
                | CvtOp::Ps2Pd
                | CvtOp::Dq2Pd,
            ) => return Ok(()),
            _ => 16,
        };
        for o in [i.dst, i.src] {
            if let Operand::Mem(m) = o {
                let a = self.addr(&m, next);
                if !a.is_multiple_of(need) {
                    return Err(Stop::Misaligned(a));
                }
            }
        }
        Ok(())
    }

    // Rare paths stay out of the hot dispatch loop (smaller code on the A57).
    #[inline(never)]
    pub(super) fn sse(
        &mut self,
        op: SseOp,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        match op {
            SseOp::ZeroUpper => {
                self.ymm_hi = [0; 16];
                return Ok(());
            }
            SseOp::ZeroAll => {
                self.xmm = [0; 16];
                self.ymm_hi = [0; 16];
                return Ok(());
            }
            SseOp::MovGpr | SseOp::MovQ => {
                // Into xmm: zero-extended; out of xmm: a GPR or memory write of `size`.
                let size = if op == SseOp::MovQ { 8 } else { i.size };
                let v = match i.src {
                    Operand::Xmm(x) => self.xmm[x as usize] as u64 & mask(size),
                    s => self.get(s, size, mem, next)?,
                };
                return match i.dst {
                    Operand::Xmm(_) => self.put_v(&i, v as u128, mem, next),
                    d => self.put(d, size, v, mem, next),
                };
            }
            SseOp::Extract(e) => {
                let Operand::Xmm(x) = i.src else {
                    unreachable!()
                };
                let lane = (i.imm as u32) & (16 / e as u32 - 1);
                let v = (self.xmm[x as usize] >> (lane * 8 * e as u32)) as u64 & mask(e);
                // To a GPR: zero-extended to 32 bits (64 for pextrq); to memory: the element.
                let size = match i.dst {
                    Operand::Reg(_) => e.max(4),
                    _ => e,
                };
                return self.put(i.dst, size, v, mem, next);
            }
            SseOp::Movmskb => {
                let Operand::Xmm(x) = i.src else {
                    unreachable!()
                };
                let v = self.xmm[x as usize];
                let m = (0..16).fold(0u64, |r, k| r | (((v >> (8 * k + 7)) & 1) as u64) << k);
                return self.put(i.dst, 4, m, mem, next);
            }
            SseOp::Ptest => {
                self.check_align(op, &i, next)?;
                let (a, b) = (self.first_dst(&i), self.get_n(i.src, 16, mem, next)?);
                let f = flag(a & b == 0, ZF) | flag(!a & b == 0, CF);
                self.flags = (self.flags & !FLAG_MASK) | f;
                return Ok(());
            }
            SseOp::Insert128 | SseOp::Extract128 => return self.half128(op, i, mem, next),
            _ => {}
        }
        self.check_align(op, &i, next)?;
        match op {
            SseOp::Fp(f, t) => return self.fp(f, t, i, mem, next),
            SseOp::Cvt(c) => return self.cvt(c, i, mem, next),
            SseOp::Comis(t, s) => return self.comis(t, s, i, mem, next),
            SseOp::MovS(t) => return self.movs(t, i, mem, next),
            SseOp::MovLH(l) => return self.movlh(l, i, mem, next),
            SseOp::Shufp(t) => return self.shufp(t, i, mem, next),
            SseOp::Movmsk(t) => return self.movmsk(t, i, mem, next),
            SseOp::Blend(_)
            | SseOp::Blendv(_)
            | SseOp::Insertps
            | SseOp::Dp(_)
            | SseOp::Broadcast(_)
            | SseOp::Perm2f128
            | SseOp::PermilV(_)
            | SseOp::PermilI(_)
            | SseOp::Cvtph2ps
            | SseOp::Cvtps2ph => return self.shuffle(op, i, mem, next),
            _ => {}
        }
        if i.vex & VEX_L != 0 {
            return self.sse256(op, i, mem, next);
        }
        let r = match op {
            SseOp::Mov | SseOp::MovU => self.get_n(i.src, 16, mem, next)?,
            SseOp::MovDup => {
                let lo = self.get_n(i.src, 8, mem, next)? as u64 as u128;
                lo | lo << 64
            }
            SseOp::Movx(from, to, s) => {
                let v = self.get_n(i.src, 16 / to * from, mem, next)?;
                extend(v, from, to, s)
            }
            SseOp::Abs(e) => {
                let b = self.get_n(i.src, 16, mem, next)?;
                lanes(b, 0, e, |x, _| sext(x, e).unsigned_abs())
            }
            SseOp::SllI(e) | SseOp::SrlI(e) | SseOp::SraI(e) => {
                let b = self.get_n(i.src, 16, mem, next)?;
                shift_lanes(op, b, i.imm as u64, e)
            }
            SseOp::Slldq | SseOp::Srldq => {
                let (b, n) = (self.get_n(i.src, 16, mem, next)?, i.imm as u32);
                match (op, n) {
                    (_, 16..) => 0,
                    (SseOp::Slldq, n) => b << (8 * n),
                    (_, n) => b >> (8 * n),
                }
            }
            SseOp::Shufd => {
                let b = self.get_n(i.src, 16, mem, next)?;
                (0..4).fold(0u128, |r, k| {
                    let sel = (i.imm >> (2 * k)) & 3;
                    r | ((b >> (32 * sel)) & 0xffff_ffff) << (32 * k)
                })
            }
            SseOp::Shuflw | SseOp::Shufhw => {
                let b = self.get_n(i.src, 16, mem, next)?;
                let (lo, hi) = (b as u64, (b >> 64) as u64);
                if op == SseOp::Shuflw {
                    shuf_words(lo, i.imm) as u128 | (hi as u128) << 64
                } else {
                    lo as u128 | (shuf_words(hi, i.imm) as u128) << 64
                }
            }
            SseOp::Insert(e) => {
                let a = self.first(&i);
                let v = self.get(i.src, e, mem, next)? as u128;
                let sh = ((i.imm as u32) & (16 / e as u32 - 1)) * 8 * e as u32;
                (a & !((mask(e) as u128) << sh)) | v << sh
            }
            _ => {
                let a = self.first(&i);
                let b = self.get_n(i.src, 16, mem, next)?;
                two_source(op, a, b, i.imm)
            }
        };
        self.put_v(&i, r, mem, next)
    }

    /// ptest's first operand: the destination register (it has no vvvv form).
    fn first_dst(&self, i: &Inst) -> u128 {
        match i.dst {
            Operand::Xmm(x) => self.xmm[x as usize],
            _ => unreachable!("decoder: ptest destination is a register"),
        }
    }

    /// 32 bytes of a ymm register or memory operand.
    pub(super) fn get_y(
        &self,
        o: Operand,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(u128, u128), Stop> {
        match o {
            Operand::Xmm(x) => Ok((self.xmm[x as usize], self.ymm_hi[x as usize])),
            Operand::Mem(m) => {
                let a = self.addr(&m, next);
                Ok((self.read_n(a, 16, mem)?, self.read_n(a + 16, 16, mem)?))
            }
            _ => unreachable!("decoder: ymm operand is a register or memory"),
        }
    }

    pub(super) fn put_y(
        &mut self,
        o: Operand,
        v: (u128, u128),
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        match o {
            Operand::Xmm(x) => {
                self.xmm[x as usize] = v.0;
                self.ymm_hi[x as usize] = v.1;
                Ok(())
            }
            Operand::Mem(m) => {
                let hi = crate::decode::MemRef {
                    disp: m.disp.wrapping_add(16),
                    ..m
                };
                self.put_x(o, v.0, mem, next)?;
                self.put_x(Operand::Mem(hi), v.1, mem, next)
            }
            _ => unreachable!("decoder: ymm operand is a register or memory"),
        }
    }

    /// VEX.256 moves and logic (the AVX1 subset; the decoder refuses the rest).
    fn sse256(&mut self, op: SseOp, i: Inst, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        let b = self.get_y(i.src, mem, next)?;
        let r = match op {
            SseOp::Mov | SseOp::MovU => b,
            _ => {
                let v = i.vvvv as usize;
                let a = (self.xmm[v], self.ymm_hi[v]);
                (
                    two_source(op, a.0, b.0, i.imm),
                    two_source(op, a.1, b.1, i.imm),
                )
            }
        };
        self.put_y(i.dst, r, mem, next)
    }

    /// vinsertf128 / vextractf128.
    fn half128(&mut self, op: SseOp, i: Inst, mem: &mut dyn Mem, next: u64) -> Result<(), Stop> {
        let high = i.imm & 1 != 0;
        if op == SseOp::Insert128 {
            let v = i.vvvv as usize;
            let (mut lo, mut hi) = (self.xmm[v], self.ymm_hi[v]);
            let x = self.get_n(i.src, 16, mem, next)?;
            if high {
                hi = x;
            } else {
                lo = x;
            }
            return self.put_y(i.dst, (lo, hi), mem, next);
        }
        let Operand::Xmm(s) = i.src else {
            unreachable!()
        };
        let x = if high {
            self.ymm_hi[s as usize]
        } else {
            self.xmm[s as usize]
        };
        self.put_v(&i, x, mem, next)
    }
}

/// Lane shifts by `n` bits (count from a register or an immediate).
fn shift_lanes(op: SseOp, a: u128, n: u64, e: u8) -> u128 {
    let bits = 8 * e as u64;
    lanes(a, 0, e, |x, _| match op {
        SseOp::Sll(_) | SseOp::SllI(_) if n < bits => x << n,
        SseOp::Srl(_) | SseOp::SrlI(_) if n < bits => x >> n,
        SseOp::Sra(_) | SseOp::SraI(_) => (sext(x, e) >> n.min(bits - 1)) as u64,
        _ => 0,
    })
}

/// Ops of the form `r = f(a, b)` on whole 128-bit values.
fn two_source(op: SseOp, a: u128, b: u128, imm: i64) -> u128 {
    use SseOp::*;
    let ones = |c: bool| if c { u64::MAX } else { 0 };
    match op {
        Xor => a ^ b,
        And => a & b,
        Andn => !a & b,
        Or => a | b,
        Add(e) => lanes(a, b, e, u64::wrapping_add),
        Sub(e) => lanes(a, b, e, u64::wrapping_sub),
        AddS(e) => lanes(a, b, e, |x, y| sat_s(sext(x, e) + sext(y, e), e)),
        SubS(e) => lanes(a, b, e, |x, y| sat_s(sext(x, e) - sext(y, e), e)),
        AddUS(e) => lanes(a, b, e, |x, y| (x + y).min(mask(e))),
        SubUS(e) => lanes(a, b, e, u64::saturating_sub),
        CmpEq(e) => lanes(a, b, e, |x, y| ones(x == y)),
        CmpGt(e) => lanes(a, b, e, |x, y| ones(sext(x, e) > sext(y, e))),
        Min(e, s) | Max(e, s) => lanes(a, b, e, |x, y| {
            let less = if s { sext(x, e) < sext(y, e) } else { x < y };
            if less == matches!(op, Min(..)) {
                x
            } else {
                y
            }
        }),
        MulLo(e) => lanes(a, b, e, u64::wrapping_mul),
        MulHi(s) => lanes(a, b, 2, |x, y| {
            if s {
                ((sext(x, 2) * sext(y, 2)) >> 16) as u64
            } else {
                (x * y) >> 16
            }
        }),
        MulUdq => lanes(a, b, 8, |x, y| (x & 0xffff_ffff) * (y & 0xffff_ffff)),
        MulDq => lanes(a, b, 8, |x, y| (sext(x, 4) * sext(y, 4)) as u64),
        Sll(e) | Srl(e) | Sra(e) => shift_lanes(op, a, b as u64, e),
        UnpackLo(e) => unpack(a, b, e, false),
        UnpackHi(e) => unpack(a, b, e, true),
        PackSS(e) => pack(a, b, e, sat_s),
        PackUS(e) => pack(a, b, e, sat_u),
        Shufb => (0..16).fold(0u128, |r, k| {
            let c = (b >> (8 * k)) as u8;
            let v = if c & 0x80 != 0 {
                0
            } else {
                (a >> (8 * (c & 15) as u32)) & 0xff
            };
            r | v << (8 * k)
        }),
        Alignr => {
            let n = (imm & 0xff) as u32;
            if n >= 32 {
                0
            } else if n >= 16 {
                a >> (8 * (n - 16))
            } else if n == 0 {
                b
            } else {
                (b >> (8 * n)) | (a << (8 * (16 - n)))
            }
        }
        _ => unreachable!("not a two-source op: {op:?}"),
    }
}

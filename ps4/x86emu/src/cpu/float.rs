//! Execution of the SSE / AVX floating-point instructions on the soft-float core.
//!
//! Every lane gets its own `Env` (one operation's flags), then the flags of all lanes
//! are or-ed into MXCSR; an unmasked one stops the CPU before the result is written.

use super::{mask, Cpu, Mem, Stop, CF, FLAG_MASK, PF, ZF};
use crate::decode::{CvtOp, FpOp, Inst, LhOp, Operand, Ty, VEX, VEX_L};
use crate::fpu::{self, Cmp, Env, Fmt, Round, F32, F64};

/// MXCSR at reset: every exception masked, round to nearest.
pub const MXCSR_RESET: u32 = 0x1f80;
/// Bits a guest may set (an Intel host's MXCSR_MASK; AMD's MM bit 17 is not modelled).
pub const MXCSR_MASK: u32 = 0xffff;

fn fmt(t: Ty) -> Fmt {
    if t.is_double() {
        F64
    } else {
        F32
    }
}

pub(super) fn lane(v: u128, w: u8, i: u32) -> u64 {
    (v >> (i * 8 * w as u32)) as u64 & mask(w)
}

pub(super) fn set_lane(v: u128, w: u8, i: u32, x: u64) -> u128 {
    let sh = i * 8 * w as u32;
    (v & !((mask(w) as u128) << sh)) | ((x & mask(w)) as u128) << sh
}

/// Base predicates of cmpps (imm & 15): result for Less, Equal, Greater, Unordered.
const PREDICATES: [[bool; 4]; 16] = [
    [false, true, false, false],  // EQ
    [true, false, false, false],  // LT
    [true, true, false, false],   // LE
    [false, false, false, true],  // UNORD
    [true, false, true, true],    // NEQ
    [false, true, true, true],    // NLT
    [false, false, true, true],   // NLE
    [true, true, true, false],    // ORD
    [false, true, false, true],   // EQ_U
    [true, false, false, true],   // NGE
    [true, true, false, true],    // NGT
    [false, false, false, false], // FALSE
    [true, false, true, false],   // NEQ_O
    [false, true, true, false],   // GE
    [false, false, true, false],  // GT
    [true, true, true, true],     // TRUE
];

/// cmpps predicate `p` (0..32) on `a`, `b`: an all-ones or zero element.
fn compare(f: Fmt, a: u64, b: u64, p: u8, env: &mut Env) -> u64 {
    let base = (p & 15) as usize;
    // The ordered/unordered "S" forms of 0..16 signal on QNaN; bit 4 swaps S and Q.
    let signaling = matches!(base, 1 | 2 | 5 | 6 | 9 | 10 | 13 | 14) != (p & 16 != 0);
    let c = fpu::compare(f, a, b, signaling, env);
    let i = match c {
        Cmp::Less => 0,
        Cmp::Equal => 1,
        Cmp::Greater => 2,
        Cmp::Unordered => 3,
    };
    if PREDICATES[base][i] {
        u64::MAX
    } else {
        0
    }
}

/// rcpps / rsqrtps: x86 leaves the exact bits implementation-defined (relative error at
/// most 1.5 * 2^-12; Jaguar's tables are not Intel's). The exact value rounded to 12
/// fraction bits, raising nothing; denormal inputs read as zero, tiny results flush.
fn approx(a: u64, sqrt: bool) -> u64 {
    let f = F32;
    let mut env = Env::from_mxcsr(MXCSR_RESET | (1 << 15) | (1 << 6));
    if f.is_nan(a) {
        return a | (1 << 22);
    }
    let neg = a & f.sign_bit() != 0;
    let mag = a & !f.sign_bit();
    if mag < 1 << 23 {
        return a & f.sign_bit() | 0x7f80_0000; // ±0 and denormals: ±inf
    }
    if sqrt && neg {
        return f.indefinite();
    }
    if mag == 0x7f80_0000 {
        return a & f.sign_bit(); // ±inf: ±0
    }
    let x = if sqrt { fpu::sqrt(f, a, &mut env) } else { a };
    let r = fpu::div(f, 0x3f80_0000, x, &mut env);
    // Keep 12 fraction bits, rounded to nearest.
    (r + (1 << 10)) & !((1 << 11) - 1)
}

/// One element of a float op.
fn element(op: FpOp, f: Fmt, a: u64, b: u64, imm: i64, env: &mut Env) -> u64 {
    match op {
        FpOp::Add => fpu::add(f, a, b, false, env),
        FpOp::Sub => fpu::add(f, a, b, true, env),
        FpOp::Mul => fpu::mul(f, a, b, env),
        FpOp::Div => fpu::div(f, a, b, env),
        FpOp::Min => fpu::min_max(f, a, b, false, env),
        FpOp::Max => fpu::min_max(f, a, b, true, env),
        FpOp::Sqrt => fpu::sqrt(f, b, env),
        FpOp::Rcp => approx(b, false),
        FpOp::Rsqrt => approx(b, true),
        FpOp::Cmp => compare(f, a, b, imm as u8, env),
        FpOp::Round => {
            let mode = if imm & 4 != 0 {
                env.round
            } else {
                [Round::Nearest, Round::Down, Round::Up, Round::Zero][(imm & 3) as usize]
            };
            fpu::round_int(f, b, mode, imm & 8 != 0, env)
        }
        FpOp::HAdd | FpOp::HSub | FpOp::AddSub => unreachable!("handled by lanes128"),
    }
}

/// A float op on one 128-bit lane: `a` is the first source, `b` the second.
fn lanes128(op: FpOp, t: Ty, a: u128, b: u128, imm: i64, mx: u32, flags: &mut u32) -> u128 {
    let (f, w) = (fmt(t), t.width());
    let n = if t.is_scalar() { 1 } else { 16 / w as u32 };
    let mut run = |x: u64, y: u64, op: FpOp| {
        let mut env = Env::from_mxcsr(mx);
        let r = element(op, f, x, y, imm, &mut env);
        *flags |= env.flags;
        r
    };
    let mut r = a;
    match op {
        FpOp::HAdd | FpOp::HSub => {
            let e = if op == FpOp::HAdd {
                FpOp::Add
            } else {
                FpOp::Sub
            };
            let half = n / 2;
            for k in 0..half {
                let v = run(lane(a, w, 2 * k), lane(a, w, 2 * k + 1), e);
                r = set_lane(r, w, k, v);
            }
            for k in 0..half {
                let v = run(lane(b, w, 2 * k), lane(b, w, 2 * k + 1), e);
                r = set_lane(r, w, half + k, v);
            }
        }
        FpOp::AddSub => {
            for k in 0..n {
                let e = if k % 2 == 0 { FpOp::Sub } else { FpOp::Add };
                let v = run(lane(a, w, k), lane(b, w, k), e);
                r = set_lane(r, w, k, v);
            }
        }
        _ => {
            for k in 0..n {
                let v = run(lane(a, w, k), lane(b, w, k), op);
                r = set_lane(r, w, k, v);
            }
        }
    }
    r
}

impl Cpu {
    /// Or `flags` into MXCSR; an unmasked exception stops at `here`.
    pub(super) fn raise(&mut self, flags: u32, here: u64) -> Result<(), Stop> {
        self.mxcsr |= flags;
        let masked = (self.mxcsr >> 7) & 0x3f;
        if flags & !masked != 0 {
            return Err(Stop::FpException(here));
        }
        Ok(())
    }

    /// Both 128-bit halves of the first source: vvvv under VEX, else the destination.
    pub(super) fn first_y(&self, i: &Inst) -> (u128, u128) {
        let x = if i.vex & VEX != 0 {
            i.vvvv as usize
        } else {
            match i.dst {
                Operand::Xmm(x) => x as usize,
                _ => return (0, 0),
            }
        };
        (self.xmm[x], self.ymm_hi[x])
    }

    /// The second source: `n` bytes of memory, or the register (both halves when wide).
    pub(super) fn second(
        &self,
        i: &Inst,
        n: u8,
        wide: bool,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(u128, u128), Stop> {
        if wide {
            self.get_y(i.src, mem, next)
        } else {
            Ok((self.get_n(i.src, n, mem, next)?, 0))
        }
    }

    /// Write a float result: both halves when wide, else as `put_v`.
    pub(super) fn put_fp(
        &mut self,
        i: &Inst,
        v: (u128, u128),
        wide: bool,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        if wide {
            self.put_y(i.dst, v, mem, next)
        } else {
            self.put_v(i, v.0, mem, next)
        }
    }

    pub(super) fn fp(
        &mut self,
        op: FpOp,
        t: Ty,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let here = next.wrapping_sub(i.len as u64);
        let wide = i.vex & VEX_L != 0;
        let a = self.first_y(&i);
        let n = if t.is_scalar() { t.width() } else { 16 };
        let b = self.second(&i, n, wide, mem, next)?;
        let mut flags = 0;
        let lo = lanes128(op, t, a.0, b.0, i.imm, self.mxcsr, &mut flags);
        let hi = if wide {
            lanes128(op, t, a.1, b.1, i.imm, self.mxcsr, &mut flags)
        } else {
            0
        };
        self.raise(flags, here)?;
        self.put_fp(&i, (lo, hi), wide, mem, next)
    }

    pub(super) fn comis(
        &mut self,
        t: Ty,
        signaling: bool,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let here = next.wrapping_sub(i.len as u64);
        let Operand::Xmm(d) = i.dst else {
            unreachable!()
        };
        let w = t.width();
        let a = self.xmm[d as usize] as u64 & mask(w);
        let b = self.get_n(i.src, w, mem, next)? as u64 & mask(w);
        let mut env = Env::from_mxcsr(self.mxcsr);
        let c = fpu::compare(fmt(t), a, b, signaling, &mut env);
        self.raise(env.flags, here)?;
        let f = match c {
            Cmp::Unordered => ZF | PF | CF,
            Cmp::Less => CF,
            Cmp::Equal => ZF,
            Cmp::Greater => 0,
        };
        self.flags = (self.flags & !FLAG_MASK) | f;
        Ok(())
    }

    pub(super) fn cvt(
        &mut self,
        op: CvtOp,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let here = next.wrapping_sub(i.len as u64);
        let wide = i.vex & VEX_L != 0;
        let mx = self.mxcsr;
        let mut flags = 0;
        let mut each = |x: u64, f: &dyn Fn(u64, &mut Env) -> u64| {
            let mut env = Env::from_mxcsr(mx);
            let r = f(x, &mut env);
            flags |= env.flags;
            r
        };
        // Destination as (lo, hi), plus whether it is a GPR result.
        let result: (u128, u128) = match op {
            CvtOp::Ss2Sd | CvtOp::Sd2Ss | CvtOp::Si2Ss | CvtOp::Si2Sd => {
                let a = self.first_y(&i).0;
                let v = match op {
                    CvtOp::Ss2Sd => {
                        let x = self.get_n(i.src, 4, mem, next)? as u64 & mask(4);
                        set_lane(a, 8, 0, each(x, &|x, e| fpu::convert(F32, F64, x, e)))
                    }
                    CvtOp::Sd2Ss => {
                        let x = self.get_n(i.src, 8, mem, next)? as u64;
                        set_lane(a, 4, 0, each(x, &|x, e| fpu::convert(F64, F32, x, e)))
                    }
                    _ => {
                        let raw = self.get(i.src, i.size, mem, next)?;
                        let v = if i.size == 8 {
                            raw as i64
                        } else {
                            raw as u32 as i32 as i64
                        };
                        if op == CvtOp::Si2Ss {
                            set_lane(
                                a,
                                4,
                                0,
                                each(v as u64, &|x, e| fpu::from_int(F32, x as i64, e)),
                            )
                        } else {
                            set_lane(
                                a,
                                8,
                                0,
                                each(v as u64, &|x, e| fpu::from_int(F64, x as i64, e)),
                            )
                        }
                    }
                };
                (v, 0)
            }
            CvtOp::Ss2Si(trunc) | CvtOp::Sd2Si(trunc) => {
                let (f, w) = if matches!(op, CvtOp::Ss2Si(_)) {
                    (F32, 4)
                } else {
                    (F64, 8)
                };
                let x = self.get_n(i.src, w, mem, next)? as u64 & mask(w);
                let bits = 8 * i.size as u32;
                let v = each(x, &|x, e| fpu::to_int(f, x, bits, trunc, e));
                self.raise(flags, here)?;
                return self.put(i.dst, i.size, v, mem, next);
            }
            CvtOp::Dq2Ps | CvtOp::Ps2Dq(_) => {
                let b = self.second(&i, 16, wide, mem, next)?;
                let mut halves = [b.0, b.1];
                for h in halves.iter_mut().take(if wide { 2 } else { 1 }) {
                    for k in 0..4 {
                        let x = lane(*h, 4, k);
                        let y = match op {
                            CvtOp::Dq2Ps => {
                                each(x, &|x, e| fpu::from_int(F32, x as u32 as i32 as i64, e))
                            }
                            CvtOp::Ps2Dq(t) => each(x, &|x, e| fpu::to_int(F32, x, 32, t, e)),
                            _ => unreachable!(),
                        };
                        *h = set_lane(*h, 4, k, y);
                    }
                }
                (halves[0], if wide { halves[1] } else { 0 })
            }
            CvtOp::Ps2Pd | CvtOp::Dq2Pd => {
                // Widening: an xmm (or m64 / m128) source fills an xmm (ymm when wide).
                let n = if wide { 16 } else { 8 };
                let b = self.get_n(i.src, n, mem, next)?;
                let mut out = [0u128; 2];
                for k in 0..(if wide { 4 } else { 2 }) {
                    let x = lane(b, 4, k);
                    let y = if op == CvtOp::Ps2Pd {
                        each(x, &|x, e| fpu::convert(F32, F64, x, e))
                    } else {
                        each(x, &|x, e| fpu::from_int(F64, x as u32 as i32 as i64, e))
                    };
                    out[(k / 2) as usize] = set_lane(out[(k / 2) as usize], 8, k % 2, y);
                }
                (out[0], out[1])
            }
            CvtOp::Pd2Ps | CvtOp::Pd2Dq(_) => {
                // Narrowing: an xmm (ymm when wide) source fills the low half of an xmm.
                let b = self.second(&i, 16, wide, mem, next)?;
                let mut r = 0u128;
                for k in 0..(if wide { 4 } else { 2 }) {
                    let x = lane(if k < 2 { b.0 } else { b.1 }, 8, k % 2);
                    let y = match op {
                        CvtOp::Pd2Ps => each(x, &|x, e| fpu::convert(F64, F32, x, e)),
                        CvtOp::Pd2Dq(t) => each(x, &|x, e| fpu::to_int(F64, x, 32, t, e)),
                        _ => unreachable!(),
                    };
                    r = set_lane(r, 4, k, y);
                }
                self.raise(flags, here)?;
                return self.put_v(&i, r, mem, next);
            }
        };
        self.raise(flags, here)?;
        let wide_out = wide
            && !matches!(
                op,
                CvtOp::Ss2Sd | CvtOp::Sd2Ss | CvtOp::Si2Ss | CvtOp::Si2Sd
            );
        self.put_fp(&i, result, wide_out, mem, next)
    }

    /// movss / movsd.
    pub(super) fn movs(
        &mut self,
        t: Ty,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let w = t.width();
        match (i.dst, i.src) {
            (Operand::Xmm(_), Operand::Mem(_)) => {
                let v = self.get_n(i.src, w, mem, next)? & mask(w) as u128;
                self.put_v(&i, v, mem, next)
            }
            (Operand::Mem(_), Operand::Xmm(s)) => {
                let v = self.xmm[s as usize] as u64 & mask(w);
                self.put(i.dst, w, v, mem, next)
            }
            (Operand::Xmm(_), Operand::Xmm(s)) => {
                let base = self.first_y(&i).0;
                let v = set_lane(base, w, 0, self.xmm[s as usize] as u64);
                self.put_v(&i, v, mem, next)
            }
            _ => unreachable!("decoder: movss operands"),
        }
    }

    pub(super) fn movlh(
        &mut self,
        op: LhOp,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let lo64 = u64::MAX as u128;
        match op {
            LhOp::StoreLo | LhOp::StoreHi => {
                let Operand::Xmm(s) = i.src else {
                    unreachable!()
                };
                let v = self.xmm[s as usize];
                let half = if op == LhOp::StoreLo { v } else { v >> 64 };
                return self.put(i.dst, 8, half as u64, mem, next);
            }
            LhOp::DupLo | LhOp::DupHi => {
                let wide = i.vex & VEX_L != 0;
                let b = self.second(&i, 16, wide, mem, next)?;
                let dup = |v: u128| {
                    let pick = if op == LhOp::DupLo {
                        [0, 0, 2, 2]
                    } else {
                        [1, 1, 3, 3]
                    };
                    (0..4).fold(0u128, |r, k| {
                        set_lane(r, 4, k, lane(v, 4, pick[k as usize]))
                    })
                };
                let r = (dup(b.0), if wide { dup(b.1) } else { 0 });
                return self.put_fp(&i, r, wide, mem, next);
            }
            _ => {}
        }
        let a = self.first_y(&i).0;
        let r = match op {
            LhOp::LoadLo => (a & !lo64) | (self.get_n(i.src, 8, mem, next)? & lo64),
            LhOp::LoadHi => (a & lo64) | (self.get_n(i.src, 8, mem, next)? << 64),
            LhOp::HiToLo => (a & !lo64) | (self.get_n(i.src, 16, mem, next)? >> 64),
            LhOp::LoToHi => (a & lo64) | (self.get_n(i.src, 16, mem, next)? << 64),
            _ => unreachable!(),
        };
        self.put_v(&i, r, mem, next)
    }

    pub(super) fn shufp(
        &mut self,
        t: Ty,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let wide = i.vex & VEX_L != 0;
        let a = self.first_y(&i);
        let b = self.second(&i, 16, wide, mem, next)?;
        let imm = i.imm as u32;
        let shuf = |a: u128, b: u128, half: u32| -> u128 {
            if t == Ty::Ps {
                let sel = |k: u32| (imm >> (2 * k)) & 3;
                let r = set_lane(0, 4, 0, lane(a, 4, sel(0)));
                let r = set_lane(r, 4, 1, lane(a, 4, sel(1)));
                let r = set_lane(r, 4, 2, lane(b, 4, sel(2)));
                set_lane(r, 4, 3, lane(b, 4, sel(3)))
            } else {
                let bit = |k: u32| (imm >> (2 * half + k)) & 1;
                set_lane(
                    set_lane(0, 8, 0, lane(a, 8, bit(0))),
                    8,
                    1,
                    lane(b, 8, bit(1)),
                )
            }
        };
        let r = (shuf(a.0, b.0, 0), if wide { shuf(a.1, b.1, 1) } else { 0 });
        self.put_fp(&i, r, wide, mem, next)
    }

    pub(super) fn movmsk(
        &mut self,
        t: Ty,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let Operand::Xmm(s) = i.src else {
            unreachable!()
        };
        let w = t.width();
        let n = 16 / w as u32;
        let halves = [self.xmm[s as usize], self.ymm_hi[s as usize]];
        let count = if i.vex & VEX_L != 0 { 2 } else { 1 };
        let mut m = 0u64;
        for (h, v) in halves.iter().take(count).enumerate() {
            for k in 0..n {
                m |= (lane(*v, w, k) >> (8 * w as u32 - 1)) << (h as u32 * n + k);
            }
        }
        self.put(i.dst, 4, m, mem, next)
    }

    /// ldmxcsr / stmxcsr; reserved bits in a loaded value are #GP.
    pub(super) fn mxcsr_op(
        &mut self,
        load: bool,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        if load {
            let v = self.get(i.dst, 4, mem, next)? as u32;
            if v & !MXCSR_MASK != 0 {
                return Err(Stop::GeneralProtection(next.wrapping_sub(i.len as u64)));
            }
            self.mxcsr = v;
            Ok(())
        } else {
            self.put(i.dst, 4, self.mxcsr as u64, mem, next)
        }
    }
}

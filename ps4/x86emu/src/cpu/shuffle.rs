//! Blends, permutes, broadcasts, insertps, dpps and the F16C conversions.

use super::float::{lane, set_lane};
use super::{mask, Cpu, Mem, Stop};
use crate::decode::{Inst, Operand, SseOp, Ty, VEX_L};
use crate::fpu::{self, Env, Round, F16, F32, F64};

/// Elements of `a` replaced by `b`'s where `pick(k)` holds.
fn blend(a: u128, b: u128, e: u8, pick: impl Fn(u32) -> bool) -> u128 {
    (0..16 / e as u32).fold(a, |r, k| {
        if pick(k) {
            set_lane(r, e, k, lane(b, e, k))
        } else {
            r
        }
    })
}

impl Cpu {
    pub(super) fn shuffle(
        &mut self,
        op: SseOp,
        i: Inst,
        mem: &mut dyn Mem,
        next: u64,
    ) -> Result<(), Stop> {
        let here = next.wrapping_sub(i.len as u64);
        let wide = i.vex & VEX_L != 0;
        let imm = i.imm as u32;
        let r: (u128, u128) = match op {
            SseOp::Blend(e) => {
                let (a, b) = (self.first_y(&i), self.second(&i, 16, wide, mem, next)?);
                let n = 16 / e as u32;
                (
                    blend(a.0, b.0, e, |k| imm >> k & 1 != 0),
                    blend(a.1, b.1, e, |k| imm >> (n + k) & 1 != 0),
                )
            }
            SseOp::Blendv(e) => {
                let (a, b) = (self.first_y(&i), self.second(&i, 16, wide, mem, next)?);
                let m = if i.vex & crate::decode::VEX != 0 {
                    i.imm as usize
                } else {
                    0
                };
                let (m0, m1) = (self.xmm[m], self.ymm_hi[m]);
                let sign = |v: u128, k: u32| lane(v, e, k) >> (8 * e as u32 - 1) != 0;
                (
                    blend(a.0, b.0, e, |k| sign(m0, k)),
                    blend(a.1, b.1, e, |k| sign(m1, k)),
                )
            }
            SseOp::Insertps => {
                let a = self.first_y(&i).0;
                let v = match i.src {
                    Operand::Xmm(s) => lane(self.xmm[s as usize], 4, (imm >> 6) & 3),
                    s => self.get_n(s, 4, mem, next)? as u64 & mask(4),
                };
                let r = set_lane(a, 4, (imm >> 4) & 3, v);
                (blend(r, 0, 4, |k| imm >> k & 1 != 0), 0)
            }
            SseOp::Dp(t) => {
                let (a, b) = (self.first_y(&i), self.second(&i, 16, wide, mem, next)?);
                let mut flags = 0;
                let lo = self.dot(t, a.0, b.0, imm, &mut flags);
                let hi = if wide {
                    self.dot(t, a.1, b.1, imm, &mut flags)
                } else {
                    0
                };
                self.raise(flags, here)?;
                (lo, hi)
            }
            SseOp::Broadcast(n) => {
                let v = self.get_n(i.src, n, mem, next)?;
                let x = match n {
                    4 => (0..4).fold(0, |r, k| set_lane(r, 4, k, v as u64)),
                    8 => (0..2).fold(0, |r, k| set_lane(r, 8, k, v as u64)),
                    _ => v,
                };
                (x, if wide { x } else { 0 })
            }
            SseOp::Perm2f128 => {
                let (a, b) = (self.first_y(&i), self.second(&i, 16, true, mem, next)?);
                let sel = |c: u32| {
                    if c & 8 != 0 {
                        return 0;
                    }
                    [a.0, a.1, b.0, b.1][(c & 3) as usize]
                };
                (sel(imm), sel(imm >> 4))
            }
            SseOp::PermilV(t) | SseOp::PermilI(t) => {
                let (data, ctrl) = if matches!(op, SseOp::PermilV(_)) {
                    (self.first_y(&i), self.second(&i, 16, wide, mem, next)?)
                } else {
                    (self.second(&i, 16, wide, mem, next)?, (0, 0))
                };
                let permute = |d: u128, c: u128, half: u32| -> u128 {
                    if t == Ty::Ps {
                        (0..4).fold(0, |r, k| {
                            let s = if matches!(op, SseOp::PermilV(_)) {
                                lane(c, 4, k) as u32 & 3
                            } else {
                                (imm >> (2 * k)) & 3
                            };
                            set_lane(r, 4, k, lane(d, 4, s))
                        })
                    } else {
                        (0..2).fold(0, |r, k| {
                            let s = if matches!(op, SseOp::PermilV(_)) {
                                (lane(c, 8, k) as u32 >> 1) & 1
                            } else {
                                (imm >> (2 * half + k)) & 1
                            };
                            set_lane(r, 8, k, lane(d, 8, s))
                        })
                    }
                };
                (
                    permute(data.0, ctrl.0, 0),
                    if wide { permute(data.1, ctrl.1, 1) } else { 0 },
                )
            }
            SseOp::Cvtph2ps => {
                let v = self.get_n(i.src, if wide { 16 } else { 8 }, mem, next)?;
                // Exact: subnormal halves become normal singles; DAZ and DE do not apply.
                let mut flags = 0;
                let mut out = [0u128; 2];
                for k in 0..(if wide { 8 } else { 4 }) {
                    let mut env = Env {
                        round: Round::Nearest,
                        ftz: false,
                        daz: false,
                        flags: 0,
                    };
                    let x = fpu::convert(F16, F32, lane(v, 2, k), &mut env);
                    flags |= env.flags & fpu::IE;
                    out[(k / 4) as usize] = set_lane(out[(k / 4) as usize], 4, k % 4, x);
                }
                self.raise(flags, here)?;
                (out[0], out[1])
            }
            SseOp::Cvtps2ph => {
                let Operand::Xmm(s) = i.src else {
                    unreachable!()
                };
                let src = [self.xmm[s as usize], self.ymm_hi[s as usize]];
                let mut env0 = Env::from_mxcsr(self.mxcsr);
                env0.ftz = false; // FTZ does not apply to vcvtps2ph
                if imm & 4 == 0 {
                    env0.round =
                        [Round::Nearest, Round::Down, Round::Up, Round::Zero][(imm & 3) as usize];
                }
                let mut flags = 0;
                let mut r = 0u128;
                for k in 0..(if wide { 8 } else { 4 }) {
                    let mut env = env0;
                    let x = fpu::convert(F32, F16, lane(src[(k / 4) as usize], 4, k % 4), &mut env);
                    flags |= env.flags;
                    r = set_lane(r, 2, k, x);
                }
                self.raise(flags, here)?;
                return match i.dst {
                    Operand::Xmm(_) => self.put_v(&i, r, mem, next),
                    Operand::Mem(m) => {
                        let a = self.addr(&m, next);
                        for (off, n) in [(0u64, 8u8), (8, 8)].iter().take(if wide { 2 } else { 1 })
                        {
                            if !mem.write(a + off, *n, (r >> (8 * off)) as u64) {
                                return Err(Stop::Fault(a + off));
                            }
                        }
                        self.note_write(a, if wide { 16 } else { 8 });
                        Ok(())
                    }
                    _ => unreachable!(),
                };
            }
            _ => unreachable!("not a shuffle: {op:?}"),
        };
        self.put_fp(&i, r, wide, mem, next)
    }

    /// dpps / dppd on one 128-bit lane: masked products, summed pairwise, broadcast.
    ///
    /// Each output lane sums in its own order, which only shows in which NaN wins. Measured
    /// on an Intel host (Jaguar's cannot be checked here): dpps lanes 0-1 add
    /// (p1 + p0) + (p3 + p2) and lanes 2-3 (p3 + p2) + (p1 + p0); dppd lane k puts pk first.
    fn dot(&self, t: Ty, a: u128, b: u128, imm: u32, flags: &mut u32) -> u128 {
        let mx = self.mxcsr;
        let (fmt, w, n) = if t == Ty::Ps {
            (F32, 4, 4)
        } else {
            (F64, 8, 2)
        };
        let mut run = |f: &dyn Fn(&mut Env) -> u64| {
            let mut env = Env::from_mxcsr(mx);
            let r = f(&mut env);
            *flags |= env.flags;
            r
        };
        let mut p = [0u64; 4];
        for k in 0..n {
            if imm >> (4 + k) & 1 != 0 {
                p[k as usize] = run(&|e| fpu::mul(fmt, lane(a, w, k), lane(b, w, k), e));
            }
        }
        let mut add = |x: u64, y: u64| run(&|e| fpu::add(fmt, x, y, false, e));
        let sums: [u64; 4] = if n == 4 {
            let (s10, s32) = (add(p[1], p[0]), add(p[3], p[2]));
            let low = add(s10, s32);
            let high = add(s32, s10);
            [low, low, high, high]
        } else {
            [add(p[0], p[1]), add(p[1], p[0]), 0, 0]
        };
        (0..n).fold(0, |r, k| {
            if imm >> k & 1 != 0 {
                set_lane(r, w, k, sums[k as usize])
            } else {
                r
            }
        })
    }
}

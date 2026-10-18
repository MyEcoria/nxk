//! Random valid programs, to compare the compiler (and the GPU itself,
//! from the `gpu` service's self-test) with the reference machine.

use alloc::vec::Vec;
use core::cell::RefCell;

use crate::*;

pub struct Rng(pub u64);

impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    pub fn below(&mut self, n: usize) -> usize {
        self.next_u32() as usize % n
    }

    pub fn float(&mut self) -> f32 {
        (self.next_u32() % 8001) as f32 / 1000.0 - 4.0
    }
}

// Random programs.

#[derive(Clone, Default)]
struct Pools {
    f: Vec<Reg>,
    u: Vec<Reg>,
    i: Vec<Reg>,
    bo: Vec<Reg>,
}

fn pick(rng: &RefCell<Rng>, v: &[Reg]) -> Reg {
    let k = rng.borrow_mut().below(v.len());
    v[k]
}

fn roll(rng: &RefCell<Rng>, n: usize) -> usize {
    rng.borrow_mut().below(n)
}

#[allow(clippy::too_many_arguments)]
fn gen_block(
    b: &mut Builder,
    rng: &RefCell<Rng>,
    pools: &mut Pools,
    base: Reg,
    threaded: bool,
    depth: usize,
    in_loop: bool,
    n: usize,
) {
    for _ in 0..n {
        if b.reg_count() > 140 {
            return;
        }
        match roll(rng, 100) {
            0..=19 => {
                let (x, y, z) = (
                    pick(rng, &pools.f),
                    pick(rng, &pools.f),
                    pick(rng, &pools.f),
                );
                let r = match roll(rng, 8) {
                    0 => b.fadd(x, y),
                    1 => b.fsub(x, y),
                    2 => b.fmul(x, y),
                    3 => b.fmin(x, y),
                    4 => b.fmax(x, y),
                    5 => b.fma(x, y, z),
                    6 => {
                        let a = b.fabs(x);
                        b.fsqrt(a)
                    }
                    _ => b.fsat(x),
                };
                pools.f.push(r);
            }
            20..=24 => {
                let x = pick(rng, &pools.f);
                let r = match roll(rng, 6) {
                    0 => b.fneg(x),
                    1 => b.fabs(x),
                    2 => b.ffloor(x),
                    3 => {
                        // The hardware's range reduction is only good
                        // near zero: keep the argument within a few turns.
                        let (lo, hi) = (b.f32(-6.0), b.f32(6.0));
                        let x = b.fmax(x, lo);
                        let x = b.fmin(x, hi);
                        b.fsin(x)
                    }
                    4 => b.fex2(x),
                    _ => {
                        let a = b.fabs(x);
                        b.flg2(a)
                    }
                };
                pools.f.push(r);
            }
            25..=39 => {
                let (x, y) = (pick(rng, &pools.u), pick(rng, &pools.u));
                let r = match roll(rng, 9) {
                    0 => b.uadd(x, y),
                    1 => b.usub(x, y),
                    2 => b.umul(x, y),
                    3 => b.umin(x, y),
                    4 => b.umax(x, y),
                    5 => b.and(x, y),
                    6 => b.or(x, y),
                    7 => b.xor(x, y),
                    _ => b.shl(x, y),
                };
                pools.u.push(r);
            }
            40..=47 => {
                let (x, y) = (pick(rng, &pools.i), pick(rng, &pools.i));
                let r = match roll(rng, 6) {
                    0 => b.iadd(x, y),
                    1 => b.isub(x, y),
                    2 => b.imin(x, y),
                    3 => b.imax(x, y),
                    4 => b.ineg(x),
                    _ => b.iabs(x),
                };
                pools.i.push(r);
            }
            48..=52 => {
                // Conversions between the kinds.
                match roll(rng, 4) {
                    0 => {
                        let x = pick(rng, &pools.f);
                        let r = b.f2i(x);
                        pools.i.push(r);
                    }
                    1 => {
                        let x = pick(rng, &pools.i);
                        let r = b.i2f(x);
                        pools.f.push(r);
                    }
                    2 => {
                        let x = pick(rng, &pools.u);
                        let r = b.ibits(x);
                        pools.i.push(r);
                    }
                    _ => {
                        let x = pick(rng, &pools.i);
                        let r = b.ubits(x);
                        pools.u.push(r);
                    }
                }
            }
            53..=62 => {
                let r = match roll(rng, 3) {
                    0 => {
                        let (x, y) = (pick(rng, &pools.f), pick(rng, &pools.f));
                        b.cmp(cmp_of(roll(rng, 6)), x, y)
                    }
                    1 => {
                        let (x, y) = (pick(rng, &pools.i), pick(rng, &pools.i));
                        b.cmp(cmp_of(roll(rng, 6)), x, y)
                    }
                    _ => {
                        let (x, y) = (pick(rng, &pools.u), pick(rng, &pools.u));
                        b.cmp(cmp_of(roll(rng, 6)), x, y)
                    }
                };
                pools.bo.push(r);
            }
            63..=66 => {
                let (x, y) = (pick(rng, &pools.bo), pick(rng, &pools.bo));
                let r = match roll(rng, 4) {
                    0 => b.band(x, y),
                    1 => b.bor(x, y),
                    2 => b.bxor(x, y),
                    _ => b.bnot(x),
                };
                pools.bo.push(r);
            }
            67..=71 => {
                let p = pick(rng, &pools.bo);
                match roll(rng, 4) {
                    0 => {
                        let (x, y) = (pick(rng, &pools.f), pick(rng, &pools.f));
                        pools.f.push(b.sel(p, x, y));
                    }
                    1 => {
                        let (x, y) = (pick(rng, &pools.u), pick(rng, &pools.u));
                        pools.u.push(b.sel(p, x, y));
                    }
                    2 => {
                        let (x, y) = (pick(rng, &pools.i), pick(rng, &pools.i));
                        pools.i.push(b.sel(p, x, y));
                    }
                    _ => {
                        let (x, y) = (pick(rng, &pools.bo), pick(rng, &pools.bo));
                        pools.bo.push(b.sel(p, x, y));
                    }
                }
            }
            72..=79 => {
                // Carry a value out of a block: assign an older register.
                match roll(rng, 3) {
                    0 => {
                        let (d, s) = (pick(rng, &pools.f), pick(rng, &pools.f));
                        b.mov(d, s);
                    }
                    1 => {
                        let (d, s) = (pick(rng, &pools.u), pick(rng, &pools.u));
                        b.mov(d, s);
                    }
                    _ => {
                        let (d, s) = (pick(rng, &pools.bo), pick(rng, &pools.bo));
                        b.mov(d, s);
                    }
                }
            }
            80..=86 if depth < 3 => {
                let cond = pick(rng, &pools.bo);
                let k = 1 + roll(rng, 4);
                let k2 = roll(rng, 4);
                let (p1, p2) = (pools.clone(), pools.clone());
                b.if_else(
                    cond,
                    |b| {
                        let mut p = p1;
                        gen_block(b, rng, &mut p, base, threaded, depth + 1, in_loop, k);
                    },
                    |b| {
                        let mut p = p2;
                        gen_block(b, rng, &mut p, base, threaded, depth + 1, in_loop, k2);
                    },
                );
            }
            87..=91 if depth < 3 => {
                let max = 1 + roll(rng, 5) as u32;
                let k = 2 + roll(rng, 5);
                let p1 = pools.clone();
                b.loop_(max, |b| {
                    let mut p = p1;
                    gen_block(b, rng, &mut p, base, threaded, depth + 1, true, k);
                });
            }
            92..=93 if in_loop => {
                let c = pick(rng, &pools.bo);
                if roll(rng, 2) == 0 {
                    b.break_if(c);
                } else {
                    b.continue_if(c);
                }
            }
            94..=96 => {
                let u = pick(rng, &pools.u);
                let m = b.u32(15);
                let idx = b.and(u, m);
                let w = [1usize, 2, 4][roll(rng, 3)];
                let t = [Ty::F32, Ty::U32, Ty::I32];
                let types: Vec<Ty> = (0..w).map(|_| t[roll(rng, 3)]).collect();
                let off = roll(rng, 4) as u16;
                let regs = b.load_n(0, idx, off, &types);
                for (r, ty) in regs.iter().zip(&types) {
                    match ty {
                        Ty::F32 => pools.f.push(*r),
                        Ty::U32 => pools.u.push(*r),
                        _ => pools.i.push(*r),
                    }
                }
            }
            97..=98 => {
                let u = pick(rng, &pools.u);
                let m = b.u32(31);
                let idx = b.and(u, m);
                let idx = b.uadd(idx, base);
                let w = [1usize, 2, 4][roll(rng, 3)];
                let srcs: Vec<Reg> = (0..w).map(|_| pick(rng, &pools.u)).collect();
                b.store(1, idx, 0, &srcs);
            }
            _ if threaded => {}
            _ => {
                let u = pick(rng, &pools.u);
                let m = b.u32(3);
                let idx = b.and(u, m);
                let v = pick(rng, &pools.u);
                let old = b.atomic_add(2, idx, v);
                pools.u.push(old);
            }
        }
    }
}

fn cmp_of(k: usize) -> CmpOp {
    [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ][k]
}

/// A random valid compute program. Inputs: buffer 0 (16 words: 4 floats,
/// 4 unsigned, 4 signed, then free), outputs: buffer 1, atomics: buffer 2.
/// `threaded`: made for many threads: each thread's values depend on its
/// id (the lanes of a warp take different paths) and it writes its own
/// 64 words (`tid * 64`); no atomics.
pub fn random_program(seed: u64, threaded: bool) -> Program {
    let rng = RefCell::new(Rng(seed));
    let mut b = Builder::new(Stage::Compute);
    let mut pools = Pools::default();
    let z = b.u32(0);
    let mut base = z;
    let mut tid = z;
    if threaded {
        tid = b.builtin(Builtin::LocalIdX);
        let six = b.u32(6);
        base = b.shl(tid, six);
    }
    for k in 0..4u16 {
        let mut f = b.load(0, z, k, Ty::F32);
        let mut u = b.load(0, z, 4 + k, Ty::U32);
        if threaded {
            // Every lane has values of its own.
            let t = b.u2f(tid);
            let q = b.f32(0.37);
            let d = b.fmul(t, q);
            f = b.fadd(f, d);
            let m = b.u32(7 + k as u32);
            let tt = b.umul(tid, m);
            u = b.uadd(u, tt);
        }
        let i = b.ibits(u);
        pools.f.push(f);
        pools.u.push(u);
        pools.i.push(i);
    }
    for k in 0..3 {
        let c = b.cmp(cmp_of(k), pools.f[k], pools.f[k + 1]);
        pools.bo.push(c);
    }
    let n = 12 + roll(&rng, 20);
    gen_block(&mut b, &rng, &mut pools, base, threaded, 0, false, n);
    // Show what the registers hold.
    for (k, r) in pools.u.iter().rev().take(6).enumerate() {
        let i = b.u32(32 + k as u32);
        let i = b.uadd(i, base);
        b.store(1, i, 0, &[*r]);
    }
    for (k, r) in pools.f.iter().rev().take(6).enumerate() {
        let i = b.u32(40 + k as u32);
        let i = b.uadd(i, base);
        let bits = b.fbits(*r);
        b.store(1, i, 0, &[bits]);
    }
    for (k, r) in pools.bo.iter().rev().take(4).enumerate() {
        let i = b.u32(48 + k as u32);
        let i = b.uadd(i, base);
        let one = b.u32(1);
        let zero = b.u32(0);
        let w = b.sel(*r, one, zero);
        b.store(1, i, 0, &[w]);
    }
    b.finish()
}

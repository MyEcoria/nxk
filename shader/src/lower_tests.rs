//! The compiler against the reference machine: the same program on
//! `interp` and, compiled, on the SASS simulator.

use std::string::String;
use std::vec;
use std::vec::Vec;

use crate::interp::{run, Machine, NoTextures};
use crate::lower::{compile, CB_BUFFERS, CB_TEXTURES, CB_UNIFORMS};
use crate::sass::{SR_CTAID_X, SR_TID_X};
use crate::sassim::Sim;
use crate::*;

const BASE: u64 = 0x2_0000_0000;

fn base_of(i: usize) -> u64 {
    BASE + 0x10000 * i as u64
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    pub fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n
    }

    pub fn float(&mut self) -> f32 {
        (self.next() % 8001) as f32 / 1000.0 - 4.0
    }
}

fn same(a: u32, b: u32) -> bool {
    if a == b {
        return true;
    }
    let (x, y) = (f32::from_bits(a), f32::from_bits(b));
    if x.is_nan() && y.is_nan() {
        return true;
    }
    if x == y {
        return true;
    }
    if x.is_infinite() || y.is_infinite() {
        return false;
    }
    (x - y).abs() <= 2e-4 * x.abs().max(y.abs()).max(1e-3)
}

/// Run `p` (compute) both ways; the buffers after, each.
type Buffers = Vec<Vec<u32>>;

pub fn both(
    p: &Program,
    uniforms: &[u32],
    bufs: &[Vec<u32>],
) -> Result<(Buffers, Buffers), String> {
    // Reference.
    let mut ref_bufs: Vec<Vec<u32>> = bufs.to_vec();
    ref_bufs.resize(MAX_BUFFERS, Vec::new());
    let mut m = Machine {
        inputs: [[0; 4]; MAX_ATTRS],
        builtins: [0; 11],
        uniforms,
        buffers: &mut ref_bufs,
        sampler: &mut NoTextures,
    };
    m.builtins[0] = 5; // local id x
    m.builtins[3] = 7; // group id x
    run(p, &mut m);
    ref_bufs.truncate(bufs.len());

    // Compiled.
    let c = compile(p).map_err(|e| std::format!("compile: {e:?}"))?;
    let mut tex = NoTextures;
    let mut s = Sim::new(&c.code, &mut tex);
    s.cbuf[CB_UNIFORMS as usize] = uniforms.to_vec();
    let mut desc = Vec::new();
    for i in 0..MAX_BUFFERS {
        let (lo, hi, len) = match bufs.get(i) {
            Some(b) => (base_of(i) as u32, (base_of(i) >> 32) as u32, b.len() as u32),
            None => (0, 0, 0),
        };
        desc.extend([lo, hi, len, 0]);
    }
    s.cbuf[CB_BUFFERS as usize] = desc;
    s.cbuf[CB_TEXTURES as usize] = (0..MAX_TEXTURES as u32).collect();
    for (i, b) in bufs.iter().enumerate() {
        s.mem.push((base_of(i), b.clone()));
    }
    s.sregs.insert(SR_TID_X, 5);
    s.sregs.insert(SR_CTAID_X, 7);
    for k in 1..3 {
        s.sregs.insert(SR_TID_X + k, 0);
        s.sregs.insert(SR_CTAID_X + k, 0);
    }
    s.regs[255] = 0;
    s.run().map_err(|e| std::format!("sim: {e}"))?;
    let out: Vec<Vec<u32>> = s.mem.iter().map(|(_, v)| v.clone()).collect();
    Ok((ref_bufs, out))
}

pub fn check(p: &Program, uniforms: &[u32], bufs: &[Vec<u32>]) -> Result<(), String> {
    let (r, s) = both(p, uniforms, bufs)?;
    for (bi, (x, y)) in r.iter().zip(&s).enumerate() {
        for (i, (&a, &b)) in x.iter().zip(y).enumerate() {
            if !same(a, b) {
                return Err(std::format!(
                    "buffer {bi}[{i}]: reference {a:#010x} ({}), compiled {b:#010x} ({})",
                    f32::from_bits(a),
                    f32::from_bits(b)
                ));
            }
        }
    }
    Ok(())
}

fn op_program(group: u8) -> (Program, usize) {
    let mut b = Builder::new(Stage::Compute);
    let z = b.u32(0);
    let x = b.load(0, z, 0, Ty::F32);
    let y = b.load(0, z, 1, Ty::F32);
    let ix = b.load(0, z, 2, Ty::I32);
    let iy = b.load(0, z, 3, Ty::I32);
    let ux = b.ubits(ix);
    let uy = b.ubits(iy);
    let mut outs: Vec<Reg> = Vec::new();
    let ax = b.fabs(x);
    match group {
        0 => {
            outs.push(b.fadd(x, y));
            outs.push(b.fsub(x, y));
            outs.push(b.fmul(x, y));
            outs.push(b.fmin(x, y));
            outs.push(b.fmax(x, y));
            outs.push(b.fma(x, y, x));
            outs.push(b.fneg(x));
            outs.push(b.fabs(x));
            outs.push(b.frcp(ax));
            outs.push(b.frsq(ax));
            outs.push(b.fsqrt(ax));
            outs.push(b.fsin(x));
            outs.push(b.fcos(x));
            outs.push(b.fex2(x));
            outs.push(b.flg2(ax));
            outs.push(b.ffloor(x));
            outs.push(b.fsat(x));
        }
        1 => {
            let t = b.f2i(x);
            outs.push(b.ubits(t));
            outs.push(b.f2u(x));
            outs.push(b.i2f(ix));
            outs.push(b.u2f(ux));
            let t = b.ineg(ix);
            outs.push(b.ubits(t));
            let t = b.iabs(ix);
            outs.push(b.ubits(t));
            outs.push(b.not(ux));
            let t = b.iadd(ix, iy);
            outs.push(b.ubits(t));
            let t = b.isub(ix, iy);
            outs.push(b.ubits(t));
            let t = b.imul(ix, iy);
            outs.push(b.ubits(t));
            let t = b.imin(ix, iy);
            outs.push(b.ubits(t));
            let t = b.imax(ix, iy);
            outs.push(b.ubits(t));
            outs.push(b.umin(ux, uy));
            outs.push(b.umax(ux, uy));
            outs.push(b.and(ux, uy));
            outs.push(b.or(ux, uy));
            outs.push(b.xor(ux, uy));
            outs.push(b.shl(ux, uy));
            outs.push(b.shr(ux, uy));
            let t = b.sar(ix, uy);
            outs.push(b.ubits(t));
        }
        _ => {
            let one = b.u32(1);
            let zero = b.u32(0);
            let ops = [
                CmpOp::Eq,
                CmpOp::Ne,
                CmpOp::Lt,
                CmpOp::Le,
                CmpOp::Gt,
                CmpOp::Ge,
            ];
            for op in ops {
                for (p, q) in [(x, y), (ix, iy), (ux, uy)] {
                    let c = b.cmp(op, p, q);
                    outs.push(b.sel(c, one, zero));
                }
            }
            let c1 = b.cmp(CmpOp::Lt, x, y);
            let c2 = b.cmp(CmpOp::Gt, ix, iy);
            for r in [b.band(c1, c2), b.bor(c1, c2), b.bxor(c1, c2), b.bnot(c1)] {
                outs.push(b.sel(r, one, zero));
            }
        }
    }
    let n = outs.len();
    for (k, o) in outs.into_iter().enumerate() {
        let i = b.u32(k as u32);
        let ob = if b.ty(o) == Ty::F32 { b.fbits(o) } else { o };
        b.store(1, i, 0, &[ob]);
    }
    (b.finish(), n)
}

#[test]
fn every_unary_and_binary_op() {
    for group in 0..3 {
        let (p, n) = op_program(group);
        let mut rng = Rng(1 + group as u64);
        for _ in 0..60 {
            let mut inp = vec![
                rng.float().to_bits(),
                rng.float().to_bits(),
                (rng.next() % 2000).wrapping_sub(1000),
                rng.next() % 70,
            ];
            if rng.below(4) == 0 {
                inp[2] = rng.next();
                inp[3] = rng.next();
            }
            if rng.below(5) == 0 {
                inp[1] = inp[0];
            }
            let r = check(&p, &[], &[inp.clone(), vec![0xAAAA_AAAA; n]]);
            assert!(
                r.is_ok(),
                "group {group}, inputs {inp:x?}: {}",
                r.unwrap_err()
            );
        }
    }
}

// Random programs.

use core::cell::RefCell;

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

fn gen_block(
    b: &mut Builder,
    rng: &RefCell<Rng>,
    pools: &mut Pools,
    depth: usize,
    in_loop: bool,
    n: usize,
) {
    for _ in 0..n {
        if b.reg_count() > 165 {
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
                    3 => b.fsin(x),
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
                        gen_block(b, rng, &mut p, depth + 1, in_loop, k);
                    },
                    |b| {
                        let mut p = p2;
                        gen_block(b, rng, &mut p, depth + 1, in_loop, k2);
                    },
                );
            }
            87..=91 if depth < 3 => {
                let max = 1 + roll(rng, 5) as u32;
                let k = 2 + roll(rng, 5);
                let p1 = pools.clone();
                b.loop_(max, |b| {
                    let mut p = p1;
                    gen_block(b, rng, &mut p, depth + 1, true, k);
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
                let w = [1usize, 2, 4][roll(rng, 3)];
                let srcs: Vec<Reg> = (0..w).map(|_| pick(rng, &pools.u)).collect();
                b.store(1, idx, 0, &srcs);
            }
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

pub fn random_program(seed: u64) -> Program {
    let rng = RefCell::new(Rng(seed));
    let mut b = Builder::new(Stage::Compute);
    let mut pools = Pools::default();
    let z = b.u32(0);
    for k in 0..4u16 {
        pools.f.push(b.load(0, z, k, Ty::F32));
        pools.u.push(b.load(0, z, 4 + k, Ty::U32));
        pools.i.push(b.load(0, z, 8 + k, Ty::I32));
    }
    for k in 0..3 {
        let c = b.cmp(cmp_of(k), pools.f[k], pools.f[k + 1]);
        pools.bo.push(c);
    }
    gen_block(&mut b, &rng, &mut pools, 0, false, 12 + roll(&rng, 20));
    // Show what the registers hold.
    for (k, r) in pools.u.iter().rev().take(6).enumerate() {
        let i = b.u32(32 + k as u32);
        b.store(1, i, 0, &[*r]);
    }
    for (k, r) in pools.f.iter().rev().take(6).enumerate() {
        let i = b.u32(40 + k as u32);
        let bits = b.fbits(*r);
        b.store(1, i, 0, &[bits]);
    }
    for (k, r) in pools.bo.iter().rev().take(4).enumerate() {
        let i = b.u32(48 + k as u32);
        let one = b.u32(1);
        let zero = b.u32(0);
        let w = b.sel(*r, one, zero);
        b.store(1, i, 0, &[w]);
    }
    b.finish()
}

#[test]
fn random_programs_match_the_reference() {
    let seeds: u64 = std::env::var("SHADER_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    for seed in 1..=seeds {
        let p = random_program(seed);
        validate(&p).unwrap_or_else(|e| panic!("seed {seed}: invalid program: {e}"));
        let mut rng = Rng(seed * 7919);
        for _ in 0..3 {
            let mut b0: Vec<u32> = (0..4).map(|_| rng.float().to_bits()).collect();
            b0.extend((0..4).map(|_| rng.next() % 40));
            b0.extend((0..4).map(|_| (rng.next() % 200).wrapping_sub(100)));
            b0.extend((0..8).map(|_| rng.next()));
            let b1 = vec![0xAAAA_AAAA; 56];
            let b2 = vec![5, 6, 7, 8];
            if let Err(e) = check(&p, &[], &[b0.clone(), b1, b2]) {
                panic!("seed {seed}, inputs {b0:x?}: {e}\n{p:#?}");
            }
        }
    }
}

#[test]
fn fuzzer_covers_control_flow() {
    let (mut ifs, mut loops, mut brks, mut insts, mut preds, mut ssy) = (0, 0, 0, 0, 0, 0);
    for seed in 1..=200 {
        let p = random_program(seed);
        walk(&p.body, &mut |i| {
            insts += 1;
            match i {
                Inst::If { .. } => ifs += 1,
                Inst::Loop { .. } => loops += 1,
                Inst::Break { .. } | Inst::Continue { .. } => brks += 1,
                _ => {}
            }
        });
        let c = compile(&p).unwrap();
        for g in c.code.chunks(4) {
            for w in &g[1..] {
                if (w >> 48) as u16 == 0xE290 {
                    ssy += 1
                }
                if (w >> 16 & 7) != 7 {
                    preds += 1;
                }
            }
        }
    }
    std::println!("ifs {ifs} loops {loops} breaks {brks} insts {insts} guarded {preds} ssy {ssy}");
    assert!(ifs > 100 && loops > 50 && brks > 20 && ssy > 20 && preds > 100);
}

// Vertex and fragment shaders.

use crate::interp::Sampler;

struct Fake;

impl Sampler for Fake {
    fn sample(&mut self, tex: u8, u: f32, v: f32, lod: Option<f32>) -> [f32; 4] {
        [u * 2.0 + tex as f32, v + 1.0, lod.unwrap_or(0.5), 1.0]
    }
}

struct Gfx {
    outputs: interp::Outputs,
    sim_out: std::collections::BTreeMap<u32, u32>,
    sim_regs: Vec<u32>,
    killed: bool,
}

fn gfx(p: &Program, uniforms: &[u32], inputs: &[(usize, usize, f32)]) -> Result<Gfx, String> {
    let mut bufs: Vec<Vec<u32>> = vec![Vec::new(); MAX_BUFFERS];
    let mut m = Machine {
        inputs: [[0; 4]; MAX_ATTRS],
        builtins: [0; 11],
        uniforms,
        buffers: &mut bufs,
        sampler: &mut Fake,
    };
    m.builtins[Builtin::VertexId as usize] = 9;
    m.builtins[Builtin::InstanceId as usize] = 3;
    for &(a, c, v) in inputs {
        m.inputs[a][c] = v.to_bits();
    }
    let outputs = run(p, &mut m);
    let c = compile(p).map_err(|e| std::format!("compile: {e:?}"))?;
    let mut fake = Fake;
    let mut s = Sim::new(&c.code, &mut fake);
    s.cbuf[CB_UNIFORMS as usize] = uniforms.to_vec();
    s.cbuf[CB_TEXTURES as usize] = (0..MAX_TEXTURES as u32).collect();
    for &(a, c, v) in inputs {
        s.attrs_in
            .insert(0x80 + 16 * a as u32 + 4 * c as u32, v.to_bits());
    }
    s.attrs_in.insert(0x7C, 1.0f32.to_bits()); // 1 / w
    s.attrs_in.insert(0x2FC, 9);
    s.attrs_in.insert(0x2F8, 3);
    s.run().map_err(|e| std::format!("sim: {e}"))?;
    Ok(Gfx {
        outputs,
        sim_out: s.attrs_out.clone(),
        sim_regs: s.regs.to_vec(),
        killed: s.killed,
    })
}

fn close(a: f32, b: f32) -> bool {
    same(a.to_bits(), b.to_bits())
}

#[test]
fn vertex_shader_transform() {
    // clip = M * (pos, 1); varying 0 = color * ambient; varying 1 = ids.
    let mut b = Builder::new(Stage::Vertex);
    let pos: Vec<Reg> = (0..3).map(|c| b.input(0, c)).collect();
    let one = b.f32(1.0);
    for row in 0..4u16 {
        let mut acc = b.uniform(4 * row + 3, Ty::F32);
        for (c, p) in pos.iter().enumerate() {
            let m = b.uniform(4 * row + c as u16, Ty::F32);
            acc = b.fma(m, *p, acc);
        }
        b.output(OutKind::Position, row as u8, acc);
    }
    let amb = b.uniform(16, Ty::F32);
    for c in 0..4 {
        let col = b.input(1, c);
        let v = b.fmul(col, amb);
        b.output(OutKind::Varying(0), c, v);
    }
    let vid = b.builtin(Builtin::VertexId);
    let iid = b.builtin(Builtin::InstanceId);
    let sum = b.uadd(vid, iid);
    let f = b.u2f(sum);
    b.output(OutKind::Varying(1), 0, f);
    b.output(OutKind::Varying(1), 1, one);
    let p = b.finish();
    let mut rng = Rng(5);
    let mut uni: Vec<u32> = (0..16).map(|_| rng.float().to_bits()).collect();
    uni.push(0.25f32.to_bits());
    let mut inputs: Vec<(usize, usize, f32)> = Vec::new();
    for c in 0..3 {
        inputs.push((0, c, rng.float()));
    }
    for c in 0..4 {
        inputs.push((1, c, rng.float()));
    }
    let g = gfx(&p, &uni, &inputs).unwrap();
    for c in 0..4 {
        let got = f32::from_bits(g.sim_out[&(0x70 + 4 * c)]);
        assert!(close(got, g.outputs.position[c as usize]), "position {c}");
        let got = f32::from_bits(g.sim_out[&(0x80 + 4 * c)]);
        assert!(close(got, g.outputs.varyings[0][c as usize]), "varying {c}");
    }
    assert_eq!(f32::from_bits(g.sim_out[&0x90]), 12.0);
    let c = compile(&p).unwrap();
    // Header: inputs 0.0-0.2 and 1.0-1.3, outputs.
    assert_eq!(c.header[6] & 0xFF, 0x07 | 0xF << 4);
    assert_eq!(c.header[0] >> 10 & 0xF, 1);
}

#[test]
fn fragment_shader_inputs_texture_discard() {
    let mut b = Builder::new(Stage::Fragment);
    let u = b.input(0, 0);
    let v = b.input(0, 1);
    let n = b.input_as(1, 0, Interp::Flat);
    let l = b.input_as(1, 1, Interp::Linear);
    let t = b.sample(2, u, v);
    let zero = b.f32(0.0);
    let dead = b.cmp(CmpOp::Lt, n, zero);
    b.discard_if(dead);
    let lod = b.f32(2.0);
    let t2 = b.sample_lod(1, u, v, lod);
    let k = b.uniform(0, Ty::F32);
    for c in 0..4 {
        let a = b.fmul(t[c], k);
        let r = b.fma(t2[c], l, a);
        b.output(OutKind::Color(0), c as u8, r);
    }
    b.output(OutKind::Color(1), 0, n);
    b.output(OutKind::Color(1), 1, u);
    b.output(OutKind::Color(1), 2, v);
    b.output(OutKind::Color(1), 3, l);
    let p = b.finish();
    let uni = [0.5f32.to_bits()];
    for n in [1.5f32, -1.0] {
        let inputs = [(0, 0, 0.25), (0, 1, 0.75), (1, 0, n), (1, 1, 2.0)];
        let g = gfx(&p, &uni, &inputs).unwrap();
        assert_eq!(g.killed, g.outputs.discarded);
        assert_eq!(g.killed, n < 0.0);
        if !g.killed {
            for c in 0..4 {
                assert!(
                    close(f32::from_bits(g.sim_regs[c]), g.outputs.colors[0][c]),
                    "c0.{c}"
                );
                assert!(
                    close(f32::from_bits(g.sim_regs[4 + c]), g.outputs.colors[1][c]),
                    "c1.{c}"
                );
            }
        }
    }
    let c = compile(&p).unwrap();
    assert!(c.header[0] >> 15 & 1 == 1, "kills pixels");
    // Color targets 0 and 1, four components each.
    assert_eq!(c.header[18] & 0xFF, 0xFF);
}

#[test]
fn builtins_and_dynamic_uniforms() {
    let mut b = Builder::new(Stage::Compute);
    let tid = b.builtin(Builtin::LocalIdX);
    let gid = b.builtin(Builtin::GroupIdX);
    let m = b.u32(3);
    let i = b.and(tid, m);
    let w = b.uniform_idx(2, i, Ty::U32);
    let z = b.u32(0);
    let one = b.u32(1);
    b.store(0, z, 0, &[tid, gid]);
    b.store(0, one, 1, &[w]);
    let p = b.finish();
    let r = check(&p, &[10, 20, 30, 40, 50, 60], &[vec![0; 4]]);
    assert!(r.is_ok(), "{}", r.unwrap_err());
}

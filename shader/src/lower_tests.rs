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

use crate::fuzz::{random_program, Rng};

const BASE: u64 = 0x2_0000_0000;

fn base_of(i: usize) -> u64 {
    BASE + 0x10000 * i as u64
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

/// Run `p` (compute) as thread `tid` of its group, both ways.
pub fn both_t(
    p: &Program,
    uniforms: &[u32],
    bufs: &[Vec<u32>],
    tid: u32,
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
    m.builtins[0] = tid; // local id x
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
    s.sregs.insert(SR_TID_X, tid);
    s.sregs.insert(SR_CTAID_X, 7);
    for k in 1..3 {
        s.sregs.insert(SR_TID_X + k, 0);
        s.sregs.insert(SR_CTAID_X + k, 0);
    }
    s.regs[255] = 0;
    // Local memory starts as junk, as on the hardware: a program writes
    // before it reads.
    s.local = vec![0xDEAD_BEEF; p.local_words as usize];
    s.run().map_err(|e| std::format!("sim: {e}"))?;
    let out: Vec<Vec<u32>> = s.mem.iter().map(|(_, v)| v.clone()).collect();
    Ok((ref_bufs, out))
}

pub fn check(p: &Program, uniforms: &[u32], bufs: &[Vec<u32>]) -> Result<(), String> {
    check_t(p, uniforms, bufs, 5)
}

pub(crate) fn check_t(
    p: &Program,
    uniforms: &[u32],
    bufs: &[Vec<u32>],
    tid: u32,
) -> Result<(), String> {
    let (r, s) = both_t(p, uniforms, bufs, tid)?;
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
                (rng.next_u32() % 2000).wrapping_sub(1000),
                rng.next_u32() % 70,
            ];
            if rng.below(4) == 0 {
                inp[2] = rng.next_u32();
                inp[3] = rng.next_u32();
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

#[test]
fn random_programs_match_the_reference() {
    let seeds: u64 = std::env::var("SHADER_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    for seed in 1..=seeds {
        let p = random_program(seed, false);
        validate(&p).unwrap_or_else(|e| panic!("seed {seed}: invalid program: {e}"));
        let mut rng = Rng(seed * 7919);
        for _ in 0..3 {
            let mut b0: Vec<u32> = (0..4).map(|_| rng.float().to_bits()).collect();
            b0.extend((0..4).map(|_| rng.next_u32() % 40));
            b0.extend((0..4).map(|_| (rng.next_u32() % 200).wrapping_sub(100)));
            b0.extend((0..8).map(|_| rng.next_u32()));
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
        let p = random_program(seed, false);
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

#[test]
fn threaded_programs_match_per_thread() {
    for seed in 1..=150u64 {
        let p = random_program(seed, true);
        validate(&p).unwrap();
        let mut rng = Rng(seed * 31);
        let mut b0: Vec<u32> = (0..4).map(|_| rng.float().to_bits()).collect();
        b0.extend((0..4).map(|_| rng.next_u32() % 40));
        b0.extend((0..4).map(|_| (rng.next_u32() % 200).wrapping_sub(100)));
        b0.extend((0..8).map(|_| rng.next_u32()));
        for tid in [0, 1, 7, 31, 63] {
            let r = check_t(
                &p,
                &[],
                &[b0.clone(), vec![0xAAAA_AAAA; 64 * 64], vec![0; 4]],
                tid,
            );
            assert!(r.is_ok(), "seed {seed} thread {tid}: {}", r.unwrap_err());
        }
    }
}

#[test]
fn dump_constant_pair() {
    let mut b = Builder::new(Stage::Vertex);
    for c in 0..4u8 {
        let p = b.input(0, c);
        b.output(OutKind::Position, c, p);
    }
    let vs = compile(&b.finish()).unwrap();
    let mut b = Builder::new(Stage::Fragment);
    for (c, v) in [0.4f32, 0.2, 0.1, 1.0].into_iter().enumerate() {
        let r = b.f32(v);
        b.output(OutKind::Color(0), c as u8, r);
    }
    let fs = compile(&b.finish()).unwrap();
    for (n, c) in [("vs", &vs), ("fs", &fs)] {
        std::println!("{n}: gprs {} header {:08x?}", c.gprs, c.header);
        for g in c.code.chunks(4) {
            std::println!("  sched {:016x}", g[0]);
            for w in &g[1..] {
                std::println!("    {w:016x}");
            }
        }
    }
}

/// How much a 1e-6 relative error in every transcendental result moves
/// the outputs of a random program: the hardware's MUFU is that far off.
#[test]
fn mufu_error_sensitivity_of_seed_10() {
    use core::sync::atomic::Ordering;
    let p = random_program(10, true);
    let mut rng = Rng(10 * 31);
    let mut b0: Vec<u32> = (0..4).map(|_| rng.float().to_bits()).collect();
    b0.extend((0..4).map(|_| rng.next_u32() % 40));
    b0.extend((0..4).map(|_| (rng.next_u32() % 200).wrapping_sub(100)));
    b0.extend((0..4).map(|_| rng.next_u32()));
    let run_with = |err: f32| {
        crate::interp::MUFU_ERROR_BITS.store(err.to_bits(), Ordering::Relaxed);
        let mut bufs = vec![Vec::new(); MAX_BUFFERS];
        bufs[0] = b0.clone();
        bufs[1] = vec![0xAAAA_AAAA; 64 * 64];
        bufs[2] = vec![0; 4];
        for tid in 0..64 {
            let mut m = Machine {
                inputs: [[0; 4]; MAX_ATTRS],
                builtins: [0; 11],
                uniforms: &[],
                buffers: &mut bufs,
                sampler: &mut NoTextures,
            };
            m.builtins[Builtin::LocalIdX as usize] = tid;
            run(&p, &mut m);
        }
        bufs.swap_remove(1)
    };
    let exact = run_with(0.0);
    let off = run_with(2e-6);
    crate::interp::MUFU_ERROR_BITS.store(0, Ordering::Relaxed);
    let worst = exact
        .iter()
        .zip(&off)
        .map(|(&a, &b)| {
            let (x, y) = (f32::from_bits(a), f32::from_bits(b));
            if x == y || (x.is_nan() && y.is_nan()) {
                0.0
            } else {
                (x - y).abs() / x.abs().max(y.abs()).max(1e-3)
            }
        })
        .fold(0.0f32, f32::max);
    std::println!("worst relative move of an output for a 2e-6 error: {worst}");
}

#[test]
fn local_memory_round_trip() {
    let mut b = Builder::new(Stage::Compute);
    b.local_words(16);
    let tid = b.builtin(Builtin::LocalIdX);
    let z = b.u32(0);
    // A stack: push tid-derived values, pop them in reverse.
    let sp = b.u32(0);
    let one = b.u32(1);
    b.loop_(6, |b| {
        let v = b.uadd(sp, tid);
        let v = b.umul(v, v);
        b.local_store(sp, v);
        let n = b.uadd(sp, one);
        b.mov(sp, n);
    });
    let acc = b.u32(0);
    b.loop_(6, |b| {
        let n = b.usub(sp, one);
        b.mov(sp, n);
        let v = b.local_load(sp, Ty::U32);
        let k = b.u32(31);
        let a = b.umul(acc, k);
        let a = b.uadd(a, v);
        b.mov(acc, a);
    });
    // Out of range: a load gives 0, a store does nothing.
    let far = b.u32(500);
    let x = b.local_load(far, Ty::U32);
    b.local_store(far, acc);
    let y = b.uadd(acc, x);
    b.store(0, z, 0, &[y]);
    let p = b.finish();
    for tid in [0, 3, 31] {
        let r = check_t(&p, &[], &[vec![0; 4]], tid);
        assert!(r.is_ok(), "thread {tid}: {}", r.unwrap_err());
    }
}

use std::vec;
use std::vec::Vec;

use super::demo::{camera_rays_from, valley};
use super::shadow::{self, u, BOXES, BOX_WORDS};
use super::*;
use crate::interp::{run, Machine, Sampler};
use crate::lower::{compile, CB_BUFFERS, CB_TEXTURES, CB_UNIFORMS};
use crate::sass::{SR_CTAID_X, SR_TID_X};
use crate::sassim::Sim;
use crate::{Builder, Builtin, OutKind, Stage, Ty, MAX_ATTRS, MAX_BUFFERS};

const W: usize = 40;
const H: usize = 30;

/// The position target of a pre-pass: a texel per camera ray.
struct Positions(Vec<[f32; 4]>);

impl Sampler for Positions {
    fn sample(&mut self, _: u8, u: f32, v: f32, _: Option<f32>) -> [f32; 4] {
        let (x, y) = ((u * W as f32) as usize, (v * H as f32) as usize);
        self.0[y.min(H - 1) * W + x.min(W - 1)]
    }
}

struct Scene {
    bvh: Bvh,
    positions: Vec<[f32; 4]>,
    uniforms: Vec<u32>,
    boxes: Vec<[f32; 18]>,
    sun: [f32; 3],
}

fn scene() -> Scene {
    let tris = valley(40, 60, 3);
    let bvh = build(&tris).unwrap();
    let cam = camera_rays_from(W, H, [0.0, 12.0, -34.0], 0.3);
    let positions: Vec<[f32; 4]> = cam
        .iter()
        .enumerate()
        .map(|(i, r)| match trace::trace(&bvh, r) {
            // Every seventh texel is "no ray needed".
            Some(h) if i % 7 != 3 => {
                let t = h.t - 0.03;
                [
                    r.origin[0] + r.dir[0] * t,
                    r.origin[1] + r.dir[1] * t,
                    r.origin[2] + r.dir[2] * t,
                    h.t,
                ]
            }
            _ => [0.0; 4],
        })
        .collect();
    let sun = {
        let v = [0.45f32, 0.7, 0.3];
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / l, v[1] / l, v[2] / l]
    };
    // Six boxes standing near the middle of the view, turned about y.
    let mut boxes = Vec::new();
    for k in 0..BOXES {
        let a = k as f32 * 0.7;
        let (s, c) = (a.sin(), a.cos());
        let origin = [-1.0 + k as f32 * 0.5, 0.5, 2.0 + (k % 2) as f32];
        boxes.push([
            c, 0.0, -s, 0.0, 1.0, 0.0, s, 0.0, c, origin[0], origin[1], origin[2], -0.4, 0.0, -0.3,
            0.4, 1.4, 0.3,
        ]);
    }
    let mut uniforms = vec![0u32; u::WORDS];
    uniforms[u::W as usize] = W as u32;
    uniforms[u::H as usize] = H as u32;
    for k in 0..3 {
        uniforms[u::SUN as usize + k] = sun[k].to_bits();
    }
    uniforms[u::T_MAX as usize] = 80.0f32.to_bits();
    uniforms[u::INV_W as usize] = (1.0 / W as f32).to_bits();
    uniforms[u::INV_H as usize] = (1.0 / H as f32).to_bits();
    for (k, v) in [0.0f32, 2.0, 2.5, 6.0].iter().enumerate() {
        uniforms[u::SPHERE as usize + k] = v.to_bits();
    }
    for (k, bx) in boxes.iter().enumerate() {
        for (j, v) in bx.iter().enumerate() {
            uniforms[u::BOXES as usize + k * BOX_WORDS as usize + j] = v.to_bits();
        }
    }
    Scene {
        bvh,
        positions,
        uniforms,
        boxes,
        sun,
    }
}

fn hits_box(b: &[f32; 18], o: [f32; 3], d: [f32; 3], t_min: f32, t_max: f32) -> bool {
    let rel = [o[0] - b[9], o[1] - b[10], o[2] - b[11]];
    let (mut tn, mut tf) = (t_min, t_max);
    for a in 0..3 {
        let ax = [b[3 * a], b[3 * a + 1], b[3 * a + 2]];
        let lp = rel[0] * ax[0] + rel[1] * ax[1] + rel[2] * ax[2];
        let ld = d[0] * ax[0] + d[1] * ax[1] + d[2] * ax[2];
        let ld = if ld.abs() < 1e-20 { 1e-20 } else { ld };
        let (t1, t2) = ((b[12 + a] - lp) / ld, (b[15 + a] - lp) / ld);
        tn = tn.max(t1.min(t2));
        tf = tf.min(t1.max(t2));
    }
    tn <= tf
}

/// What each texel should say: 0 for none, else the distance, negative in
/// shadow.
fn expected(sc: &Scene) -> Vec<f32> {
    sc.positions
        .iter()
        .map(|p| {
            if p[3] <= 0.0 {
                return 0.0;
            }
            let o = [p[0], p[1], p[2]];
            let ray = Ray {
                origin: o,
                t_min: shadow::T_MIN,
                dir: sc.sun,
                t_max: 80.0,
            };
            let blocked = trace::occluded(&sc.bvh, &ray)
                || sc
                    .boxes
                    .iter()
                    .any(|b| hits_box(b, o, sc.sun, shadow::T_MIN, 80.0));
            if blocked {
                -p[3]
            } else {
                p[3]
            }
        })
        .collect()
}

fn agree(got: &[u32], want: &[f32]) -> usize {
    got.iter()
        .zip(want)
        .filter(|(g, w)| {
            let g = f32::from_bits(**g);
            (g - **w).abs() > 1e-3 * w.abs().max(1.0)
        })
        .count()
}

#[test]
fn the_shadow_kernel_matches_the_cpu() {
    let sc = scene();
    let want = expected(&sc);
    let p = shadow::kernel(shadow::Slots::default());
    let mut bufs = vec![Vec::new(); MAX_BUFFERS];
    bufs[0] = vec![0xAAAA_AAAA; W * H];
    bufs[2] = sc.bvh.nodes.clone();
    bufs[3] = sc.bvh.tris.clone();
    let mut tex = Positions(sc.positions.clone());
    let groups = (W.div_ceil(8), H.div_ceil(4));
    for gy in 0..groups.1 {
        for gx in 0..groups.0 {
            for ly in 0..4 {
                for lx in 0..8 {
                    let mut m = Machine {
                        inputs: [[0; 4]; MAX_ATTRS],
                        builtins: [0; 11],
                        uniforms: &sc.uniforms,
                        buffers: &mut bufs,
                        sampler: &mut tex,
                    };
                    m.builtins[Builtin::LocalIdX as usize] = lx;
                    m.builtins[Builtin::LocalIdY as usize] = ly;
                    m.builtins[Builtin::GroupIdX as usize] = gx as u32;
                    m.builtins[Builtin::GroupIdY as usize] = gy as u32;
                    run(&p, &mut m);
                }
            }
        }
    }
    let shadowed = want.iter().filter(|w| **w < 0.0).count();
    let lit = want.iter().filter(|w| **w > 0.0).count();
    let wrong = agree(&bufs[0], &want);
    std::println!("shadow kernel: {lit} lit, {shadowed} shadowed texels, {wrong} differ");
    assert!(shadowed > 20 && lit > 100, "a scene with both");
    assert!(wrong <= W * H / 100, "{wrong} texels differ");
}

#[test]
fn the_compiled_shadow_kernel_matches_the_cpu() {
    let sc = scene();
    let want = expected(&sc);
    let p = shadow::kernel(shadow::Slots::default());
    let c = compile(&p).unwrap();
    let base = |i: usize| 0x2_0000_0000u64 + 0x100_0000 * i as u64;
    let mut out = vec![0xAAAA_AAAAu32; W * H];
    for gy in 0..H.div_ceil(4) {
        for gx in 0..W.div_ceil(8) {
            for ly in 0..4u32 {
                for lx in 0..8u32 {
                    let mut tex = Positions(sc.positions.clone());
                    let mut s = Sim::new(&c.code, &mut tex);
                    s.cbuf[CB_UNIFORMS as usize] = sc.uniforms.clone();
                    let bufs = [
                        out.clone(),
                        vec![],
                        sc.bvh.nodes.clone(),
                        sc.bvh.tris.clone(),
                    ];
                    let mut desc = Vec::new();
                    for (i, b) in bufs.iter().enumerate() {
                        desc.extend([base(i) as u32, (base(i) >> 32) as u32, b.len() as u32, 0]);
                    }
                    s.cbuf[CB_BUFFERS as usize] = desc;
                    s.cbuf[CB_TEXTURES as usize] = (0..8u32).collect();
                    for (i, b) in bufs.iter().enumerate() {
                        s.mem.push((base(i), b.clone()));
                    }
                    s.sregs.insert(SR_TID_X, lx);
                    s.sregs.insert(SR_TID_X + 1, ly);
                    s.sregs.insert(SR_TID_X + 2, 0);
                    s.sregs.insert(SR_CTAID_X, gx as u32);
                    s.sregs.insert(SR_CTAID_X + 1, gy as u32);
                    s.sregs.insert(SR_CTAID_X + 2, 0);
                    s.regs[255] = 0;
                    s.local = vec![0xDEAD_BEEF; p.local_words as usize];
                    s.run().unwrap();
                    let (x, y) = (gx * 8 + lx as usize, gy * 4 + ly as usize);
                    if x < W && y < H {
                        out[y * W + x] = s.mem[0].1[y * W + x];
                    }
                }
            }
        }
    }
    let wrong = agree(&out, &want);
    std::println!(
        "compiled shadow kernel: {} registers, {} differ of {}",
        c.gprs,
        wrong,
        W * H
    );
    assert!(wrong <= W * H / 100, "{wrong} texels differ");
}

/// `lit_factor` on a mask, for a pixel at (px, py) with distance `dist`.
fn factor(mask: &[f32], w: u32, h: u32, px: f32, py: f32, dist: f32) -> f32 {
    let mut b = Builder::new(Stage::Fragment);
    let (px_r, py_r, d_r) = (
        b.uniform(0, Ty::F32),
        b.uniform(1, Ty::F32),
        b.uniform(2, Ty::F32),
    );
    let low = [b.uniform(3, Ty::F32), b.uniform(4, Ty::F32)];
    let (k, slack) = (b.f32(0.04), b.f32(0.15));
    let tol = b.fma(d_r, k, slack);
    let f = shadow::lit_factor(&mut b, 0, px_r, py_r, d_r, tol, low);
    b.output(OutKind::Color(0), 0, f);
    let p = b.finish();
    let mut bufs = vec![Vec::new(); MAX_BUFFERS];
    bufs[0] = mask.iter().map(|v| v.to_bits()).collect();
    let uniforms = [px, py, dist, w as f32, h as f32].map(f32::to_bits);
    let mut tex = Positions(vec![]);
    let mut m = Machine {
        inputs: [[0; 4]; MAX_ATTRS],
        builtins: [0; 11],
        uniforms: &uniforms,
        buffers: &mut bufs,
        sampler: &mut tex,
    };
    run(&p, &mut m).colors[0][0]
}

#[test]
fn the_mask_is_filtered_by_distance() {
    let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
    // 4 x 3 mask, every texel at distance 10.
    let lit = vec![10.0f32; 12];
    let dark = vec![-10.0f32; 12];
    assert!(near(factor(&lit, 4, 3, 1.3, 1.7, 10.0), 1.0));
    assert!(near(factor(&dark, 4, 3, 1.3, 1.7, 10.0), 0.0));
    // Lit on the left (x <= 1), in shadow on the right.
    let mut half = vec![10.0f32; 12];
    for y in 0..3 {
        for x in 2..4 {
            half[y * 4 + x] = -10.0;
        }
    }
    assert!(near(factor(&half, 4, 3, 1.0, 1.0, 10.0), 1.0));
    assert!(near(factor(&half, 4, 3, 1.5, 1.0, 10.0), 0.5));
    assert!(near(factor(&half, 4, 3, 2.0, 1.0, 10.0), 0.0));
    // Past the mask's edge the nearest texel counts.
    assert!(near(factor(&half, 4, 3, -0.4, 1.0, 10.0), 1.0));
    assert!(near(factor(&half, 4, 3, 3.4, 2.4, 10.0), 0.0));
    // A texel far in depth is ignored: the lit one at 10 decides.
    let mut edge = vec![10.0f32; 12];
    edge[5] = -40.0;
    assert!(near(factor(&edge, 4, 3, 1.0, 1.0, 10.0), 1.0));
    assert!(near(factor(&edge, 4, 3, 1.5, 1.0, 10.0), 1.0));
    // No usable texel: lit; only far ones: the nearest in depth.
    assert!(near(factor(&vec![0.0; 12], 4, 3, 1.5, 1.5, 10.0), 1.0));
    assert!(near(factor(&vec![-40.0; 12], 4, 3, 1.5, 1.5, 10.0), 0.0));
}

/// `lit_factor` compiled, on the simulator, against the reference machine.
#[test]
fn the_compiled_mask_filter_matches_the_reference() {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(64, 1, 1);
    let (px, py, d) = (
        b.uniform(0, Ty::F32),
        b.uniform(1, Ty::F32),
        b.uniform(2, Ty::F32),
    );
    let low = [b.uniform(3, Ty::F32), b.uniform(4, Ty::F32)];
    let (k, slack) = (b.f32(0.04), b.f32(0.15));
    let tol = b.fma(d, k, slack);
    let f = shadow::lit_factor(&mut b, 0, px, py, d, tol, low);
    let zero = b.u32(0);
    let bits = b.fbits(f);
    b.store(1, zero, 0, &[bits]);
    let p = b.finish();
    // A mask of 8 x 6 with a shadow edge and a depth edge in it.
    let mut mask = vec![10.0f32; 48];
    for y in 0..6 {
        for x in 3..8 {
            mask[y * 8 + x] = -10.0;
        }
    }
    mask[9] = -40.0;
    mask[20] = 0.0;
    let bufs = vec![
        mask.iter().map(|v| v.to_bits()).collect::<Vec<u32>>(),
        vec![0u32; 4],
    ];
    let mut worst = 0.0f32;
    for (px, py, dist) in [
        (2.5f32, 2.0f32, 10.0f32),
        (1.2, 1.7, 10.0),
        (3.0, 3.5, 10.0),
        (6.4, 4.4, 10.0),
        (0.2, 0.3, 10.0),
        (1.5, 1.5, 11.0),
        (4.5, 2.5, 30.0),
        (-0.5, 5.5, 10.0),
        (7.9, 5.9, 10.0),
    ] {
        let uniforms = [px, py, dist, 8.0, 6.0].map(f32::to_bits);
        let (r, s) = crate::lower_tests::both_t(&p, &uniforms, &bufs, 0).unwrap();
        let (a, c) = (f32::from_bits(r[1][0]), f32::from_bits(s[1][0]));
        std::println!("({px}, {py}, {dist}): reference {a}, compiled {c}");
        worst = worst.max((a - c).abs());
    }
    assert!(worst < 1e-3, "the compiled filter differs by {worst}");
}

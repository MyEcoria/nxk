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
const HALF: f32 = 0.577_350_3;

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
    tris: Vec<Triangle>,
    boxes: Vec<[f32; 18]>,
    sun: [f32; 3],
}

/// The view-projection that maps a point to the texel grid the way
/// `camera_rays_from` shoots it (rows, as the kernel reads them).
fn view_projection(eye: [f32; 3], pitch: f32) -> [[f32; 4]; 4] {
    let (sp, cp) = (pitch.sin(), pitch.cos());
    let up = [0.0, cp, sp];
    let fwd = [0.0, -sp, cp];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    [
        [1.0 / HALF, 0.0, 0.0, -eye[0] / HALF],
        // Window y grows downward with NDC y, as the console's viewport has it.
        [0.0, -up[1] / HALF, -up[2] / HALF, dot(eye, up) / HALF],
        [0.0, 0.0, 0.0, 0.0],
        [0.0, fwd[1], fwd[2], -dot(eye, fwd)],
    ]
}

/// What `pack_normal` stores, and what the kernel reads back from it.
fn quantize(n: [f32; 3]) -> ([f32; 3], f32) {
    let e = |v: f32| ((v * 0.5 + 0.5).clamp(0.0, 1.0) * 2047.0 + 0.5) as u32;
    let (ex, ey) = (e(n[0]), e(n[1]));
    let sign = (n[2] < 0.0) as u32;
    let packed = (sign << 22 | ex << 11 | ey) as f32 + 1.0;
    let (x, y) = (
        ex as f32 / 2047.0 * 2.0 - 1.0,
        ey as f32 / 2047.0 * 2.0 - 1.0,
    );
    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
    ([x, y, if sign == 1 { -z } else { z }], packed)
}

/// The pre-pass's texels: the surface point, the normal packed in w
/// (0: no ray needed), and the normals as the kernel will read them.
fn positions(sc: &Scene, eye: [f32; 3], pitch: f32) -> (Vec<[f32; 4]>, Vec<[f32; 3]>) {
    let mut normals = Vec::new();
    let pos = camera_rays_from(W, H, eye, pitch)
        .iter()
        .enumerate()
        .map(|(i, r)| match trace::trace(&sc.bvh, r) {
            // Every seventh texel is "no ray needed".
            Some(h) if i % 7 != 3 => {
                let v = sc.tris[h.id as usize].v;
                let e1 = [v[1][0] - v[0][0], v[1][1] - v[0][1], v[1][2] - v[0][2]];
                let e2 = [v[2][0] - v[0][0], v[2][1] - v[0][1], v[2][2] - v[0][2]];
                let mut n = [
                    e1[1] * e2[2] - e1[2] * e2[1],
                    e1[2] * e2[0] - e1[0] * e2[2],
                    e1[0] * e2[1] - e1[1] * e2[0],
                ];
                let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                let side = n[0] * r.dir[0] + n[1] * r.dir[1] + n[2] * r.dir[2];
                let f = if side > 0.0 { -1.0 } else { 1.0 } / l;
                n = [n[0] * f, n[1] * f, n[2] * f];
                let (nq, packed) = quantize(n);
                normals.push(nq);
                [
                    r.origin[0] + r.dir[0] * h.t,
                    r.origin[1] + r.dir[1] * h.t,
                    r.origin[2] + r.dir[2] * h.t,
                    packed,
                ]
            }
            _ => {
                normals.push([0.0, 1.0, 0.0]);
                [0.0; 4]
            }
        })
        .collect();
    (pos, normals)
}

fn scene() -> Scene {
    let tris = valley(40, 60, 3);
    let bvh = build(&tris).unwrap();
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
    Scene {
        bvh,
        tris,
        boxes,
        sun,
    }
}

/// The uniforms of a frame: `prev` the camera of the last one, `frame`
/// its number, no jitter unless `jitter`.
fn uniforms(
    sc: &Scene,
    cur: [f32; 3],
    prev: ([f32; 3], f32),
    frame: u32,
    jitter: f32,
    reset: bool,
) -> Vec<u32> {
    let mut un = vec![0u32; u::WORDS];
    un[u::W as usize] = W as u32;
    un[u::H as usize] = H as u32;
    let mut set = |at: u16, v: &[f32]| {
        for (k, x) in v.iter().enumerate() {
            un[at as usize + k] = x.to_bits();
        }
    };
    set(u::SUN, &sc.sun);
    set(u::T_MAX, &[80.0]);
    set(u::INV_W, &[1.0 / W as f32, 1.0 / H as f32]);
    set(u::SPHERE, &[0.0, 2.0, 2.5, 6.0]);
    for (k, bx) in sc.boxes.iter().enumerate() {
        set(u::BOXES + (k as u16) * BOX_WORDS, bx);
    }
    let vp = view_projection(prev.0, prev.1);
    for (r, row) in vp.iter().enumerate() {
        set(u::PREV_VP + 4 * r as u16, row);
    }
    set(u::PREV_CAM, &prev.0);
    set(u::CAM, &cur);
    set(u::ALPHA, &[0.15]);
    set(u::JITTER, &[jitter]);
    set(u::RESET, &[if reset { 1.0 } else { 0.0 }]);
    // Two unit vectors perpendicular to the sun.
    let s = sc.sun;
    let a = if s[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let cross = |p: [f32; 3], q: [f32; 3]| {
        [
            p[1] * q[2] - p[2] * q[1],
            p[2] * q[0] - p[0] * q[2],
            p[0] * q[1] - p[1] * q[0],
        ]
    };
    let norm = |v: [f32; 3]| {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / l, v[1] / l, v[2] / l]
    };
    let su = norm(cross(s, a));
    let sv = norm(cross(s, su));
    set(u::SUN_U, &su);
    set(u::SUN_V, &sv);
    set(u::PREV_SPHERE, &[1.0e6, 0.0, 0.0, 0.1]);
    un[u::FRAME as usize] = frame;
    un
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

/// What each texel should say without jitter: (distance, 1 lit or 0
/// shadowed); (0, 1) for none.
fn expected(sc: &Scene, cam: [f32; 3], pos: &[[f32; 4]], normals: &[[f32; 3]]) -> Vec<(f32, f32)> {
    pos.iter()
        .zip(normals)
        .map(|(p, n)| {
            if p[3] <= 0.0 {
                return (0.0, 1.0);
            }
            let dist =
                ((p[0] - cam[0]).powi(2) + (p[1] - cam[1]).powi(2) + (p[2] - cam[2]).powi(2))
                    .sqrt();
            let lit_n = (n[0] * sc.sun[0] + n[1] * sc.sun[1] + n[2] * sc.sun[2]).clamp(0.0, 1.0);
            let bias = shadow::BIAS + shadow::BIAS_SLOPE * (1.0 - lit_n);
            let o = [p[0] + n[0] * bias, p[1] + n[1] * bias, p[2] + n[2] * bias];
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
            (dist, if blocked { 0.0 } else { 1.0 })
        })
        .collect()
}

/// How many texels of `got` (two words each) differ from `want` by more
/// than `tol` in the share, or in the distance.
fn differ(got: &[u32], want: &[(f32, f32)], tol: f32) -> usize {
    want.iter()
        .enumerate()
        .filter(|(i, w)| {
            let (d, c) = (f32::from_bits(got[2 * i]), f32::from_bits(got[2 * i + 1]));
            (d - w.0).abs() > 1e-3 * w.0.max(1.0) || (c - w.1).abs() > tol
        })
        .count()
}

/// One frame of the kernel on the reference machine: its mask.
fn frame(sc: &Scene, pos: &[[f32; 4]], hist: &[u32], un: &[u32]) -> Vec<u32> {
    let p = shadow::kernel(shadow::Slots::default());
    let mut bufs = vec![Vec::new(); MAX_BUFFERS];
    bufs[0] = vec![0xAAAA_AAAA; 2 * W * H];
    bufs[1] = hist.to_vec();
    bufs[2] = sc.bvh.nodes.clone();
    bufs[3] = sc.bvh.tris.clone();
    let mut tex = Positions(pos.to_vec());
    for gy in 0..H.div_ceil(4) {
        for gx in 0..W.div_ceil(8) {
            for ly in 0..4 {
                for lx in 0..8 {
                    let mut m = Machine {
                        inputs: [[0; 4]; MAX_ATTRS],
                        builtins: [0; 11],
                        uniforms: un,
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
    bufs.swap_remove(0)
}

/// The distance of each texel's point from `cam`; 0 for none.
fn dists(pos: &[[f32; 4]], cam: [f32; 3]) -> Vec<f32> {
    pos.iter()
        .map(|p| {
            if p[3] <= 0.0 {
                return 0.0;
            }
            ((p[0] - cam[0]).powi(2) + (p[1] - cam[1]).powi(2) + (p[2] - cam[2]).powi(2)).sqrt()
        })
        .collect()
}

const EYE: [f32; 3] = [0.0, 12.0, -34.0];
const PITCH: f32 = 0.3;

#[test]
fn the_shadow_kernel_matches_the_cpu() {
    let sc = scene();
    let (pos, normals) = positions(&sc, EYE, PITCH);
    let want = expected(&sc, EYE, &pos, &normals);
    // A first frame: no history, so every texel traces.
    let un = uniforms(&sc, EYE, (EYE, PITCH), 0, 0.0, false);
    let out = frame(&sc, &pos, &vec![0; 2 * W * H], &un);
    let shadowed = want.iter().filter(|w| w.0 > 0.0 && w.1 == 0.0).count();
    let lit = want.iter().filter(|w| w.0 > 0.0 && w.1 == 1.0).count();
    let wrong = differ(&out, &want, 1e-3);
    std::println!("shadow kernel: {lit} lit, {shadowed} shadowed texels, {wrong} differ");
    assert!(shadowed > 20 && lit > 100, "a scene with both");
    assert!(wrong <= W * H / 100, "{wrong} texels differ");
}

#[test]
fn history_is_found_where_the_point_was_and_half_the_texels_keep_it() {
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    // The last frame said 0.5 everywhere, with the right distances.
    let hist: Vec<u32> = dists(&pos, EYE)
        .iter()
        .flat_map(|d| [d.to_bits(), 0.5f32.to_bits()])
        .collect();
    let un = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.0, false);
    let out = frame(&sc, &pos, &hist, &un);
    let needed = pos.iter().filter(|p| p[3] > 0.0).count();
    let kept = (0..W * H)
        .filter(|&i| pos[i][3] > 0.0 && f32::from_bits(out[2 * i + 1]) == 0.5)
        .count();
    std::println!("history: {kept} of {needed} texels kept their 0.5");
    // Half the texels do not trace; the history of the others is blended
    // toward lit or shadow (0.575 or 0.425), the sphere's neighbours faster.
    assert!(
        kept > needed * 4 / 10 && kept < needed * 6 / 10,
        "{kept} of {needed}"
    );
    // Forgetting history: all trace, none keeps 0.5.
    let un = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.0, true);
    let out = frame(&sc, &pos, &hist, &un);
    let kept = (0..W * H)
        .filter(|&i| pos[i][3] > 0.0 && f32::from_bits(out[2 * i + 1]) == 0.5)
        .count();
    assert_eq!(kept, 0);
}

#[test]
fn a_moving_camera_keeps_the_picture_right() {
    let sc = scene();
    let (eye0, eye1) = (EYE, [0.6, 12.2, -33.4]);
    let (pos0, _) = positions(&sc, eye0, PITCH);
    let un0 = uniforms(&sc, eye0, (eye0, PITCH), 0, 0.0, false);
    let first = frame(&sc, &pos0, &vec![0; 2 * W * H], &un0);
    let (pos1, normals1) = positions(&sc, eye1, PITCH);
    let want = expected(&sc, eye1, &pos1, &normals1);
    // Two frames on, the camera has moved: the history came through the
    // projection of the last frame's camera and must still give the truth
    // within a few texels at the edges.
    let un1 = uniforms(&sc, eye1, (eye0, PITCH), 1, 0.0, false);
    let second = frame(&sc, &pos1, &first, &un1);
    let wrong = differ(&second, &want, 0.3);
    std::println!("moving camera: {wrong} of {} texels off", W * H);
    assert!(wrong <= W * H / 20, "{wrong} texels differ");
}

#[test]
fn the_jitter_makes_penumbrae() {
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    let mut hist = vec![0u32; 2 * W * H];
    let mut cams = (EYE, PITCH);
    for f in 0..24 {
        let un = uniforms(&sc, EYE, cams, f, 0.03, false);
        hist = frame(&sc, &pos, &hist, &un);
        cams = (EYE, PITCH);
    }
    let soft = (0..W * H)
        .filter(|&i| {
            let c = f32::from_bits(hist[2 * i + 1]);
            pos[i][3] > 0.0 && c > 0.1 && c < 0.9
        })
        .count();
    std::println!("penumbra texels after 24 frames: {soft}");
    assert!(soft > 3, "{soft}");
}

#[test]
fn the_compiled_shadow_kernel_matches_the_reference() {
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    let hist: Vec<u32> = dists(&pos, EYE)
        .iter()
        .enumerate()
        .flat_map(|(i, d)| [d.to_bits(), ((i % 3) as f32 * 0.5).to_bits()])
        .collect();
    let un = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.02, false);
    let want = frame(&sc, &pos, &hist, &un);
    let p = shadow::kernel(shadow::Slots::default());
    let c = compile(&p).unwrap();
    let base = |i: usize| 0x2_0000_0000u64 + 0x100_0000 * i as u64;
    let mut out = vec![0xAAAA_AAAAu32; 2 * W * H];
    for gy in 0..H.div_ceil(4) {
        for gx in 0..W.div_ceil(8) {
            for ly in 0..4u32 {
                for lx in 0..8u32 {
                    let mut tex = Positions(pos.clone());
                    let mut s = Sim::new(&c.code, &mut tex);
                    s.cbuf[CB_UNIFORMS as usize] = un.clone();
                    let bufs = [
                        out.clone(),
                        hist.clone(),
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
                        let i = 2 * (y * W + x);
                        out[i] = s.mem[0].1[i];
                        out[i + 1] = s.mem[0].1[i + 1];
                    }
                }
            }
        }
    }
    let wrong = (0..W * H)
        .filter(|&i| {
            let (a, b) = (
                f32::from_bits(out[2 * i + 1]),
                f32::from_bits(want[2 * i + 1]),
            );
            (a - b).abs() > 0.05 || out[2 * i] != want[2 * i]
        })
        .count();
    std::println!(
        "compiled shadow kernel: {} registers, {} differ of {}",
        c.gprs,
        wrong,
        W * H
    );
    assert!(wrong <= W * H / 50, "{wrong} texels differ");
}

/// `lit_factor` on a mask given as signed distances (negative: in
/// shadow, 0: none), for a pixel at (px, py) with distance `dist`.
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
    bufs[0] = records(mask);
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

/// The mask's records from signed distances.
fn records(mask: &[f32]) -> Vec<u32> {
    mask.iter()
        .flat_map(|v| {
            let cov = if *v < 0.0 { 0.0f32 } else { 1.0 };
            [v.abs().to_bits(), cov.to_bits()]
        })
        .collect()
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
    let bufs = vec![records(&mask), vec![0u32; 4]];
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
        worst = worst.max((a - c).abs());
    }
    assert!(worst < 1e-3, "the compiled filter differs by {worst}");
}

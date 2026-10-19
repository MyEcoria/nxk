use std::vec;
use std::vec::Vec;

use super::shadow::{self, u};
use super::*;
use crate::interp::{run, Machine};
use crate::lower::{compile, CB_BUFFERS, CB_TEXTURES, CB_UNIFORMS};
use crate::sass::{SR_CTAID_X, SR_TID_X};
use crate::sassim::Sim;
use crate::{Builder, OutKind, Stage, Ty, MAX_ATTRS, MAX_BUFFERS};

const W: usize = 40;
const H: usize = 30;
const TW: usize = shadow::TEXEL_WORDS;

use super::shadow_demo::{Positions, Scene};

fn scene() -> Scene {
    super::shadow_demo::scene()
}

fn positions(sc: &Scene, eye: [f32; 3], pitch: f32) -> (Vec<[f32; 4]>, Vec<[f32; 3]>) {
    let (p, n) = super::shadow_demo::positions(sc, W, H, eye, pitch);
    (p.texels, n)
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
    let f = super::shadow_demo::Frame {
        eye: cur,
        prev,
        number: frame,
        jitter,
        reset,
    };
    super::shadow_demo::uniforms(sc, W, H, &f)
}

fn texels(pos: &[[f32; 4]]) -> Positions {
    Positions {
        w: W,
        h: H,
        texels: pos.to_vec(),
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
                || sc.boxes.iter().any(|b| {
                    let f = shadow::FIGURE_BIAS;
                    let fo = [p[0] + n[0] * f, p[1] + n[1] * f, p[2] + n[2] * f];
                    hits_box(b, fo, sc.sun, shadow::FIGURE_T_MIN, 80.0)
                });
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
            let (d, c) = (f32::from_bits(got[TW * i]), f32::from_bits(got[TW * i + 1]));
            (d - w.0).abs() > 1e-3 * w.0.max(1.0) || (c - w.1).abs() > tol
        })
        .count()
}

/// One frame of the kernel on the reference machine: its mask.
fn frame(sc: &Scene, pos: &[[f32; 4]], hist: &[u32], un: &[u32]) -> Vec<u32> {
    super::shadow_demo::reference(sc, &mut texels(pos), hist, un)
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
    let out = frame(&sc, &pos, &vec![0; TW * W * H], &un);
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
    // The last frame said 0.5 everywhere, with the right distances, and
    // every texel had traced.
    let hist_with = |traced: f32| -> Vec<u32> {
        dists(&pos, EYE)
            .iter()
            .flat_map(|d| {
                [
                    d.to_bits(),
                    0.5f32.to_bits(),
                    0.5f32.to_bits(),
                    traced.to_bits(),
                ]
            })
            .collect()
    };
    let hist = hist_with(1.0);
    let un = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.0, false);
    let needed = pos.iter().filter(|p| p[3] > 0.0).count();
    // The scene's share kept as it was.
    let kept = |out: &[u32]| {
        (0..W * H)
            .filter(|&i| pos[i][3] > 0.0 && f32::from_bits(out[TW * i + 2]) == 0.5)
            .count()
    };
    let out = frame(&sc, &pos, &hist, &un);
    let k = kept(&out);
    std::println!("history: {k} of {needed} texels kept their 0.5");
    // Half the texels do not trace; the history of the others is blended
    // toward lit or shadow (0.575 or 0.425).
    assert!(
        k > needed * 4 / 10 && k < needed * 6 / 10,
        "{k} of {needed}"
    );
    // Forgetting history, or history whose points did not trace: all trace.
    let un_reset = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.0, true);
    assert_eq!(kept(&frame(&sc, &pos, &hist, &un_reset)), 0);
    assert_eq!(kept(&frame(&sc, &pos, &hist_with(0.0), &un)), 0);
}

#[test]
fn a_moving_camera_keeps_the_picture_right() {
    let sc = scene();
    let (eye0, eye1) = (EYE, [0.6, 12.2, -33.4]);
    let (pos0, _) = positions(&sc, eye0, PITCH);
    let un0 = uniforms(&sc, eye0, (eye0, PITCH), 0, 0.0, false);
    let first = frame(&sc, &pos0, &vec![0; TW * W * H], &un0);
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

/// The camera pans one texel a frame: a point keeps the parity of its
/// texel, so the checkerboard alone would never trace half of them. Over
/// two frames every point must trace.
#[test]
fn a_point_moving_a_texel_a_frame_still_traces() {
    use super::shadow_demo as sd;
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    let d = dists(&pos, EYE);
    // Last frame's view one texel to the right: the point of texel x was
    // at x + 1, where the history holds its distance.
    let shifted = |number: u32| {
        let mut un = uniforms(&sc, EYE, (EYE, PITCH), number, 0.0, false);
        let vp = sd::view_projection(EYE, PITCH);
        for c in 0..4 {
            let v = vp[0][c] + vp[3][c] * 2.0 / W as f32;
            un[(u::PREV_VP + c as u16) as usize] = v.to_bits();
        }
        un
    };
    let hist: Vec<u32> = (0..W * H)
        .flat_map(|i| {
            let from = if i % W == 0 { 0.0 } else { d[i - 1] };
            [
                from.to_bits(),
                1.0f32.to_bits(),
                1.0f32.to_bits(),
                1.0f32.to_bits(),
            ]
        })
        .collect();
    let first = frame(&sc, &pos, &hist, &shifted(0));
    // The second frame reads the first's mask one texel to the right too.
    let second = frame(&sc, &pos, &first, &shifted(1));
    let traced = |m: &[u32], i: usize| f32::from_bits(m[TW * i + 3]) > 0.5;
    let missed = (0..W * H)
        .filter(|&i| {
            let x = i % W;
            pos[i][3] > 0.0 && x + 1 < W && !traced(&second, i) && !traced(&first, i + 1)
        })
        .count();
    std::println!("one texel a frame: {missed} points never traced");
    assert_eq!(missed, 0);
}

/// A box a few centimetres from a lit surface, toward the sun, shades it
/// (an arm over the body): the boxes' rays start just off the surface.
#[test]
fn a_box_just_above_a_surface_shades_it() {
    use super::shadow_demo as sd;
    let mut sc = scene();
    let p = [0.0f32, 20.0, 10.0];
    let (n, packed) = sd::quantize([0.0, 1.0, 0.0]);
    let sun = sc.sun;
    // A 0.2 m cube whose centre is 0.18 m from the point along the sun.
    let c = [
        p[0] + sun[0] * 0.18,
        p[1] + sun[1] * 0.18,
        p[2] + sun[2] * 0.18,
    ];
    sc.boxes = vec![[
        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, c[0], c[1], c[2], -0.1, -0.1, -0.1, 0.1, 0.1,
        0.1,
    ]];
    while sc.boxes.len() < shadow::BOXES {
        sc.boxes.push([
            1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 900.0, 900.0, 900.0, 0.0, 0.0, 0.0, 0.1,
            0.1, 0.1,
        ]);
    }
    let _ = n;
    let (w, h) = (8, 4);
    let mut pos = Positions {
        w,
        h,
        texels: vec![[p[0], p[1], p[2], packed]; w * h],
    };
    let eye = [0.0, 23.0, 0.0];
    let f = sd::Frame {
        eye,
        prev: (eye, PITCH),
        number: 0,
        jitter: 0.0,
        reset: true,
    };
    let mut un = sd::uniforms(&sc, w, h, &f);
    for (k, v) in [p[0], p[1], p[2], 2.0].iter().enumerate() {
        un[u::SPHERE as usize + k] = v.to_bits();
    }
    // Under the valley's own ground the scene's ray could be blocked too:
    // read the figure's share alone (the share over the scene's).
    let out = sd::reference(&sc, &mut pos, &vec![0; TW * w * h], &un);
    let (share, scene) = (f32::from_bits(out[1]), f32::from_bits(out[2]));
    let figure = if scene > 0.0 { share / scene } else { 0.0 };
    std::println!("box just above: share {share}, scene {scene}");
    assert!(scene > 0.0 && figure < 0.01, "share {share}, scene {scene}");
}

#[test]
fn the_jitter_makes_penumbrae() {
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    let mut hist = vec![0u32; TW * W * H];
    let mut cams = (EYE, PITCH);
    for f in 0..24 {
        let un = uniforms(&sc, EYE, cams, f, 0.03, false);
        hist = frame(&sc, &pos, &hist, &un);
        cams = (EYE, PITCH);
    }
    let soft = (0..W * H)
        .filter(|&i| {
            let c = f32::from_bits(hist[TW * i + 1]);
            pos[i][3] > 0.0 && c > 0.1 && c < 0.9
        })
        .count();
    std::println!("penumbra texels after 24 frames: {soft}");
    assert!(soft > 3, "{soft}");
}

/// One frame of the compiled kernel on the simulator, a thread at a time.
fn on_sim(sc: &Scene, pos: &Positions, hist: &[u32], un: &[u32]) -> Vec<u32> {
    let (w, h) = (pos.w, pos.h);
    let p = shadow::kernel(shadow::Slots::default());
    let c = compile(&p).unwrap();
    let base = |i: usize| 0x2_0000_0000u64 + 0x100_0000 * i as u64;
    let mut out = vec![0xAAAA_AAAAu32; TW * w * h];
    for gy in 0..h.div_ceil(4) {
        for gx in 0..w.div_ceil(8) {
            for ly in 0..4u32 {
                for lx in 0..8u32 {
                    let (x, y) = (gx * 8 + lx as usize, gy * 4 + ly as usize);
                    if x >= w || y >= h {
                        continue;
                    }
                    let mut tex = Positions {
                        w,
                        h,
                        texels: pos.texels.clone(),
                    };
                    let mut s = Sim::new(&c.code, &mut tex);
                    s.cbuf[CB_UNIFORMS as usize] = un.to_vec();
                    let bufs = [
                        out.clone(),
                        hist.to_vec(),
                        sc.bvh.nodes.clone(),
                        sc.bvh.tris.clone(),
                    ];
                    let mut desc = Vec::new();
                    for (i, b) in bufs.iter().enumerate() {
                        desc.extend([base(i) as u32, (base(i) >> 32) as u32, b.len() as u32, 0]);
                    }
                    s.cbuf[CB_BUFFERS as usize] = desc;
                    s.cbuf[CB_TEXTURES as usize] = (0..8u32).collect();
                    for (i, b) in bufs.into_iter().enumerate() {
                        s.mem.push((base(i), b));
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
                    let i = TW * (y * w + x);
                    out[i..i + TW].copy_from_slice(&s.mem[0].1[i..i + TW]);
                }
            }
        }
    }
    out
}

/// Texels whose share differs by more than 0.05, or whose distance is not
/// the same.
fn mismatches(got: &[u32], want: &[u32]) -> Vec<usize> {
    (0..got.len() / TW)
        .filter(|&i| {
            let (a, b) = (
                f32::from_bits(got[TW * i + 1]),
                f32::from_bits(want[TW * i + 1]),
            );
            (a - b).abs() > 0.05 || got[TW * i] != want[TW * i]
        })
        .collect()
}

#[test]
fn the_compiled_shadow_kernel_matches_the_reference() {
    let sc = scene();
    let (pos, _) = positions(&sc, EYE, PITCH);
    let hist: Vec<u32> = dists(&pos, EYE)
        .iter()
        .enumerate()
        .flat_map(|(i, d)| {
            let c = ((i % 3) as f32 * 0.5).to_bits();
            [d.to_bits(), c, c, ((i % 5 != 0) as u32 as f32).to_bits()]
        })
        .collect();
    let un = uniforms(&sc, EYE, (EYE, PITCH), 1, 0.02, false);
    let want = frame(&sc, &pos, &hist, &un);
    let out = on_sim(&sc, &texels(&pos), &hist, &un);
    let wrong = mismatches(&out, &want).len();
    std::println!("compiled shadow kernel: {} differ of {}", wrong, W * H);
    assert!(wrong <= W * H / 50, "{wrong} texels differ");
}

/// The scene `gpu test` runs on the console (80 x 60, no jitter): the
/// simulator must agree with the reference.
#[test]
fn the_consoles_scene_on_the_simulator() {
    use super::shadow_demo as sd;
    let (w, h) = (80, 60);
    let sc = scene();
    let (mut pos, _) = sd::positions(&sc, w, h, EYE, PITCH);
    let f = sd::Frame {
        eye: EYE,
        prev: (EYE, PITCH),
        number: 0,
        jitter: 0.0,
        reset: true,
    };
    let un = sd::uniforms(&sc, w, h, &f);
    let zeros = vec![0u32; TW * w * h];
    let want = sd::reference(&sc, &mut pos, &zeros, &un);
    let got = on_sim(&sc, &pos, &zeros, &un);
    let wrong = mismatches(&got, &want);
    std::println!(
        "console's scene on the simulator: {} differ: {:?}",
        wrong.len(),
        wrong.iter().map(|i| (i % w, i / w)).collect::<Vec<_>>()
    );
    assert!(wrong.is_empty());
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
    let mut tex = crate::interp::NoTextures;
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
            [
                v.abs().to_bits(),
                cov.to_bits(),
                cov.to_bits(),
                1.0f32.to_bits(),
            ]
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

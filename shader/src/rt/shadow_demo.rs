//! A scene for the shadow kernel, shared by the tests on the PC and the
//! GPU's own test (`gpu test`): a valley with six boxes standing in it,
//! the pre-pass's positions worked out on the CPU (a camera ray per
//! texel), the uniforms of a frame, and the kernel run by the reference
//! machine.

use alloc::vec;
use alloc::vec::Vec;

use super::demo::{camera_rays_from, valley};
use super::shadow::{self, u, BOXES, BOX_WORDS};
use super::{build, trace, Bvh, Triangle};
use crate::fmath;
use crate::interp::{run, Machine, Sampler};
use crate::{Builtin, MAX_ATTRS, MAX_BUFFERS};

/// tan(30 degrees): the half field of view of `camera_rays_from`.
const HALF: f32 = 0.577_350_3;

pub struct Scene {
    pub bvh: Bvh,
    pub tris: Vec<Triangle>,
    pub boxes: Vec<[f32; 18]>,
    pub sun: [f32; 3],
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = fmath::sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2]);
    [v[0] / l, v[1] / l, v[2] / l]
}

fn cross(p: [f32; 3], q: [f32; 3]) -> [f32; 3] {
    [
        p[1] * q[2] - p[2] * q[1],
        p[2] * q[0] - p[0] * q[2],
        p[0] * q[1] - p[1] * q[0],
    ]
}

/// A valley of 40 x 40 cells with 60 trees, the sun up and to the side,
/// and six boxes near the middle of the view, turned about y.
pub fn scene() -> Scene {
    let tris = valley(40, 60, 3);
    let bvh = build(&tris).unwrap_or_else(|_| unreachable!("the valley builds"));
    let mut boxes = Vec::new();
    for k in 0..BOXES {
        let a = k as f32 * 0.7;
        let (s, c) = (fmath::sin(a), fmath::cos(a));
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
        sun: normalize([0.45, 0.7, 0.3]),
    }
}

/// The view-projection that maps a point to the texel grid the way
/// `camera_rays_from` shoots it (rows, as the kernel reads them).
pub fn view_projection(eye: [f32; 3], pitch: f32) -> [[f32; 4]; 4] {
    let (sp, cp) = (fmath::sin(pitch), fmath::cos(pitch));
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

/// What `shadow::pack_normal` stores, and the normal the kernel reads
/// back from it.
pub fn quantize(n: [f32; 3]) -> ([f32; 3], f32) {
    let e = |v: f32| ((v * 0.5 + 0.5).clamp(0.0, 1.0) * 2047.0 + 0.5) as u32;
    let (ex, ey) = (e(n[0]), e(n[1]));
    let sign = (n[2] < 0.0) as u32;
    let packed = (sign << 22 | ex << 11 | ey) as f32 + 1.0;
    let (x, y) = (
        ex as f32 / 2047.0 * 2.0 - 1.0,
        ey as f32 / 2047.0 * 2.0 - 1.0,
    );
    let z = fmath::sqrt((1.0 - x * x - y * y).max(0.0));
    ([x, y, if sign == 1 { -z } else { z }], packed)
}

/// The pre-pass's target: `w` x `h` texels, row by row.
pub struct Positions {
    pub w: usize,
    pub h: usize,
    pub texels: Vec<[f32; 4]>,
}

impl Sampler for Positions {
    fn sample(&mut self, _: u8, u: f32, v: f32, _: Option<f32>) -> [f32; 4] {
        let (x, y) = ((u * self.w as f32) as usize, (v * self.h as f32) as usize);
        self.texels[y.min(self.h - 1) * self.w + x.min(self.w - 1)]
    }
}

/// The pre-pass's texels seen from `eye`: the surface point, the normal
/// packed in w (0: no ray needed, every seventh texel), and the normals
/// as the kernel will read them.
pub fn positions(sc: &Scene, w: usize, h: usize, eye: [f32; 3], pitch: f32) -> (Positions, Vec<[f32; 3]>) {
    let mut normals = Vec::new();
    let texels = camera_rays_from(w, h, eye, pitch)
        .iter()
        .enumerate()
        .map(|(i, r)| match trace::trace(&sc.bvh, r) {
            Some(hit) if i % 7 != 3 => {
                let v = sc.tris[hit.id as usize].v;
                let e1 = [v[1][0] - v[0][0], v[1][1] - v[0][1], v[1][2] - v[0][2]];
                let e2 = [v[2][0] - v[0][0], v[2][1] - v[0][1], v[2][2] - v[0][2]];
                let mut n = normalize(cross(e1, e2));
                // Toward the camera.
                if n[0] * r.dir[0] + n[1] * r.dir[1] + n[2] * r.dir[2] > 0.0 {
                    n = [-n[0], -n[1], -n[2]];
                }
                let (nq, packed) = quantize(n);
                normals.push(nq);
                [
                    r.origin[0] + r.dir[0] * hit.t,
                    r.origin[1] + r.dir[1] * hit.t,
                    r.origin[2] + r.dir[2] * hit.t,
                    packed,
                ]
            }
            _ => {
                normals.push([0.0, 1.0, 0.0]);
                [0.0; 4]
            }
        })
        .collect();
    (Positions { w, h, texels }, normals)
}

/// One frame's camera and settings.
#[derive(Clone, Copy)]
pub struct Frame {
    pub eye: [f32; 3],
    /// The last frame's camera: position and pitch.
    pub prev: ([f32; 3], f32),
    pub number: u32,
    /// Radius of the sun's disc as a tangent (0: no jitter).
    pub jitter: f32,
    pub reset: bool,
}

/// The kernel's uniforms for frame `f` on a `w` x `h` target.
pub fn uniforms(sc: &Scene, w: usize, h: usize, f: &Frame) -> Vec<u32> {
    let mut un = vec![0u32; u::WORDS];
    un[u::W as usize] = w as u32;
    un[u::H as usize] = h as u32;
    let mut set = |at: u16, v: &[f32]| {
        for (k, x) in v.iter().enumerate() {
            un[at as usize + k] = x.to_bits();
        }
    };
    set(u::SUN, &sc.sun);
    set(u::T_MAX, &[80.0]);
    set(u::INV_W, &[1.0 / w as f32, 1.0 / h as f32]);
    set(u::SPHERE, &[0.0, 2.0, 2.5, 6.0]);
    for (k, bx) in sc.boxes.iter().enumerate() {
        set(u::BOXES + (k as u16) * BOX_WORDS, bx);
    }
    let vp = view_projection(f.prev.0, f.prev.1);
    for (r, row) in vp.iter().enumerate() {
        set(u::PREV_VP + 4 * r as u16, row);
    }
    set(u::PREV_CAM, &f.prev.0);
    set(u::CAM, &f.eye);
    set(u::ALPHA, &[0.15]);
    set(u::JITTER, &[f.jitter]);
    set(u::RESET, &[if f.reset { 1.0 } else { 0.0 }]);
    let s = sc.sun;
    let a = if s[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let su = normalize(cross(s, a));
    set(u::SUN_U, &su);
    set(u::SUN_V, &normalize(cross(s, su)));
    set(u::PREV_SPHERE, &[1.0e6, 0.0, 0.0, 0.1]);
    un[u::FRAME as usize] = f.number;
    un
}

/// One frame of the kernel on the reference machine: its mask (two words
/// a texel), from last frame's `hist`.
pub fn reference(sc: &Scene, pos: &mut Positions, hist: &[u32], un: &[u32]) -> Vec<u32> {
    let (w, h) = (pos.w, pos.h);
    let p = shadow::kernel(shadow::Slots::default());
    let mut bufs = vec![Vec::new(); MAX_BUFFERS];
    bufs[0] = vec![0xAAAA_AAAA; 2 * w * h];
    bufs[1] = hist.to_vec();
    bufs[2] = sc.bvh.nodes.clone();
    bufs[3] = sc.bvh.tris.clone();
    let (gw, gh) = (shadow::GROUP[0] as usize, shadow::GROUP[1] as usize);
    for gy in 0..h.div_ceil(gh) {
        for gx in 0..w.div_ceil(gw) {
            for ly in 0..gh as u32 {
                for lx in 0..gw as u32 {
                    let mut m = Machine {
                        inputs: [[0; 4]; MAX_ATTRS],
                        builtins: [0; 11],
                        uniforms: un,
                        buffers: &mut bufs,
                        sampler: pos,
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

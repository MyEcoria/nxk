//! Scenes and rays to test the tree and the kernels with.

use alloc::vec::Vec;

use super::{Ray, Triangle};
use crate::fuzz::Rng;

fn unit(r: &mut Rng) -> f32 {
    (r.next_u32() >> 8) as f32 / (1 << 24) as f32
}

/// Triangles scattered in a box of side 20, each about `size` across.
pub fn soup(n: usize, size: f32, seed: u64) -> Vec<Triangle> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let c = [
                unit(&mut r) * 20.0 - 10.0,
                unit(&mut r) * 20.0 - 10.0,
                unit(&mut r) * 20.0 - 10.0,
            ];
            let mut v = [[0.0; 3]; 3];
            for p in &mut v {
                for a in 0..3 {
                    p[a] = c[a] + (unit(&mut r) - 0.5) * size;
                }
            }
            Triangle { v, id: i as u32 }
        })
        .collect()
}

pub fn rays(n: usize, seed: u64) -> Vec<Ray> {
    let mut r = Rng(seed);
    (0..n)
        .map(|_| {
            let o = [
                unit(&mut r) * 24.0 - 12.0,
                unit(&mut r) * 24.0 - 12.0,
                unit(&mut r) * 24.0 - 12.0,
            ];
            let mut d = [unit(&mut r) - 0.5, unit(&mut r) - 0.5, unit(&mut r) - 0.5];
            let l = crate::fmath::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).max(1e-3);
            d = [d[0] / l, d[1] / l, d[2] / l];
            Ray {
                origin: o,
                t_min: 1e-4,
                dir: d,
                t_max: 1e9,
            }
        })
        .collect()
}

/// A pinhole camera at (0, 0, -14) looking along +z, 60 degrees across:
/// `w` x `h` rays, row by row (neighbours in a warp are neighbours on the
/// screen, as a game's camera rays are).
pub fn camera_rays(w: usize, h: usize) -> Vec<Ray> {
    let half = 0.577_350_3f32;
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let u = ((x as f32 + 0.5) / w as f32 * 2.0 - 1.0) * half;
            let t = (1.0 - (y as f32 + 0.5) / h as f32 * 2.0) * half;
            let l = crate::fmath::sqrt(u * u + t * t + 1.0);
            v.push(Ray {
                origin: [0.0, 0.0, -14.0],
                t_min: 1e-4,
                dir: [u / l, t / l, 1.0 / l],
                t_max: 1e9,
            });
        }
    }
    v
}

/// One shadow ray per camera ray, toward `sun` (a unit vector), from where
/// the camera ray hits (a point 12 along it when it hits nothing).
pub fn shadow_rays(bvh: &super::Bvh, camera: &[Ray], sun: [f32; 3]) -> Vec<Ray> {
    camera
        .iter()
        .map(|r| {
            let t = super::trace::trace(bvh, r).map_or(12.0, |h| h.t);
            Ray {
                origin: [
                    r.origin[0] + r.dir[0] * t,
                    r.origin[1] + r.dir[1] * t,
                    r.origin[2] + r.dir[2] * t,
                ],
                t_min: 1e-3,
                dir: sun,
                t_max: 1e9,
            }
        })
        .collect()
}

/// A valley: a `n` x `n` cell height field (2 triangles a cell, 1 unit
/// apart, centred) with `trees` cones of 6 triangles on it, as
/// triangles with ids in order. About what a small game level is.
pub fn valley(n: usize, trees: usize, seed: u64) -> Vec<Triangle> {
    let h = |x: f32, z: f32| {
        4.0 * crate::fmath::sin(x * 0.07) * crate::fmath::cos(z * 0.05)
            + 2.0 * crate::fmath::sin(x * 0.19 + z * 0.13)
    };
    let half = n as f32 / 2.0;
    let mut v: Vec<[[f32; 3]; 3]> = Vec::new();
    let p = |i: usize, j: usize| {
        let (x, z) = (i as f32 - half, j as f32 - half);
        [x, h(x, z), z]
    };
    for i in 0..n {
        for j in 0..n {
            v.push([p(i, j), p(i + 1, j), p(i, j + 1)]);
            v.push([p(i + 1, j), p(i + 1, j + 1), p(i, j + 1)]);
        }
    }
    let mut r = Rng(seed);
    for _ in 0..trees {
        let (x, z) = (
            unit(&mut r) * n as f32 - half,
            unit(&mut r) * n as f32 - half,
        );
        let base = h(x, z);
        let top = [x, base + 3.0 + unit(&mut r) * 2.0, z];
        let ring: Vec<[f32; 3]> = (0..6)
            .map(|k| {
                let a = k as f32 * (core::f32::consts::TAU / 6.0);
                [
                    x + 0.8 * crate::fmath::cos(a),
                    base,
                    z + 0.8 * crate::fmath::sin(a),
                ]
            })
            .collect();
        for k in 0..6 {
            v.push([ring[k], ring[(k + 1) % 6], top]);
        }
    }
    v.into_iter()
        .enumerate()
        .map(|(i, v)| Triangle { v, id: i as u32 })
        .collect()
}

/// A camera at `eye`, `pitch` radians down (looking along +z): `w` x `h`
/// rays, row by row.
pub fn camera_rays_from(w: usize, h: usize, eye: [f32; 3], pitch: f32) -> Vec<Ray> {
    let half = 0.577_350_3f32;
    let (sp, cp) = (crate::fmath::sin(pitch), crate::fmath::cos(pitch));
    let mut v = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let u = ((x as f32 + 0.5) / w as f32 * 2.0 - 1.0) * half;
            let t = (1.0 - (y as f32 + 0.5) / h as f32 * 2.0) * half;
            // (u, t, 1) tilted down by `pitch` about the x axis.
            let d = [u, t * cp - sp, t * sp + cp];
            let l = crate::fmath::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
            v.push(Ray {
                origin: eye,
                t_min: 1e-4,
                dir: [d[0] / l, d[1] / l, d[2] / l],
                t_max: 1e9,
            });
        }
    }
    v
}

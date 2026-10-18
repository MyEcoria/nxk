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

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

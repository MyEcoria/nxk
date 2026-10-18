//! The same queries on the CPU: the reference the kernels are checked
//! against, and a brute-force one to check the tree with.

use alloc::vec::Vec;

use super::bvh::{Bvh, Triangle};
use super::{Hit, Ray, EMPTY, LEAF, NODE_WORDS, TRI_WORDS};

fn f(w: u32) -> f32 {
    f32::from_bits(w)
}

/// Moller-Trumbore on `v0`, `e1 = v1 - v0`, `e2 = v2 - v0`, two-sided:
/// (t, u, v) if the ray hits within (`t_min`, `t_max`).
pub fn intersect(
    v0: [f32; 3],
    e1: [f32; 3],
    e2: [f32; 3],
    o: [f32; 3],
    d: [f32; 3],
    t_min: f32,
    t_max: f32,
) -> Option<(f32, f32, f32)> {
    let p = [
        d[1] * e2[2] - d[2] * e2[1],
        d[2] * e2[0] - d[0] * e2[2],
        d[0] * e2[1] - d[1] * e2[0],
    ];
    let det = e1[0] * p[0] + e1[1] * p[1] + e1[2] * p[2];
    if det.abs() <= 1e-12 || det.is_nan() {
        return None;
    }
    let inv = 1.0 / det;
    let tv = [o[0] - v0[0], o[1] - v0[1], o[2] - v0[2]];
    let u = (tv[0] * p[0] + tv[1] * p[1] + tv[2] * p[2]) * inv;
    let q = [
        tv[1] * e1[2] - tv[2] * e1[1],
        tv[2] * e1[0] - tv[0] * e1[2],
        tv[0] * e1[1] - tv[1] * e1[0],
    ];
    let v = (d[0] * q[0] + d[1] * q[1] + d[2] * q[2]) * inv;
    let t = (e2[0] * q[0] + e2[1] * q[1] + e2[2] * q[2]) * inv;
    (u >= 0.0 && v >= 0.0 && u + v <= 1.0 && t > t_min && t < t_max).then_some((t, u, v))
}

fn tri(tris: &[u32], i: u32) -> ([f32; 3], [f32; 3], [f32; 3], u32) {
    let w = &tris[i as usize * TRI_WORDS..][..TRI_WORDS];
    (
        [f(w[0]), f(w[1]), f(w[2])],
        [f(w[3]), f(w[4]), f(w[5])],
        [f(w[6]), f(w[7]), f(w[8])],
        w[9],
    )
}

/// Distance at which the ray enters box `c` of node `n`, if it does
/// before `t_max`.
fn slab(n: &[u32], c: usize, o: [f32; 3], inv: [f32; 3], t_min: f32, t_max: f32) -> Option<f32> {
    let (mut near, mut far) = (t_min, t_max);
    for a in 0..3 {
        let lo = (f(n[4 + 4 * a + c]) - o[a]) * inv[a];
        let hi = (f(n[16 + 4 * a + c]) - o[a]) * inv[a];
        near = near.max(lo.min(hi));
        far = far.min(lo.max(hi));
    }
    (near <= far).then_some(near)
}

/// The nearest hit, or any hit when `any` (then the first found).
fn walk(bvh: &Bvh, ray: &Ray, any: bool) -> Option<Hit> {
    let inv = [1.0 / ray.dir[0], 1.0 / ray.dir[1], 1.0 / ray.dir[2]];
    let mut t_max = ray.t_max;
    let mut best: Option<Hit> = None;
    let mut stack: Vec<u32> = alloc::vec![0];
    while let Some(r) = stack.pop() {
        if r & LEAF != 0 {
            let (first, count) = ((r & !LEAF) >> 3, (r & 7) + 1);
            for i in first..first + count {
                let (v0, e1, e2, id) = tri(&bvh.tris, i);
                if let Some((t, u, v)) =
                    intersect(v0, e1, e2, ray.origin, ray.dir, ray.t_min, t_max)
                {
                    t_max = t;
                    best = Some(Hit { t, u, v, id });
                    if any {
                        return best;
                    }
                }
            }
            continue;
        }
        let n = &bvh.nodes[r as usize * NODE_WORDS..][..NODE_WORDS];
        let mut near: Vec<(f32, u32)> = Vec::new();
        for c in 0..4 {
            if n[c] == EMPTY {
                continue;
            }
            if let Some(t) = slab(n, c, ray.origin, inv, ray.t_min, t_max) {
                near.push((t, n[c]));
            }
        }
        // Farthest first, so the nearest comes off the stack first.
        near.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));
        stack.extend(near.into_iter().map(|(_, r)| r));
    }
    best
}

/// The nearest triangle the ray hits.
pub fn trace(bvh: &Bvh, ray: &Ray) -> Option<Hit> {
    walk(bvh, ray, false)
}

/// Whether anything is hit.
pub fn occluded(bvh: &Bvh, ray: &Ray) -> bool {
    walk(bvh, ray, true).is_some()
}

/// The nearest hit by testing every triangle.
pub fn brute_force(tris: &[Triangle], ray: &Ray) -> Option<Hit> {
    let mut t_max = ray.t_max;
    let mut best = None;
    for t in tris {
        let e1 = [
            t.v[1][0] - t.v[0][0],
            t.v[1][1] - t.v[0][1],
            t.v[1][2] - t.v[0][2],
        ];
        let e2 = [
            t.v[2][0] - t.v[0][0],
            t.v[2][1] - t.v[0][1],
            t.v[2][2] - t.v[0][2],
        ];
        if let Some((tt, u, v)) = intersect(t.v[0], e1, e2, ray.origin, ray.dir, ray.t_min, t_max) {
            t_max = tt;
            best = Some(Hit {
                t: tt,
                u,
                v,
                id: t.id,
            });
        }
    }
    best
}

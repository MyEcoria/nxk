//! Low-poly meshes built in code: flat-shaded triangles with a color each.

use nxgfx::raytrace::Triangle;

/// The vertex the lit shaders read: position, normal, color.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct V {
    pub p: [f32; 3],
    pub n: [f32; 3],
    pub c: [u8; 4],
}

pub const STRIDE: u32 = std::mem::size_of::<V>() as u32;

pub fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-9);
    [a[0] / l, a[1] / l, a[2] / l]
}

#[derive(Default)]
pub struct MeshData {
    pub v: Vec<V>,
    pub i: Vec<u16>,
}

impl MeshData {
    pub fn new() -> Self {
        Self::default()
    }

    /// A triangle, counter-clockwise seen from the front; flat normal.
    pub fn tri(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3], col: [u8; 4]) {
        let n = normalize(cross(sub(b, a), sub(c, a)));
        let base = self.v.len() as u16;
        for p in [a, b, c] {
            self.v.push(V { p, n, c: col });
        }
        self.i.extend([base, base + 1, base + 2]);
    }

    pub fn quad(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3], col: [u8; 4]) {
        self.tri(a, b, c, col);
        self.tri(a, c, d, col);
    }

    /// A box from `min` to `max`.
    pub fn cuboid(&mut self, min: [f32; 3], max: [f32; 3], col: [u8; 4]) {
        let [x0, y0, z0] = min;
        let [x1, y1, z1] = max;
        self.quad([x1, y0, z1], [x1, y0, z0], [x1, y1, z0], [x1, y1, z1], col);
        self.quad([x0, y0, z0], [x0, y0, z1], [x0, y1, z1], [x0, y1, z0], col);
        self.quad([x0, y1, z1], [x1, y1, z1], [x1, y1, z0], [x0, y1, z0], col);
        self.quad([x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1], col);
        self.quad([x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1], col);
        self.quad([x1, y0, z0], [x0, y0, z0], [x0, y1, z0], [x1, y1, z0], col);
    }

    /// A cone up from `base`, the sides only.
    pub fn cone(&mut self, base: [f32; 3], r: f32, h: f32, segs: u32, col: [u8; 4]) {
        let apex = [base[0], base[1] + h, base[2]];
        let pt = |k: u32| {
            let a = k as f32 / segs as f32 * std::f32::consts::TAU;
            [base[0] + r * a.cos(), base[1], base[2] + r * a.sin()]
        };
        for k in 0..segs {
            self.tri(pt(k), apex, pt(k + 1), col);
        }
    }

    /// An octahedron of radius `r` around `c`.
    pub fn octa(&mut self, c: [f32; 3], r: f32, col: [u8; 4]) {
        let at = |s: [f32; 3]| [c[0] + s[0] * r, c[1] + s[1] * r, c[2] + s[2] * r];
        for sx in [1.0f32, -1.0] {
            for sy in [1.0f32, -1.0] {
                for sz in [1.0f32, -1.0] {
                    let (a, b, d) = (at([sx, 0.0, 0.0]), at([0.0, sy, 0.0]), at([0.0, 0.0, sz]));
                    if sx * sy * sz > 0.0 {
                        self.tri(a, b, d, col);
                    } else {
                        self.tri(a, d, b, col);
                    }
                }
            }
        }
    }

    /// The triangles with their numbers, for the raytracer.
    pub fn triangles(&self, first_id: u32) -> Vec<Triangle> {
        self.i
            .chunks(3)
            .enumerate()
            .map(|(k, t)| Triangle {
                v: [
                    self.v[t[0] as usize].p,
                    self.v[t[1] as usize].p,
                    self.v[t[2] as usize].p,
                ],
                id: first_id + k as u32,
            })
            .collect()
    }
}

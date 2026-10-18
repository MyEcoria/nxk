//! The valley: a heightfield with meadows, rock, snow and lakes; trees,
//! boulders and the lights to collect.

use crate::meshes::{normalize, MeshData, V};

/// Vertices per side and meters between them.
pub const N: usize = 129;
pub const CELL: f32 = 2.0;
pub const HALF: f32 = (N as f32 - 1.0) * CELL * 0.5;
pub const WATER: f32 = 0.25;

pub struct Rng(pub u32);

impl Rng {
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    pub fn unit(&mut self) -> f32 {
        (self.next() >> 8) as f32 / (1 << 24) as f32
    }

    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

fn hash(ix: i32, iz: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(374_761_393)
        ^ (iz as u32).wrapping_mul(668_265_263)
        ^ seed.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0xFF_FFFF) as f32 / 16_777_216.0
}

fn vnoise(x: f32, z: f32, seed: u32) -> f32 {
    let (fx, fz) = (x.floor(), z.floor());
    let (tx, tz) = (x - fx, z - fz);
    let (sx, sz) = (tx * tx * (3.0 - 2.0 * tx), tz * tz * (3.0 - 2.0 * tz));
    let (ix, iz) = (fx as i32, fz as i32);
    let a = hash(ix, iz, seed);
    let b = hash(ix + 1, iz, seed);
    let c = hash(ix, iz + 1, seed);
    let d = hash(ix + 1, iz + 1, seed);
    (a + (b - a) * sx) + ((c + (d - c) * sx) - (a + (b - a) * sx)) * sz
}

fn fbm(x: f32, z: f32) -> f32 {
    let (mut sum, mut amp, mut f) = (0.0, 0.5, 1.0);
    for o in 0..5 {
        sum += vnoise(x * f, z * f, 17 + o) * amp;
        amp *= 0.5;
        f *= 2.0;
    }
    sum
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The ground's height at (x, z) before it is sampled into the grid.
fn raw_height(x: f32, z: f32) -> f32 {
    let r = (x * x + z * z).sqrt();
    let n = fbm(x * 0.012, z * 0.012);
    let ridge = (n - 0.42).max(0.0);
    let wild = n * 12.0 - 4.0 + ridge * ridge * 170.0;
    let meadow = 2.0 + 0.6 * (x * 0.2).sin() * (z * 0.17).cos();
    meadow + (wild - meadow) * smoothstep(16.0, 70.0, r)
}

pub struct Terrain {
    h: Vec<f32>,
}

impl Terrain {
    pub fn new() -> Self {
        let mut h = Vec::with_capacity(N * N);
        for j in 0..N {
            for i in 0..N {
                h.push(raw_height(
                    i as f32 * CELL - HALF,
                    j as f32 * CELL - HALF,
                ));
            }
        }
        Self { h }
    }

    fn at(&self, i: usize, j: usize) -> f32 {
        self.h[j.min(N - 1) * N + i.min(N - 1)]
    }

    /// Height of the ground under (x, z), bilinear between the vertices.
    pub fn height(&self, x: f32, z: f32) -> f32 {
        let gx = ((x + HALF) / CELL).clamp(0.0, N as f32 - 1.001);
        let gz = ((z + HALF) / CELL).clamp(0.0, N as f32 - 1.001);
        let (i, j) = (gx as usize, gz as usize);
        let (tx, tz) = (gx - i as f32, gz - j as f32);
        let a = self.at(i, j) + (self.at(i + 1, j) - self.at(i, j)) * tx;
        let b = self.at(i, j + 1) + (self.at(i + 1, j + 1) - self.at(i, j + 1)) * tx;
        a + (b - a) * tz
    }

    pub fn normal(&self, x: f32, z: f32) -> [f32; 3] {
        let e = CELL;
        normalize([
            self.height(x - e, z) - self.height(x + e, z),
            2.0 * e,
            self.height(x, z - e) - self.height(x, z + e),
        ])
    }

    fn color(&self, x: f32, z: f32, y: f32, n: [f32; 3]) -> [u8; 4] {
        let tex = 0.88 + 0.12 * vnoise(x * 0.9, z * 0.9, 5);
        let c = if y < WATER + 0.7 {
            [0.78, 0.72, 0.52]
        } else if n[1] < 0.72 || y > 18.0 {
            if y > 26.0 && n[1] > 0.55 {
                [0.92, 0.94, 0.98]
            } else {
                [0.46, 0.43, 0.41]
            }
        } else {
            let g = vnoise(x * 0.05, z * 0.05, 9);
            [0.20 + 0.12 * g, 0.46 + 0.14 * g, 0.16]
        };
        [
            (c[0] * tex * 255.0) as u8,
            (c[1] * tex * 255.0) as u8,
            (c[2] * tex * 255.0) as u8,
            255,
        ]
    }

    /// The ground as a mesh, smooth normals.
    pub fn mesh(&self) -> MeshData {
        let mut m = MeshData::new();
        for j in 0..N {
            for i in 0..N {
                let (x, z) = (i as f32 * CELL - HALF, j as f32 * CELL - HALF);
                let y = self.at(i, j);
                let n = self.normal(x, z);
                m.v.push(V {
                    p: [x, y, z],
                    n,
                    c: self.color(x, z, y, n),
                });
            }
        }
        for j in 0..N - 1 {
            for i in 0..N - 1 {
                let p00 = (j * N + i) as u16;
                let (p10, p01, p11) = (p00 + 1, p00 + N as u16, p00 + N as u16 + 1);
                m.i.extend([p00, p01, p10, p10, p01, p11]);
            }
        }
        m
    }
}

pub struct Scenery {
    pub trees: MeshData,
    pub rocks: MeshData,
    pub orbs: Vec<[f32; 3]>,
}

/// Trees on the meadows, boulders on the slopes, lights in the open.
pub fn scatter(t: &Terrain, seed: u32) -> Scenery {
    let mut rng = Rng(seed);
    let mut trees = MeshData::new();
    let mut rocks = MeshData::new();
    let mut orbs = Vec::new();
    let mut placed = 0;
    let mut tries = 0;
    while placed < 230 && tries < 6000 {
        tries += 1;
        let (x, z) = (rng.range(-HALF + 6.0, HALF - 6.0), rng.range(-HALF + 6.0, HALF - 6.0));
        let y = t.height(x, z);
        let n = t.normal(x, z);
        if (x * x + z * z).sqrt() < 18.0 || n[1] < 0.84 || !(WATER + 0.9..17.0).contains(&y) {
            continue;
        }
        let s = rng.range(0.8, 1.5);
        let g = rng.range(0.85, 1.15);
        let trunk = [(92.0 * g) as u8, (62.0 * g) as u8, (40.0 * g) as u8, 255];
        trees.cuboid(
            [x - 0.18 * s, y - 0.1, z - 0.18 * s],
            [x + 0.18 * s, y + 1.6 * s, z + 0.18 * s],
            trunk,
        );
        for (k, (r, h)) in [(1.5, 1.7), (1.15, 1.5), (0.8, 1.3)].into_iter().enumerate() {
            let green = [
                (34.0 * g) as u8,
                ((96.0 + 18.0 * k as f32) * g) as u8,
                (44.0 * g) as u8,
                255,
            ];
            trees.cone([x, y + (0.9 + 1.0 * k as f32) * s, z], r * s, h * s, 7, green);
        }
        placed += 1;
    }
    let mut count = 0;
    tries = 0;
    while count < 70 && tries < 4000 {
        tries += 1;
        let (x, z) = (rng.range(-HALF + 6.0, HALF - 6.0), rng.range(-HALF + 6.0, HALF - 6.0));
        let y = t.height(x, z);
        let n = t.normal(x, z);
        if (x * x + z * z).sqrt() < 12.0 || y < WATER + 0.3 || n[1] > 0.9 {
            continue;
        }
        let r = rng.range(0.7, 2.0);
        let g = (rng.range(0.8, 1.1) * 120.0) as u8;
        rocks.octa([x, y + r * 0.35, z], r, [g, g, g + 6, 255]);
        count += 1;
    }
    tries = 0;
    while orbs.len() < 24 && tries < 4000 {
        tries += 1;
        let ang = rng.range(0.0, std::f32::consts::TAU);
        let rad = rng.range(10.0, HALF - 12.0);
        let (x, z) = (rad * ang.cos(), rad * ang.sin());
        let y = t.height(x, z);
        if y < WATER + 0.6 {
            continue;
        }
        orbs.push([x, y + 1.3, z]);
    }
    Scenery { trees, rocks, orbs }
}

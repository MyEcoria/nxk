//! The Lab: a hidden mode (L + R + Minus) that puts the GPU through what
//! 3D games ask of it, with the numbers on screen. Left / Right change
//! the room, Up / Down its load.
//!
//! * Overview: frame times and the work drawn.
//! * Draw calls: a lattice of cubes, one draw and one uniform block each.
//! * Particles: additive billboards from a buffer rewritten every frame.
//! * Rays: a grid of rays from the camera into the valley's triangles on
//!   the GPU (a BVH walked by a compute shader), each hit sending a
//!   shadow ray to the sun: the hybrid raytracing, shown as a heat map.

use std::rc::Rc;

use nxgfx::raytrace::{self, RtKernels, RtScene};
use nxgfx::{rgb, Buffer, Button, Canvas, Frame, Graphics};

use crate::meshes::{normalize, V};

pub const ROOMS: [&str; 4] = ["OVERVIEW", "DRAW CALLS", "PARTICLES", "RAYS"];

const MAX_CUBES: u32 = 700;
const MAX_PARTICLES: u32 = 1500;
pub const RAY_COLS: usize = 96;
pub const RAY_ROWS: usize = 54;
const RAYS: usize = RAY_COLS * RAY_ROWS;

pub struct Lab {
    pub room: usize,
    pub cubes: u32,
    pub particles: u32,
    times: Vec<f32>,
    /// Draws and triangles of the last frame.
    pub draws: u32,
    pub tris: u32,
    pub rays: Option<RayRoom>,
    /// What the sun's shadows cost, step by step (Overview, Up / Down):
    /// 0 all of it, 1 without the mask in the lit shader, 2 positions
    /// only, 3 none, 4 the mask itself, 5 all of it with the history forgotten
    /// every frame.
    pub shadow_mode: u8,
    pub particle_buf: Option<(Buffer, Buffer)>,
    /// Set when a frame was refused for being too big.
    pub refused: u32,
}

pub struct RayRoom {
    scene: Rc<RtScene>,
    kernels: RtKernels,
    primary: Buffer,
    hits: Buffer,
    shadow: Buffer,
    shadow_hits: Buffer,
    /// The picture from the last frame: distance, lit.
    pub image: Vec<(f32, bool)>,
    /// Closest hits waiting for their shadow rays.
    pending: Vec<Option<[f32; 3]>>,
    /// The camera ray of each cell, this frame.
    dirs: Vec<[f32; 3]>,
    eye: [f32; 3],
    pub build_ms: f32,
    pub triangles: u32,
}

impl Lab {
    pub fn new() -> Self {
        Self {
            room: 0,
            cubes: 200,
            particles: 600,
            times: Vec::new(),
            draws: 0,
            tris: 0,
            rays: None,
            shadow_mode: 0,
            particle_buf: None,
            refused: 0,
        }
    }

    pub fn button(&mut self, b: Button) {
        match b {
            Button::Left => self.room = (self.room + ROOMS.len() - 1) % ROOMS.len(),
            Button::Right => self.room = (self.room + 1) % ROOMS.len(),
            Button::Up => match self.room {
                1 => self.cubes = (self.cubes + 50).min(MAX_CUBES),
                2 => self.particles = (self.particles + 100).min(MAX_PARTICLES),
                0 => self.shadow_mode = (self.shadow_mode + 1) % 8,
                _ => {}
            },
            Button::Down => match self.room {
                1 => self.cubes = self.cubes.saturating_sub(50).max(50),
                2 => self.particles = self.particles.saturating_sub(100).max(100),
                0 => self.shadow_mode = (self.shadow_mode + 7) % 8,
                _ => {}
            },
            _ => {}
        }
    }

    pub fn frame_time(&mut self, ms: f32) {
        self.times.push(ms);
        if self.times.len() > 120 {
            self.times.remove(0);
        }
    }

    fn stats(&self) -> (f32, f32, f32) {
        if self.times.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        let avg = self.times.iter().sum::<f32>() / self.times.len() as f32;
        let max = self.times.iter().cloned().fold(0.0, f32::max);
        let min = self.times.iter().cloned().fold(f32::MAX, f32::min);
        (avg, min, max)
    }

    /// The overlay: title, room, numbers, the frame-time strip.
    pub fn hud(&self, c: &mut Canvas) {
        let (avg, min, max) = self.stats();
        c.text(28.0, 24.0, 3.0, "LAB", rgb(255, 200, 80));
        let title = format!("{} {}/{}", ROOMS[self.room], self.room + 1, ROOMS.len());
        c.text(110.0, 24.0, 3.0, &title, rgb(255, 255, 255));
        let fps = if avg > 0.0 { 1000.0 / avg } else { 0.0 };
        c.text(
            28.0,
            62.0,
            2.0,
            &format!(
                "{:.1} FPS  {:.1} MS (MIN {:.1} MAX {:.1})",
                fps, avg, min, max
            ),
            rgb(200, 220, 255),
        );
        c.text(
            28.0,
            86.0,
            2.0,
            &format!("{} DRAWS  {} TRIANGLES", self.draws, self.tris),
            rgb(200, 220, 255),
        );
        let extra = match self.room {
            1 => format!("{} CUBES (UP/DOWN)", self.cubes),
            2 => format!("{} PARTICLES (UP/DOWN)", self.particles),
            3 => match &self.rays {
                Some(r) => format!(
                    "{} RAYS+{} SHADOW  {} TRIS  BVH {:.0} MS",
                    RAYS, RAYS, r.triangles, r.build_ms
                ),
                None => "BUILDING...".into(),
            },
            _ => format!(
                "SHADOWS: {} (UP/DOWN)",
                [
                    "RAY TRACED",
                    "NO MASK IN LIT SHADER",
                    "POSITIONS ONLY",
                    "OFF",
                    "MASK VIEW",
                    "NO HISTORY",
                    "NO JITTER",
                    "NO JITTER, NO HISTORY"
                ][self.shadow_mode as usize]
            ),
        };
        c.text(28.0, 110.0, 2.0, &extra, rgb(160, 255, 180));
        if self.refused > 0 {
            c.text(
                28.0,
                134.0,
                2.0,
                &format!("{} FRAMES REFUSED: TOO BIG", self.refused),
                rgb(255, 120, 100),
            );
        }
        // Frame times as bars, a bar per frame, 33 ms high.
        let base = 198.0;
        for (i, t) in self.times.iter().enumerate() {
            let h = (t / 33.0).min(1.0) * 40.0;
            let col = if *t > 33.0 {
                rgb(255, 90, 80)
            } else if *t > 17.5 {
                rgb(255, 200, 80)
            } else {
                rgb(90, 220, 120)
            };
            c.rect(30.0 + i as f32 * 3.4, base - h, 2.6, h, col);
        }
        // Behind everything else: a canvas keeps the first thing drawn.
        c.rect(16.0, 16.0, 590.0, 190.0, [0.0, 0.0, 0.0, 1.0]);
    }

    /// Vertices of `n` billboards around the player, moving.
    pub fn particle_vertices(
        &self,
        t: f32,
        around: [f32; 3],
        right: [f32; 3],
        up: [f32; 3],
    ) -> Vec<V> {
        let mut v = Vec::with_capacity(self.particles as usize * 4);
        for k in 0..self.particles {
            let f = k as f32;
            let a = f * 2.399 + t * (0.2 + (f * 0.37).fract() * 0.5);
            let r = 3.0 + (f * 0.618).fract() * 9.0;
            let y = 0.5 + ((f * 0.131).fract() * 6.0 + t * 0.7 * (1.0 + (f * 0.07).fract())) % 6.0;
            let c = [
                around[0] + r * a.cos(),
                around[1] + y,
                around[2] + r * a.sin(),
            ];
            let s = 0.12 + (f * 0.29).fract() * 0.22;
            let hue = (f * 0.071 + t * 0.05).fract();
            let col = [
                (60.0 + 190.0 * (hue * 6.28).sin().abs()) as u8,
                (60.0 + 190.0 * ((hue + 0.33) * 6.28).sin().abs()) as u8,
                (60.0 + 190.0 * ((hue + 0.66) * 6.28).sin().abs()) as u8,
                140,
            ];
            let n = normalize([-right[2] * 0.0 + up[1] * 0.0, 0.0, 1.0]);
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                v.push(V {
                    p: [
                        c[0] + (right[0] * dx + up[0] * dy) * s,
                        c[1] + (right[1] * dx + up[1] * dy) * s,
                        c[2] + (right[2] * dx + up[2] * dy) * s,
                    ],
                    n,
                    c: col,
                });
            }
        }
        v
    }

    /// Resources of the particle room (made on first use).
    pub fn ensure_particles(&mut self, g: &Graphics) {
        if self.particle_buf.is_some() {
            return;
        }
        let mut idx: Vec<u16> = Vec::new();
        for k in 0..MAX_PARTICLES as u16 {
            let b = k * 4;
            idx.extend([b, b + 1, b + 2, b, b + 2, b + 3]);
        }
        let ib = g.buffer_from(&idx);
        let vb = g.buffer_zeroed(MAX_PARTICLES as usize * 4 * (std::mem::size_of::<V>() / 4));
        if let (Ok(vb), Ok(ib)) = (vb, ib) {
            self.particle_buf = Some((vb, ib));
        }
    }

    /// Resources of the ray room: the tree over the valley's triangles.
    /// `scene`: the game's tree (built once at load, in `build_ms`).
    pub fn ensure_rays(&mut self, g: &Graphics, scene: &Rc<RtScene>, build_ms: f32) {
        if self.rays.is_some() {
            return;
        }
        let scene = scene.clone();
        let (Ok(kernels), Ok(primary), Ok(hits), Ok(shadow), Ok(shadow_hits)) = (
            g.rt_kernels(),
            g.rt_rays(RAYS),
            g.rt_hits(RAYS, false),
            g.rt_rays(RAYS),
            g.rt_hits(RAYS, true),
        ) else {
            eprintln!("lumen: ray buffers");
            return;
        };
        self.rays = Some(RayRoom {
            triangles: scene.triangles,
            scene,
            kernels,
            primary,
            hits,
            shadow,
            shadow_hits,
            image: vec![(f32::INFINITY, true); RAYS],
            pending: vec![None; RAYS],
            dirs: vec![[0.0, 0.0, 1.0]; RAYS],
            eye: [0.0; 3],
            build_ms,
        });
    }
}

impl RayRoom {
    /// Record both queries for this frame: the camera's rays, and the
    /// shadow rays for last frame's hits.
    pub fn trace(
        &mut self,
        f: &mut Frame,
        eye: [f32; 3],
        fwd: [f32; 3],
        right: [f32; 3],
        up: [f32; 3],
        sun: [f32; 3],
    ) {
        let tan = (1.0f32 / 2.0).tan();
        let aspect = RAY_COLS as f32 / RAY_ROWS as f32;
        for y in 0..RAY_ROWS {
            for x in 0..RAY_COLS {
                let u = ((x as f32 + 0.5) / RAY_COLS as f32 * 2.0 - 1.0) * tan * aspect;
                let v = (1.0 - (y as f32 + 0.5) / RAY_ROWS as f32 * 2.0) * tan;
                let d = normalize([
                    fwd[0] + right[0] * u + up[0] * v,
                    fwd[1] + right[1] * u + up[1] * v,
                    fwd[2] + right[2] * u + up[2] * v,
                ]);
                let i = y * RAY_COLS + x;
                self.dirs[i] = d;
                raytrace::write_ray(
                    &self.primary,
                    i,
                    &raytrace::Ray {
                        origin: eye,
                        t_min: 0.01,
                        dir: d,
                        t_max: 400.0,
                    },
                );
                let sray = match self.pending[i] {
                    Some(p) => raytrace::Ray {
                        origin: p,
                        t_min: 0.02,
                        dir: sun,
                        t_max: 300.0,
                    },
                    // Nothing to shade: a ray that goes nowhere.
                    None => raytrace::Ray {
                        origin: eye,
                        t_min: 1.0e9,
                        dir: sun,
                        t_max: 1.0e9,
                    },
                };
                raytrace::write_ray(&self.shadow, i, &sray);
            }
        }
        f.trace(
            &self.kernels,
            &self.scene,
            &self.shadow,
            &self.shadow_hits,
            RAYS as u32,
            true,
        );
        f.trace(
            &self.kernels,
            &self.scene,
            &self.primary,
            &self.hits,
            RAYS as u32,
            false,
        );
        self.eye = eye;
    }

    /// After the frame ran: the picture, and the points to shade next time.
    pub fn collect(&mut self) {
        for i in 0..RAYS {
            let blocked = raytrace::read_blocked(&self.shadow_hits, i);
            match raytrace::read_hit(&self.hits, i) {
                Some(h) => {
                    self.image[i] = (h.t, !blocked);
                    let d = self.dirs[i];
                    let t = (h.t - 0.05).max(0.0);
                    self.pending[i] = Some([
                        self.eye[0] + d[0] * t,
                        self.eye[1] + d[1] * t,
                        self.eye[2] + d[2] * t,
                    ]);
                }
                None => {
                    self.image[i] = (f32::INFINITY, true);
                    self.pending[i] = None;
                }
            }
        }
    }

    /// The rays as a grid of colored cells, in a corner of the screen.
    pub fn draw(&self, c: &mut Canvas) {
        let (x0, y0, w, h) = (1280.0 - 16.0 - 640.0, 16.0, 640.0, 360.0);
        let (cw, ch) = (w / RAY_COLS as f32, h / RAY_ROWS as f32);
        for y in 0..RAY_ROWS {
            for x in 0..RAY_COLS {
                let (t, lit) = self.image[y * RAY_COLS + x];
                let col = if t.is_finite() {
                    // Near is warm, far is cool; shadowed cells are dark.
                    let k = (t / 90.0).min(1.0);
                    let s = if lit { 1.0 } else { 0.28 };
                    [
                        (1.0 - k) * 0.95 * s,
                        (0.3 + 0.4 * (1.0 - (k - 0.5).abs() * 2.0).max(0.0)) * s,
                        (0.2 + k * 0.8) * s,
                        1.0,
                    ]
                } else {
                    [0.45, 0.62, 0.9, 1.0]
                };
                c.rect(
                    x0 + x as f32 * cw,
                    y0 + y as f32 * ch,
                    cw + 0.5,
                    ch + 0.5,
                    col,
                );
            }
        }
        c.rect(x0 - 4.0, y0 - 4.0, w + 8.0, h + 8.0, [0.0, 0.0, 0.0, 1.0]);
    }
}

//! The hero: a walking figure of boxes, and the controller that moves it
//! over the ground.

use nxgfx::{vec3, Mat4};

use crate::meshes::MeshData;
use crate::world::{Terrain, HALF, WATER};

pub struct Player {
    pub pos: [f32; 3],
    pub heading: f32,
    pub vy: f32,
    pub on_ground: bool,
    /// Phase of the walk cycle, radians.
    pub stride: f32,
    pub speed: f32,
}

impl Player {
    pub fn new(t: &Terrain) -> Self {
        Self {
            pos: [0.0, t.height(0.0, 0.0), 6.0],
            heading: std::f32::consts::PI,
            vy: 0.0,
            on_ground: true,
            stride: 0.0,
            speed: 0.0,
        }
    }

    /// `wish`: the direction asked for in the world (length 0..1), `run`
    /// for a sprint, `jump` for a jump.
    pub fn update(&mut self, t: &Terrain, dt: f32, wish: [f32; 2], run: bool, jump: bool) {
        let top = if run { 9.0 } else { 5.5 };
        let want = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt().min(1.0) * top;
        // Ease toward the wanted speed.
        self.speed += (want - self.speed) * (1.0 - (-10.0 * dt).exp());
        if want > 0.05 {
            let target = wish[0].atan2(wish[1]);
            let mut d = target - self.heading;
            while d > std::f32::consts::PI {
                d -= std::f32::consts::TAU;
            }
            while d < -std::f32::consts::PI {
                d += std::f32::consts::TAU;
            }
            self.heading += d * (1.0 - (-14.0 * dt).exp());
            let l = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt().max(1e-4);
            let (nx, nz) = (
                self.pos[0] + wish[0] / l * self.speed * dt,
                self.pos[2] + wish[1] / l * self.speed * dt,
            );
            // Steep slopes and the edge of the world stop the walk.
            let slope = t.normal(nx, nz)[1];
            if slope > 0.55 && nx.abs() < HALF - 4.0 && nz.abs() < HALF - 4.0 {
                self.pos[0] = nx;
                self.pos[2] = nz;
            } else {
                self.speed *= 0.5;
            }
        }
        let ground = t.height(self.pos[0], self.pos[2]);
        if jump && self.on_ground {
            self.vy = 7.5;
            self.on_ground = false;
        }
        self.vy -= 20.0 * dt;
        self.pos[1] += self.vy * dt;
        if self.pos[1] <= ground {
            self.pos[1] = ground;
            self.vy = 0.0;
            self.on_ground = true;
        } else if self.pos[1] > ground + 0.05 {
            self.on_ground = false;
        }
        self.stride += self.speed * dt * 1.7;
        // Wading slows the figure.
        if ground < WATER {
            self.speed *= 0.97;
        }
    }
}

/// Meshes of the figure's parts, each with its pivot at the origin.
pub struct Figure {
    pub torso: MeshData,
    pub head: MeshData,
    pub arm: MeshData,
    pub leg: MeshData,
}

pub fn figure() -> Figure {
    let mut torso = MeshData::new();
    torso.cuboid([-0.26, 0.0, -0.15], [0.26, 0.66, 0.15], [220, 90, 60, 255]);
    let mut head = MeshData::new();
    head.cuboid([-0.15, 0.0, -0.15], [0.15, 0.3, 0.15], [236, 196, 160, 255]);
    head.cuboid([-0.16, 0.2, -0.16], [0.16, 0.34, 0.16], [70, 46, 30, 255]);
    let mut arm = MeshData::new();
    arm.cuboid(
        [-0.07, -0.58, -0.07],
        [0.07, 0.0, 0.07],
        [236, 196, 160, 255],
    );
    arm.cuboid(
        [-0.075, -0.2, -0.075],
        [0.075, 0.0, 0.075],
        [220, 90, 60, 255],
    );
    let mut leg = MeshData::new();
    leg.cuboid([-0.1, -0.74, -0.11], [0.1, 0.0, 0.11], [52, 66, 120, 255]);
    leg.cuboid(
        [-0.105, -0.74, -0.12],
        [0.105, -0.6, 0.16],
        [60, 40, 30, 255],
    );
    Figure {
        torso,
        head,
        arm,
        leg,
    }
}

/// Model matrices of the parts for the pose now: torso, head, arms, legs.
pub fn pose(p: &Player) -> [Mat4; 6] {
    let root = Mat4::translation(vec3(p.pos[0], p.pos[1], p.pos[2])) * Mat4::rotation_y(p.heading);
    let swing = (p.stride).sin() * (p.speed / 6.0).min(1.2) * 0.9;
    let bob = (p.stride * 2.0).sin().abs() * 0.04 * (p.speed / 6.0).min(1.0);
    let air = if p.on_ground { 0.0 } else { 0.6 };
    let at = |x: f32, y: f32, z: f32| Mat4::translation(vec3(x, y, z));
    [
        root * at(0.0, 0.78 + bob, 0.0),
        root * at(0.0, 1.46 + bob, 0.0),
        root * at(0.33, 1.38 + bob, 0.0) * Mat4::rotation_x(-swing - air),
        root * at(-0.33, 1.38 + bob, 0.0) * Mat4::rotation_x(swing - air),
        root * at(0.12, 0.78 + bob, 0.0) * Mat4::rotation_x(swing * 0.9),
        root * at(-0.12, 0.78 + bob, 0.0) * Mat4::rotation_x(-swing * 0.9),
    ]
}

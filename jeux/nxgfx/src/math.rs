//! Vectors and 4x4 matrices, row-major (`m[row][col]`), column vectors,
//! OpenGL conventions: right-handed, y up, clip z from -w to w.

use core::ops::{Add, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

impl Vec3 {
    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        vec3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    pub fn length(self) -> f32 {
        sqrt(self.dot(self))
    }

    pub fn normalized(self) -> Vec3 {
        let l = self.length();
        if l > 0.0 {
            self * (1.0 / l)
        } else {
            self
        }
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        vec3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        vec3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Vec3;
    fn mul(self, k: f32) -> Vec3 {
        vec3(self.x * k, self.y * k, self.z * k)
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        vec3(-self.x, -self.y, -self.z)
    }
}

/// Square root without libm (Newton steps from a bit-level guess).
pub fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut y = f32::from_bits((x.to_bits() >> 1) + 0x1FBD_1DF5);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

/// Sine and cosine (radians), by range reduction and a polynomial good
/// to about 1e-6.
pub fn sin_cos(a: f32) -> (f32, f32) {
    const PI: f32 = core::f32::consts::PI;
    const TAU: f32 = 2.0 * PI;
    let mut x = a - TAU * ((a / TAU) + 0.5).floor_();
    // x in [-pi, pi]; fold to [-pi/2, pi/2] for the sine.
    let cos_sign;
    if x > PI / 2.0 {
        x = PI - x;
        cos_sign = -1.0;
    } else if x < -PI / 2.0 {
        x = -PI - x;
        cos_sign = -1.0;
    } else {
        cos_sign = 1.0;
    }
    let x2 = x * x;
    let s = x
        * (1.0
            + x2 * (-1.0 / 6.0
                + x2 * (1.0 / 120.0 + x2 * (-1.0 / 5040.0 + x2 * (1.0 / 362_880.0)))));
    let c = 1.0
        + x2 * (-0.5
            + x2 * (1.0 / 24.0
                + x2 * (-1.0 / 720.0 + x2 * (1.0 / 40_320.0 + x2 * (-1.0 / 3_628_800.0)))));
    (s, c * cos_sign)
}

pub fn tan(a: f32) -> f32 {
    let (s, c) = sin_cos(a);
    s / c
}

trait FloorExt {
    fn floor_(self) -> f32;
}

impl FloorExt for f32 {
    fn floor_(self) -> f32 {
        let t = self as i32 as f32;
        if t > self {
            t - 1.0
        } else {
            t
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat4(pub [[f32; 4]; 4]);

impl Mat4 {
    pub const IDENTITY: Mat4 = Mat4([
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]);

    pub fn translation(v: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[0][3] = v.x;
        m.0[1][3] = v.y;
        m.0[2][3] = v.z;
        m
    }

    pub fn scale(v: Vec3) -> Mat4 {
        let mut m = Mat4::IDENTITY;
        m.0[0][0] = v.x;
        m.0[1][1] = v.y;
        m.0[2][2] = v.z;
        m
    }

    pub fn rotation_x(a: f32) -> Mat4 {
        let (s, c) = sin_cos(a);
        let mut m = Mat4::IDENTITY;
        m.0[1][1] = c;
        m.0[1][2] = -s;
        m.0[2][1] = s;
        m.0[2][2] = c;
        m
    }

    pub fn rotation_y(a: f32) -> Mat4 {
        let (s, c) = sin_cos(a);
        let mut m = Mat4::IDENTITY;
        m.0[0][0] = c;
        m.0[0][2] = s;
        m.0[2][0] = -s;
        m.0[2][2] = c;
        m
    }

    pub fn rotation_z(a: f32) -> Mat4 {
        let (s, c) = sin_cos(a);
        let mut m = Mat4::IDENTITY;
        m.0[0][0] = c;
        m.0[0][1] = -s;
        m.0[1][0] = s;
        m.0[1][1] = c;
        m
    }

    /// OpenGL perspective: `fov_y` radians, clip z -w..w.
    pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
        let f = 1.0 / tan(fov_y / 2.0);
        let mut m = Mat4([[0.0; 4]; 4]);
        m.0[0][0] = f / aspect;
        m.0[1][1] = f;
        m.0[2][2] = (far + near) / (near - far);
        m.0[2][3] = 2.0 * far * near / (near - far);
        m.0[3][2] = -1.0;
        m
    }

    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
        let f = (target - eye).normalized();
        let s = f.cross(up).normalized();
        let u = s.cross(f);
        Mat4([
            [s.x, s.y, s.z, -s.dot(eye)],
            [u.x, u.y, u.z, -u.dot(eye)],
            [-f.x, -f.y, -f.z, f.dot(eye)],
            [0.0, 0.0, 0.0, 1.0],
        ])
    }

    pub fn transform(&self, p: Vec3) -> Vec3 {
        let m = &self.0;
        vec3(
            m[0][0] * p.x + m[0][1] * p.y + m[0][2] * p.z + m[0][3],
            m[1][0] * p.x + m[1][1] * p.y + m[1][2] * p.z + m[1][3],
            m[2][0] * p.x + m[2][1] * p.y + m[2][2] * p.z + m[2][3],
        )
    }
}

impl Mul for Mat4 {
    type Output = Mat4;
    fn mul(self, o: Mat4) -> Mat4 {
        let mut r = [[0.0f32; 4]; 4];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..4).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        Mat4(r)
    }
}

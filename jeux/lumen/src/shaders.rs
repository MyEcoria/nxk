//! The game's shaders, written in the IR: one lit vertex and fragment pair
//! for everything solid (a sun, a hemisphere light, distance fog, gamma).

use nxgfx::ir::rt::shadow::lit_factor as shadow_factor;
use nxgfx::ir::{Builder, CmpOp, OutKind, Program, Reg, Stage, Ty};

/// Uniform words: the model-view-projection rows, the model rows (3x4,
/// rotation and translation, no scale), the lights and the tint.
pub const U_MVP: u16 = 0;
pub const U_MODEL: u16 = 16;
/// Unit vector toward the sun.
pub const U_SUN_DIR: u16 = 28;
pub const U_SUN_COL: u16 = 31;
pub const U_SKY: u16 = 34;
pub const U_GROUND: u16 = 37;
pub const U_FOG: u16 = 40;
pub const U_FOG_DENSITY: u16 = 43;
pub const U_CAMERA: u16 = 44;
/// Multiplies the vertex color (rgb) and its alpha.
pub const U_TINT: u16 = 47;
/// Added light, times the albedo.
pub const U_EMISSIVE: u16 = 51;
/// Ray-traced shadows: 1 when the mask (buffer slot 0) applies, then its
/// size in texels.
pub const U_SHADOW_ON: u16 = 52;
pub const U_SHADOW_W: u16 = 53;
pub const U_SHADOW_H: u16 = 54;
pub const WORDS: usize = 55;

fn dot3(b: &mut Builder, a: [Reg; 3], c: [Reg; 3]) -> Reg {
    let x = b.fmul(a[0], c[0]);
    let y = b.fma(a[1], c[1], x);
    b.fma(a[2], c[2], y)
}

fn uni3(b: &mut Builder, at: u16) -> [Reg; 3] {
    [
        b.uniform(at, Ty::F32),
        b.uniform(at + 1, Ty::F32),
        b.uniform(at + 2, Ty::F32),
    ]
}

/// Position, normal, color in; clip position out; world normal, color and
/// world position to the fragment shader.
pub fn vertex() -> Program {
    let mut b = Builder::new(Stage::Vertex);
    let p = [b.input(0, 0), b.input(0, 1), b.input(0, 2)];
    let n = [b.input(1, 0), b.input(1, 1), b.input(1, 2)];
    let c = [b.input(2, 0), b.input(2, 1), b.input(2, 2), b.input(2, 3)];
    let mut clip = [b.f32(0.0); 4];
    for row in 0..4u16 {
        let mut acc = b.uniform(U_MVP + 4 * row + 3, Ty::F32);
        for (j, pj) in p.iter().enumerate() {
            let m = b.uniform(U_MVP + 4 * row + j as u16, Ty::F32);
            acc = b.fma(m, *pj, acc);
        }
        b.output(OutKind::Position, row as u8, acc);
        clip[row as usize] = acc;
    }
    // Clip x, y and w, interpolated like any attribute: the fragment
    // shader finds its place on the screen from them.
    b.output(OutKind::Varying(3), 0, clip[0]);
    b.output(OutKind::Varying(3), 1, clip[1]);
    b.output(OutKind::Varying(3), 2, clip[3]);
    for row in 0..3u16 {
        let mut acc = b.uniform(U_MODEL + 4 * row + 3, Ty::F32);
        let mut nrm = b.f32(0.0);
        for j in 0..3u16 {
            let m = b.uniform(U_MODEL + 4 * row + j, Ty::F32);
            acc = b.fma(m, p[j as usize], acc);
            nrm = b.fma(m, n[j as usize], nrm);
        }
        b.output(OutKind::Varying(2), row as u8, acc);
        b.output(OutKind::Varying(0), row as u8, nrm);
    }
    for (i, ci) in c.iter().enumerate() {
        b.output(OutKind::Varying(1), i as u8, *ci);
    }
    b.finish()
}

pub fn fragment() -> Program {
    let mut b = Builder::new(Stage::Fragment);
    let n = [b.input(0, 0), b.input(0, 1), b.input(0, 2)];
    let col = [b.input(1, 0), b.input(1, 1), b.input(1, 2), b.input(1, 3)];
    let w = [b.input(2, 0), b.input(2, 1), b.input(2, 2)];
    let clip = [b.input(3, 0), b.input(3, 1), b.input(3, 2)];
    // Normalize the interpolated normal.
    let len2 = dot3(&mut b, n, n);
    let tiny = b.f32(1e-8);
    let len2 = b.fmax(len2, tiny);
    let len = b.fsqrt(len2);
    let rl = b.frcp(len);
    let n = [b.fmul(n[0], rl), b.fmul(n[1], rl), b.fmul(n[2], rl)];
    let sun = uni3(&mut b, U_SUN_DIR);
    let sun_c = uni3(&mut b, U_SUN_COL);
    let sky = uni3(&mut b, U_SKY);
    let ground = uni3(&mut b, U_GROUND);
    let fog = uni3(&mut b, U_FOG);
    let cam = uni3(&mut b, U_CAMERA);
    let tint = [
        b.uniform(U_TINT, Ty::F32),
        b.uniform(U_TINT + 1, Ty::F32),
        b.uniform(U_TINT + 2, Ty::F32),
        b.uniform(U_TINT + 3, Ty::F32),
    ];
    let emissive = b.uniform(U_EMISSIVE, Ty::F32);
    let ndl = dot3(&mut b, n, sun);
    let ndl = b.fsat(ndl);
    let half = b.f32(0.5);
    let h = b.fma(n[1], half, half);
    // Fog: 1 - 2^(-density * distance / ln 2).
    let d = [
        b.fsub(w[0], cam[0]),
        b.fsub(w[1], cam[1]),
        b.fsub(w[2], cam[2]),
    ];
    let d2 = dot3(&mut b, d, d);
    let dist = b.fsqrt(d2);
    let density = b.uniform(U_FOG_DENSITY, Ty::F32);
    let k = b.f32(-1.442_695);
    let e = b.fmul(density, k);
    let e = b.fmul(e, dist);
    let t = b.fex2(e);
    let one = b.f32(1.0);
    let f = b.fsub(one, t);
    let f = b.fsat(f);
    let zero = b.f32(0.0);
    // The sun's share from the ray-traced mask, filtered at this pixel.
    let shadow = b.copy(one);
    let on = b.uniform(U_SHADOW_ON, Ty::F32);
    let enabled = b.cmp(CmpOp::Gt, on, zero);
    b.if_(enabled, |b| {
        let low = [
            b.uniform(U_SHADOW_W, Ty::F32),
            b.uniform(U_SHADOW_H, Ty::F32),
        ];
        let rw = b.frcp(clip[2]);
        let (nx, ny) = (b.fmul(clip[0], rw), b.fmul(clip[1], rw));
        let half = b.f32(0.5);
        let (hw, hh) = (b.fmul(low[0], half), b.fmul(low[1], half));
        let mhh = b.fneg(hh);
        let (ox, oy) = (b.fsub(hw, half), b.fsub(hh, half));
        let px = b.fma(nx, hw, ox);
        let py = b.fma(ny, mhh, oy);
        // Neighbouring texels of a surface seen at a grazing angle are far
        // apart in depth: the tolerance grows as the view turns flat.
        let to_cam = [
            b.fsub(cam[0], w[0]),
            b.fsub(cam[1], w[1]),
            b.fsub(cam[2], w[2]),
        ];
        let facing = dot3(b, n, to_cam);
        let facing = b.fabs(facing);
        let floor = b.f32(0.08);
        let across = b.fmul(dist, floor);
        let facing = b.fmax(facing, across);
        let base = b.f32(0.15);
        let rel = b.f32(0.04);
        let scaled = b.fmul(dist, rel);
        let rf = b.frcp(facing);
        let grazing = b.fmul(scaled, dist);
        let tol = b.fma(grazing, rf, base);
        let f = shadow_factor(b, 0, px, py, dist, tol, low);
        b.mov(shadow, f);
    });
    let sun_term = b.fmul(ndl, shadow);
    for i in 0..3 {
        let albedo = b.fmul(col[i], tint[i]);
        let span = b.fsub(sky[i], ground[i]);
        let hemi = b.fma(span, h, ground[i]);
        let light = b.fma(sun_c[i], sun_term, hemi);
        let light = b.fadd(light, emissive);
        let lit = b.fmul(albedo, light);
        let to_fog = b.fsub(fog[i], lit);
        let mixed = b.fma(to_fog, f, lit);
        let mixed = b.fmax(mixed, zero);
        // Gamma 2.
        let out = b.fsqrt(mixed);
        b.output(OutKind::Color(0), i as u8, out);
    }
    let a = b.fmul(col[3], tint[3]);
    b.output(OutKind::Color(0), 3, a);
    b.finish()
}

/// The shadow pre-pass: the world position of the surface (pushed off it
/// along the normal), and in w its distance from the camera, or 0 where
/// the sun does not reach it anyway (facing away) and no ray is needed.
pub fn position_fragment() -> Program {
    let mut b = Builder::new(Stage::Fragment);
    let n = [b.input(0, 0), b.input(0, 1), b.input(0, 2)];
    let w = [b.input(2, 0), b.input(2, 1), b.input(2, 2)];
    let len2 = dot3(&mut b, n, n);
    let tiny = b.f32(1e-8);
    let len2 = b.fmax(len2, tiny);
    let len = b.fsqrt(len2);
    let rl = b.frcp(len);
    let n = [b.fmul(n[0], rl), b.fmul(n[1], rl), b.fmul(n[2], rl)];
    let sun = uni3(&mut b, U_SUN_DIR);
    let cam = uni3(&mut b, U_CAMERA);
    let ndl = dot3(&mut b, n, sun);
    let lit = b.fsat(ndl);
    // A little more offset where the sun grazes the surface.
    let one = b.f32(1.0);
    let graze = b.fsub(one, lit);
    let (slope, base) = (b.f32(0.06), b.f32(0.04));
    let bias = b.fma(graze, slope, base);
    for i in 0..3 {
        let p = b.fma(n[i], bias, w[i]);
        b.output(OutKind::Color(0), i as u8, p);
    }
    let d = [
        b.fsub(w[0], cam[0]),
        b.fsub(w[1], cam[1]),
        b.fsub(w[2], cam[2]),
    ];
    let d2 = dot3(&mut b, d, d);
    let dist = b.fsqrt(d2);
    let zero = b.f32(0.0);
    let floor = b.f32(0.02);
    let facing = b.cmp(CmpOp::Gt, ndl, floor);
    let out = b.sel(facing, dist, zero);
    b.output(OutKind::Color(0), 3, out);
    b.finish()
}

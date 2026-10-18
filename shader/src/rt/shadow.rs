//! Ray-traced sun shadows for a raster game. A pre-pass draws the world
//! positions of the visible surfaces at reduced resolution into an
//! RGBA32F target (xyz: the point, pushed off the surface; w: the distance
//! from the camera, 0 where no ray is needed). `kernel` reads that target
//! and shoots one any-hit ray toward the sun per texel, over the scene's
//! tree and over the boxes of the moving figure, and writes one word per
//! texel: the distance with its sign telling shadowed (negative) from
//! lit, 0 for none. The lit fragment shader asks `lit_factor` for the
//! mask at its pixel: a joint bilateral upsample that weighs the four
//! nearest texels by how close their distance is to its own.
//!
//! The kernel's groups are 8 x 4 threads so that a warp is a block of
//! screen: neighbouring rays take the same path.

use super::kernel::walk;
use super::STACK_WORDS;
use crate::{Builder, Builtin, CmpOp, Program, Reg, Stage, Ty};

/// Threads of a group: x, y.
pub const GROUP: [u32; 2] = [8, 4];
/// Hits nearer than this to the start of a shadow ray do not count.
pub const T_MIN: f32 = 0.05;
/// Oriented boxes of the moving figure that cast shadows.
pub const BOXES: usize = 6;
/// Words of a box: three axes (the world directions of its local x, y,
/// z), the origin of its local frame, the minimum and the maximum corner
/// in that frame.
pub const BOX_WORDS: u16 = 18;

/// Uniform words of the kernel.
pub mod u {
    /// Size of the position target (U32).
    pub const W: u16 = 0;
    pub const H: u16 = 1;
    /// Unit vector toward the light.
    pub const SUN: u16 = 2;
    /// Longest shadow ray.
    pub const T_MAX: u16 = 5;
    /// 1 / W, 1 / H.
    pub const INV_W: u16 = 6;
    pub const INV_H: u16 = 7;
    /// Bounding sphere of the figure: centre, radius.
    pub const SPHERE: u16 = 8;
    pub const BOXES: u16 = 12;
    pub const WORDS: usize = BOXES as usize + super::BOXES * super::BOX_WORDS as usize;
}

/// Which buffer slot holds what.
#[derive(Clone, Copy, Debug)]
pub struct Slots {
    pub out: u8,
    pub nodes: u8,
    pub tris: u8,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            out: 0,
            nodes: 2,
            tris: 3,
        }
    }
}

fn dot(b: &mut Builder, a: [Reg; 3], c: [Reg; 3]) -> Reg {
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

/// Whether the ray from `o` along `d` (distances from `t_min`, up to
/// `t_max`) goes through box `k`.
fn hits_box(b: &mut Builder, k: usize, o: [Reg; 3], d: [Reg; 3], t_min: Reg, t_max: Reg) -> Reg {
    let at = u::BOXES + k as u16 * BOX_WORDS;
    let axes = [uni3(b, at), uni3(b, at + 3), uni3(b, at + 6)];
    let origin = uni3(b, at + 9);
    let (lo, hi) = (uni3(b, at + 12), uni3(b, at + 15));
    let rel = [
        b.fsub(o[0], origin[0]),
        b.fsub(o[1], origin[1]),
        b.fsub(o[2], origin[2]),
    ];
    let tiny = b.f32(1e-20);
    let (mut tn, mut tf) = (t_min, t_max);
    for a in 0..3 {
        let lp = dot(b, rel, axes[a]);
        let ld = dot(b, d, axes[a]);
        let m = b.fabs(ld);
        let small = b.cmp(CmpOp::Lt, m, tiny);
        let safe = b.sel(small, tiny, ld);
        let inv = b.frcp(safe);
        let (l, h) = (b.fsub(lo[a], lp), b.fsub(hi[a], lp));
        let (t1, t2) = (b.fmul(l, inv), b.fmul(h, inv));
        let near = b.fmin(t1, t2);
        let far = b.fmax(t1, t2);
        tn = b.fmax(tn, near);
        tf = b.fmin(tf, far);
    }
    b.cmp(CmpOp::Le, tn, tf)
}

/// The shadow kernel: texture 0 is the position target, `u` the uniforms.
pub fn kernel(s: Slots) -> Program {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(GROUP[0] as u16, GROUP[1] as u16, 1);
    b.local_words(STACK_WORDS as u16);
    let (lx, ly) = (b.builtin(Builtin::LocalIdX), b.builtin(Builtin::LocalIdY));
    let (gx, gy) = (b.builtin(Builtin::GroupIdX), b.builtin(Builtin::GroupIdY));
    let (three, two) = (
        b.u32(GROUP[0].trailing_zeros()),
        b.u32(GROUP[1].trailing_zeros()),
    );
    let (gx8, gy4) = (b.shl(gx, three), b.shl(gy, two));
    let x = b.uadd(gx8, lx);
    let y = b.uadd(gy4, ly);
    let (w, h) = (b.uniform(u::W, Ty::U32), b.uniform(u::H, Ty::U32));
    let in_x = b.cmp(CmpOp::Lt, x, w);
    let in_y = b.cmp(CmpOp::Lt, y, h);
    let live = b.band(in_x, in_y);
    b.if_(live, |b| {
        let (xf, yf) = (b.u2f(x), b.u2f(y));
        let half = b.f32(0.5);
        let (xc, yc) = (b.fadd(xf, half), b.fadd(yf, half));
        let (iw, ih) = (b.uniform(u::INV_W, Ty::F32), b.uniform(u::INV_H, Ty::F32));
        let (tu, tv) = (b.fmul(xc, iw), b.fmul(yc, ih));
        let lod = b.f32(0.0);
        let p = b.sample_lod(0, tu, tv, lod);
        let dist = p[3];
        let zero_f = b.f32(0.0);
        let value = b.copy(zero_f);
        let needed = b.cmp(CmpOp::Gt, dist, zero_f);
        b.if_(needed, |b| {
            let o = [p[0], p[1], p[2]];
            let d = uni3(b, u::SUN);
            // Hits closer than this are the surface's own (flat triangles against
            // a smooth normal).
            let t_min = b.f32(T_MIN);
            let t_max = b.uniform(u::T_MAX, Ty::F32);
            let wk = walk(b, s.nodes, s.tris, o, d, t_min, t_max, true, false);
            let blocked = b.copy(wk.found);
            // The figure: a sphere test first, then its boxes.
            let c = uni3(b, u::SPHERE);
            let r = b.uniform(u::SPHERE + 3, Ty::F32);
            let to = [b.fsub(c[0], o[0]), b.fsub(c[1], o[1]), b.fsub(c[2], o[2])];
            let along = dot(b, to, d);
            let all = dot(b, to, to);
            let aa = b.fmul(along, along);
            let perp = b.fsub(all, aa);
            let r2 = b.fmul(r, r);
            let near = b.cmp(CmpOp::Le, perp, r2);
            let nr = b.fneg(r);
            let ahead = b.cmp(CmpOp::Gt, along, nr);
            let close = b.band(near, ahead);
            let zero_u = b.u32(0);
            let free = b.cmp(CmpOp::Eq, blocked, zero_u);
            let test = b.band(close, free);
            b.if_(test, |b| {
                let one_u = b.u32(1);
                for k in 0..BOXES {
                    let hit = hits_box(b, k, o, d, t_min, t_max);
                    let nb = b.sel(hit, one_u, blocked);
                    b.mov(blocked, nb);
                }
            });
            let shadowed = b.cmp(CmpOp::Ne, blocked, zero_u);
            let neg = b.fneg(dist);
            let v = b.sel(shadowed, neg, dist);
            b.mov(value, v);
        });
        let row = b.umul(y, w);
        let at = b.uadd(row, x);
        let bits = b.fbits(value);
        b.store(s.out, at, 0, &[bits]);
    });
    b.finish()
}

/// The lit fragment shader's share of the sun: 1 when the pixel is lit, 0
/// in shadow, between where the mask's four nearest texels disagree.
/// `px`, `py`: the pixel in texel coordinates of the mask, texel centres
/// at whole numbers; `dist`: its distance from the camera; `tol`: how far
/// from `dist` a texel's distance may be and still count (more on a
/// surface seen at a grazing angle, where neighbours differ much); `low`:
/// the mask's size (F32); `buf`: the slot holding the mask.
pub fn lit_factor(
    b: &mut Builder,
    buf: u8,
    px: Reg,
    py: Reg,
    dist: Reg,
    tol: Reg,
    low: [Reg; 2],
) -> Reg {
    lit_factor_ex(b, buf, px, py, dist, tol, low).0
}

/// `lit_factor`, and whether the texels agreed well enough to be
/// filtered (false: the nearest in depth decided).
pub fn lit_factor_ex(
    b: &mut Builder,
    buf: u8,
    px: Reg,
    py: Reg,
    dist: Reg,
    tol: Reg,
    low: [Reg; 2],
) -> (Reg, Reg) {
    let (zero, one) = (b.f32(0.0), b.f32(1.0));
    let (x0, y0) = (b.ffloor(px), b.ffloor(py));
    let (fx, fy) = (b.fsub(px, x0), b.fsub(py, y0));
    let (gx, gy) = (b.fsub(one, fx), b.fsub(one, fy));
    let one_f = one;
    let (xmax, ymax) = (b.fsub(low[0], one_f), b.fsub(low[1], one_f));
    // Texel coordinates, clamped to the mask.
    let clamp = |b: &mut Builder, v: Reg, hi: Reg| {
        let v = b.fmax(v, zero);
        b.fmin(v, hi)
    };
    let (x1, y1) = (b.fadd(x0, one), b.fadd(y0, one));
    let xs = [clamp(b, x0, xmax), clamp(b, x1, xmax)];
    let ys = [clamp(b, y0, ymax), clamp(b, y1, ymax)];
    let wu = b.f2u(low[0]);
    let inv_tol = b.frcp(tol);
    let huge = b.f32(1e9);
    let (mut sum_w, mut sum_l) = (zero, zero);
    let (mut best_e, mut best_l) = (huge, one);
    for (j, yv) in ys.iter().enumerate() {
        let yu = b.f2u(*yv);
        let row = b.umul(yu, wu);
        for (i, xv) in xs.iter().enumerate() {
            let xu = b.f2u(*xv);
            let at = b.uadd(row, xu);
            let s = b.load(buf, at, 0, Ty::F32);
            let valid = b.cmp(CmpOp::Ne, s, zero);
            let positive = b.cmp(CmpOp::Gt, s, zero);
            let d = b.fabs(s);
            let diff = b.fsub(d, dist);
            let e = b.fabs(diff);
            let scaled = b.fmul(e, inv_tol);
            let closeness = b.fsub(one, scaled);
            let wd = b.fmax(closeness, zero);
            let (wx, wy) = (if i == 0 { gx } else { fx }, if j == 0 { gy } else { fy });
            let bw = b.fmul(wx, wy);
            let weight = b.fmul(bw, wd);
            let weight = b.sel(valid, weight, zero);
            // An empty texel counts as lit.
            let lit = b.sel(positive, one, zero);
            let lit = b.sel(valid, lit, one);
            sum_w = b.fadd(sum_w, weight);
            sum_l = b.fma(weight, lit, sum_l);
            let err = b.sel(valid, e, huge);
            let better = b.cmp(CmpOp::Lt, err, best_e);
            best_e = b.sel(better, err, best_e);
            best_l = b.sel(better, lit, best_l);
        }
    }
    let enough = b.f32(0.02);
    let strong = b.cmp(CmpOp::Gt, sum_w, enough);
    let tiny = b.f32(1e-6);
    let denom = b.fmax(sum_w, tiny);
    let rcp = b.frcp(denom);
    let mean = b.fmul(sum_l, rcp);
    (b.sel(strong, mean, best_l), strong)
}

//! Ray-traced sun shadows for a raster game. A pre-pass draws the world
//! positions of the visible surfaces at reduced resolution into an
//! RGBA32F target (xyz: the point, pushed off the surface; w: the distance
//! from the camera, 0 where no ray is needed). `kernel` reads that target
//! and shoots an any-hit ray toward the sun per texel over the scene's
//! tree, and writes `TEXEL_WORDS` words per texel: the distance (0 where
//! no ray was needed), the share of the sun that reaches it, the scene's
//! share alone, and whether the texel traced. The scene's share is
//! accumulated over frames: the texel's world position is projected with
//! the last frame's camera, the mask of the last frame (the other buffer
//! of a ping-pong pair) is read there, accepted when its distance agrees,
//! and blended with a new sample; the ray leaves in a direction jittered
//! inside the sun's disc, per texel and frame, which turns the edges into
//! penumbrae; half the texels (a checkerboard that swaps every frame)
//! trace and the rest keep their history, unless it was rejected or its
//! point did not trace last frame. The moving figure's boxes keep no
//! history: four rays across the sun's disc test them every frame, near
//! its bounding sphere, and their share multiplies the scene's. The lit
//! fragment shader asks
//! `lit_factor` for the mask at its pixel: a joint bilateral upsample that
//! weighs the four nearest texels by how close their distance is to its
//! own.
//!
//! The kernel's groups are 8 x 4 threads so that a warp is a block of
//! screen: neighbouring rays take the same path.

use super::kernel::walk;
use super::STACK_WORDS;
use crate::{Builder, Builtin, CmpOp, Program, Reg, Stage, Ty};

/// Threads of a group: x, y.
pub const GROUP: [u32; 2] = [8, 4];
/// Hits nearer than this to the start of a shadow ray do not count.
pub const T_MIN: f32 = 0.10;
/// A shadow ray starts this far off the surface along its normal, plus
/// `BIAS_SLOPE` times how much the light grazes it.
pub const BIAS: f32 = 0.10;
pub const BIAS_SLOPE: f32 = 0.15;
/// The same for the figure's boxes, which are exact.
pub const FIGURE_BIAS: f32 = 0.02;
pub const FIGURE_T_MIN: f32 = 0.01;
/// Oriented boxes of the moving figure that cast shadows.
pub const BOXES: usize = 6;
/// Words of a texel of the mask: the distance, the share of the sun, the
/// scene's share alone (the accumulated one), and 1 if it traced (2 if
/// it did because its history was rejected).
pub const TEXEL_WORDS: usize = 4;
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
    /// The camera of the last frame: its view-projection (4 x 4, rows) and
    /// position.
    pub const PREV_VP: u16 = BOXES + (super::BOXES as u16) * super::BOX_WORDS;
    pub const PREV_CAM: u16 = PREV_VP + 16;
    /// The frame number (U32), the weight of a new sample, the radius of
    /// the sun's disc as a tangent, and a flag (> 0.5) to forget history.
    pub const FRAME: u16 = PREV_CAM + 3;
    pub const ALPHA: u16 = FRAME + 1;
    pub const JITTER: u16 = FRAME + 2;
    pub const RESET: u16 = FRAME + 3;
    /// Two unit vectors perpendicular to the sun and to each other.
    pub const SUN_U: u16 = FRAME + 4;
    pub const SUN_V: u16 = FRAME + 7;
    /// The camera now.
    pub const CAM: u16 = FRAME + 10;
    pub const WORDS: usize = CAM as usize + 3;
}

/// Which buffer slot holds what.
#[derive(Clone, Copy, Debug)]
pub struct Slots {
    /// This frame's mask (written; the lit shader reads it) and last
    /// frame's (read).
    pub out: u8,
    pub hist: u8,
    pub nodes: u8,
    pub tris: u8,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            out: 0,
            hist: 1,
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

/// Whether the ray from `o` along `d` enters the box whose words start at
/// `base` (a U32 register, words from `u::BOXES`) between `t_min` and
/// `t_max`. A box that holds `o` does not count: the boxes are a little
/// larger than the meshes, and a point of a mesh may lie inside its own.
fn hits_box(b: &mut Builder, base: Reg, o: [Reg; 3], d: [Reg; 3], t_min: Reg, t_max: Reg) -> Reg {
    let word = |b: &mut Builder, k: u16| b.uniform_idx(u::BOXES + k, base, Ty::F32);
    let v3 = |b: &mut Builder, at: u16| [word(b, at), word(b, at + 1), word(b, at + 2)];
    let axes = [v3(b, 0), v3(b, 3), v3(b, 6)];
    let origin = v3(b, 9);
    let (lo, hi) = (v3(b, 12), v3(b, 15));
    let rel = [
        b.fsub(o[0], origin[0]),
        b.fsub(o[1], origin[1]),
        b.fsub(o[2], origin[2]),
    ];
    let tiny = b.f32(1e-20);
    let (mut tn, mut tf) = (b.f32(-1.0e30), t_max);
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
    let through = b.cmp(CmpOp::Le, tn, tf);
    let ahead = b.cmp(CmpOp::Ge, tn, t_min);
    b.band(through, ahead)
}

/// Whether the ray from `o` along unit `d` goes near the sphere (centre
/// and radius at uniform word `at`).
fn near_sphere(b: &mut Builder, at: u16, o: [Reg; 3], d: [Reg; 3]) -> Reg {
    let c = uni3(b, at);
    let r = b.uniform(at + 3, Ty::F32);
    let to = [b.fsub(c[0], o[0]), b.fsub(c[1], o[1]), b.fsub(c[2], o[2])];
    let along = dot(b, to, d);
    let all = dot(b, to, to);
    let aa = b.fmul(along, along);
    let perp = b.fsub(all, aa);
    let r2 = b.fmul(r, r);
    let near = b.cmp(CmpOp::Le, perp, r2);
    let nr = b.fneg(r);
    let ahead = b.cmp(CmpOp::Gt, along, nr);
    b.band(near, ahead)
}

/// What the pre-pass puts in w: 0 for no ray needed, else the unit normal
/// as 1 + sign(nz) * 2^22 + x * 2^11 + y with x, y in 0..2047 (every value
/// an exact float below 2^24).
pub fn pack_normal(b: &mut Builder, n: [Reg; 3]) -> Reg {
    let (half, scale, round) = (b.f32(0.5), b.f32(2047.0), b.f32(0.5));
    let mut e = [b.u32(0); 2];
    for k in 0..2 {
        let t = b.fma(n[k], half, half);
        let t = b.fsat(t);
        let t = b.fma(t, scale, round);
        e[k] = b.f2u(t);
    }
    let (eleven, twenty_two) = (b.u32(11), b.u32(22));
    // The sign of nz as 0 or 1 without a select: -nz scaled up and
    // saturated, rounded.
    let (big, round) = (b.f32(1.0e6), b.f32(0.5));
    let mz = b.fneg(n[2]);
    let sat = b.fmul(mz, big);
    let sat = b.fsat(sat);
    let sat = b.fadd(sat, round);
    let sign = b.f2u(sat);
    let sg = b.shl(sign, twenty_two);
    let xs = b.shl(e[0], eleven);
    let bits = b.or(sg, xs);
    let bits = b.or(bits, e[1]);
    let v = b.u2f(bits);
    let one = b.f32(1.0);
    b.fadd(v, one)
}

/// The normal `pack_normal` packed.
fn unpack_normal(b: &mut Builder, packed: Reg) -> [Reg; 3] {
    let one = b.f32(1.0);
    let v = b.fsub(packed, one);
    let bits = b.f2u(v);
    let (mask, eleven, twenty_two) = (b.u32(2047), b.u32(11), b.u32(22));
    let ey = b.and(bits, mask);
    let ex_s = b.shr(bits, eleven);
    let ex = b.and(ex_s, mask);
    let sg = b.shr(bits, twenty_two);
    let (inv, two, minus) = (b.f32(1.0 / 2047.0), b.f32(2.0), b.f32(-1.0));
    let mut n = [one; 3];
    for (k, e) in [ex, ey].into_iter().enumerate() {
        let f = b.u2f(e);
        let t = b.fmul(f, inv);
        n[k] = b.fma(t, two, minus);
    }
    let (x2, y2) = (b.fmul(n[0], n[0]), b.fmul(n[1], n[1]));
    let s2 = b.fadd(x2, y2);
    let rest = b.fsub(one, s2);
    let zero = b.f32(0.0);
    let rest = b.fmax(rest, zero);
    let z = b.fsqrt(rest);
    let zero_u = b.u32(0);
    let flip = b.cmp(CmpOp::Ne, sg, zero_u);
    let nz = b.fneg(z);
    n[2] = b.sel(flip, nz, z);
    n
}

/// The unit direction `sun + ra * SUN_U + rb * SUN_V`.
fn disc_dir(b: &mut Builder, sun: [Reg; 3], ra: Reg, rb: Reg) -> [Reg; 3] {
    let (su, sv) = (uni3(b, u::SUN_U), uni3(b, u::SUN_V));
    let mut d = sun;
    for k in 0..3 {
        let t0 = b.fma(su[k], ra, sun[k]);
        d[k] = b.fma(sv[k], rb, t0);
    }
    let len2 = dot(b, d, d);
    let rl = b.frsq(len2);
    [b.fmul(d[0], rl), b.fmul(d[1], rl), b.fmul(d[2], rl)]
}

/// `v` modulo 1.
fn fract(b: &mut Builder, v: Reg) -> Reg {
    let f = b.ffloor(v);
    b.fsub(v, f)
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
        let packed = p[3];
        let (zero_f, one_f) = (b.f32(0.0), b.f32(1.0));
        // What this texel says: no distance, all lit, until a ray decides.
        let out_dist = b.copy(zero_f);
        let out_cov = b.copy(one_f);
        let out_scene = b.copy(one_f);
        let out_traced = b.copy(zero_f);
        let needed = b.cmp(CmpOp::Gt, packed, zero_f);
        b.if_(needed, |b| {
            // The point on the surface, its distance from the camera, and
            // the normal (packed in w, see `pack_normal`).
            let o = [p[0], p[1], p[2]];
            let cam = uni3(b, u::CAM);
            let to_cam = [
                b.fsub(o[0], cam[0]),
                b.fsub(o[1], cam[1]),
                b.fsub(o[2], cam[2]),
            ];
            let d2 = dot(b, to_cam, to_cam);
            let dist = b.fsqrt(d2);
            b.mov(out_dist, dist);
            let n = unpack_normal(b, packed);

            // Where this point was on the last frame's mask, and what the four
            // texels around it said, each weighted by how near it is and by
            // how well its distance agrees (a nearest texel snaps the history
            // to the grid as the camera moves, and picks the wrong surface at
            // edges).
            let m = |b: &mut Builder, r: u16, c: u16| b.uniform(u::PREV_VP + 4 * r + c, Ty::F32);
            let mut clip = [zero_f; 3];
            for (k, row) in [0u16, 1, 3].into_iter().enumerate() {
                let (a0, a1, a2, a3) = (m(b, row, 0), m(b, row, 1), m(b, row, 2), m(b, row, 3));
                let t0 = b.fma(a0, o[0], a3);
                let t1 = b.fma(a1, o[1], t0);
                clip[k] = b.fma(a2, o[2], t1);
            }
            let min_w = b.f32(0.01);
            let in_front = b.cmp(CmpOp::Gt, clip[2], min_w);
            let safe_w = b.fmax(clip[2], min_w);
            let rw = b.frcp(safe_w);
            let (nx, ny) = (b.fmul(clip[0], rw), b.fmul(clip[1], rw));
            let (wf, hf) = (b.u2f(w), b.u2f(h));
            let (hw, hh) = (b.fmul(wf, half), b.fmul(hf, half));
            let (ox, oy) = (b.fsub(hw, half), b.fsub(hh, half));
            // The viewport: window = ndc * size / 2 + size / 2, both axes;
            // texel centres at whole numbers.
            let (px, py) = (b.fma(nx, hw, ox), b.fma(ny, hh, oy));
            let (x0, y0) = (b.ffloor(px), b.ffloor(py));
            let (fx, fy) = (b.fsub(px, x0), b.fsub(py, y0));
            let (gx, gy) = (b.fsub(one_f, fx), b.fsub(one_f, fy));
            let (wmax, hmax) = (b.fsub(wf, one_f), b.fsub(hf, one_f));
            let pc = uni3(b, u::PREV_CAM);
            let rel = [
                b.fsub(o[0], pc[0]),
                b.fsub(o[1], pc[1]),
                b.fsub(o[2], pc[2]),
            ];
            let r2 = dot(b, rel, rel);
            let prev_dist = b.fsqrt(r2);
            let (rel_tol, base_tol) = (b.f32(0.06), b.f32(0.2));
            let tol = b.fma(prev_dist, rel_tol, base_tol);
            let inv_tol = b.frcp(tol);
            let one_u = b.u32(1);
            let two_u = b.u32(2);
            let (sum_w, sum_c, sum_t) = (b.copy(zero_f), b.copy(zero_f), b.copy(zero_f));
            for j in 0..2 {
                let yv = if j == 0 { y0 } else { b.fadd(y0, one_f) };
                let in_y = {
                    let a = b.cmp(CmpOp::Ge, yv, zero_f);
                    let c = b.cmp(CmpOp::Le, yv, hmax);
                    b.band(a, c)
                };
                let cy = b.fmax(yv, zero_f);
                let cy = b.fmin(cy, hmax);
                let yi = b.f2u(cy);
                let row = b.umul(yi, w);
                for i in 0..2 {
                    let xv = if i == 0 { x0 } else { b.fadd(x0, one_f) };
                    let in_x = {
                        let a = b.cmp(CmpOp::Ge, xv, zero_f);
                        let c = b.cmp(CmpOp::Le, xv, wmax);
                        b.band(a, c)
                    };
                    let cx = b.fmax(xv, zero_f);
                    let cx = b.fmin(cx, wmax);
                    let xi = b.f2u(cx);
                    let hidx = b.uadd(row, xi);
                    let at4 = b.shl(hidx, two_u);
                    let past = b.load_n(s.hist, at4, 0, &[Ty::F32; TEXEL_WORDS]);
                    let (hd, hc, hfl) = (past[0], past[2], past[3]);
                    let diff = b.fsub(hd, prev_dist);
                    let diff = b.fabs(diff);
                    let scaled = b.fmul(diff, inv_tol);
                    let close = b.fsub(one_f, scaled);
                    let close = b.fmax(close, zero_f);
                    let bw = b.fmul(if i == 0 { gx } else { fx }, if j == 0 { gy } else { fy });
                    let wt = b.fmul(bw, close);
                    let had = b.cmp(CmpOp::Gt, hd, zero_f);
                    let ok = b.band(in_x, in_y);
                    let ok = b.band(ok, had);
                    let wt = b.sel(ok, wt, zero_f);
                    let nw = b.fadd(sum_w, wt);
                    b.mov(sum_w, nw);
                    let nc = b.fma(wt, hc, sum_c);
                    b.mov(sum_c, nc);
                    let was = b.cmp(CmpOp::Gt, hfl, half);
                    let tw = b.sel(was, wt, zero_f);
                    let nt = b.fadd(sum_t, tw);
                    b.mov(sum_t, nt);
                }
            }
            // Too little weight: the point was hidden or off the mask.
            let enough = b.f32(0.05);
            let agrees = b.cmp(CmpOp::Gt, sum_w, enough);
            let denom = b.fmax(sum_w, enough);
            let rcp = b.frcp(denom);
            let hc = b.fmul(sum_c, rcp);
            // The point traced last frame if most of its weight did.
            let half_w = b.fmul(sum_w, half);
            let traced_before = b.cmp(CmpOp::Gt, sum_t, half_w);
            let reset = b.uniform(u::RESET, Ty::F32);
            let keep = b.cmp(CmpOp::Le, reset, half);
            let valid = b.band(in_front, agrees);
            let valid = b.band(valid, keep);

            // Half the texels trace this frame; one whose history was
            // rejected, or whose point did not trace last frame, always does
            // (a point that moves an odd number of texels a frame would
            // never trace otherwise: the checkerboard is the screen's).
            let frame = b.uniform(u::FRAME, Ty::U32);
            let xy = b.uadd(x, y);
            let par = b.uadd(xy, frame);
            let low_bit = b.and(par, one_u);
            let zero_u = b.u32(0);
            let mine = b.cmp(CmpOp::Eq, low_bit, zero_u);
            let invalid = b.bnot(valid);
            let trace = b.bor(mine, invalid);
            let stale = b.bnot(traced_before);
            let trace = b.bor(trace, stale);
            // 1: traced; 2: traced because the history was rejected (the
            // Lab's mask view shows those).
            let traced = b.sel(trace, one_f, zero_f);
            let two_f = b.f32(2.0);
            let traced = b.sel(invalid, two_f, traced);
            let cov = b.copy(hc);
            // `valid` is needed after the walk: kept as a number meanwhile, so
            // that no predicate is held across the traversal.
            let (one_n, zero_n) = (b.u32(1), b.u32(0));
            let valid_n = b.sel(valid, one_n, zero_n);
            // The ray starts off the surface along the normal, more where
            // the light grazes it.
            let sun = uni3(b, u::SUN);
            let sun_n = dot(b, n, sun);
            let lit_n = b.fsat(sun_n);
            let graze = b.fsub(one_f, lit_n);
            let (slope, base) = (b.f32(BIAS_SLOPE), b.f32(BIAS));
            let bias = b.fma(graze, slope, base);
            let surface = o;
            let o = [
                b.fma(n[0], bias, o[0]),
                b.fma(n[1], bias, o[1]),
                b.fma(n[2], bias, o[2]),
            ];
            // Hits closer than this are the surface's own (flat triangles
            // against a smooth normal).
            let t_min = b.f32(T_MIN);
            let t_max = b.uniform(u::T_MAX, Ty::F32);
            let radius = b.uniform(u::JITTER, Ty::F32);
            b.if_(trace, |b| {
                // A direction in the sun's disc: a point of the R2
                // sequence, shifted per texel, advancing with the frame.
                let k13 = b.u32(2_654_435_761);
                let k7 = b.u32(2_246_822_519);
                let (hx32, hy32) = (b.umul(x, k13), b.umul(y, k7));
                let seed = b.xor(hx32, hy32);
                let eight = b.u32(8);
                let (sa, sb) = (b.shr(seed, eight), b.umul(seed, k13));
                let sb = b.shr(sb, eight);
                let scale = b.f32(1.0 / 16_777_216.0);
                let (ja, jb) = (b.u2f(sa), b.u2f(sb));
                let (ja, jb) = (b.fmul(ja, scale), b.fmul(jb, scale));
                let mask = b.u32(1023);
                let fm = b.and(frame, mask);
                let fmf = b.u2f(fm);
                let (g1, g2) = (b.f32(0.754_877_7), b.f32(0.569_840_3));
                let (a0, b0) = (b.fma(fmf, g1, ja), b.fma(fmf, g2, jb));
                let (a1, b1) = (fract(b, a0), fract(b, b0));
                let (two_f, minus) = (b.f32(2.0), b.f32(-1.0));
                let (a2, b2) = (b.fma(a1, two_f, minus), b.fma(b1, two_f, minus));
                let (ra, rb) = (b.fmul(a2, radius), b.fmul(b2, radius));
                let d = disc_dir(b, sun, ra, rb);
                let wk = walk(b, s.nodes, s.tris, o, d, t_min, t_max, true, false);
                let shadowed = b.cmp(CmpOp::Ne, wk.found, zero_u);
                let lit = b.sel(shadowed, zero_f, one_f);
                let alpha = b.uniform(u::ALPHA, Ty::F32);
                let delta = b.fsub(lit, hc);
                let blend = b.fma(delta, alpha, hc);
                let valid_again = b.cmp(CmpOp::Ne, valid_n, zero_n);
                let fresh = b.sel(valid_again, blend, lit);
                b.mov(cov, fresh);
            });
            // The figure, every frame and without history: the share of
            // four rays across the sun's disc that miss all its boxes.
            let figure = b.copy(one_f);
            // The boxes are exact: their rays start just off the surface
            // (the scene's offset is for flat triangles under smooth
            // normals, and is wider than an arm is from the body).
            let (lift, near) = (b.f32(FIGURE_BIAS), b.f32(FIGURE_T_MIN));
            let o = [
                b.fma(n[0], lift, surface[0]),
                b.fma(n[1], lift, surface[1]),
                b.fma(n[2], lift, surface[2]),
            ];
            let t_min = near;
            let close = near_sphere(b, u::SPHERE, o, sun);
            b.if_(close, |b| {
                let quarter = b.f32(0.25);
                let (half_r, nhalf_r) = (b.fmul(radius, half), b.f32(0.0));
                let nhalf_r = b.fsub(nhalf_r, half_r);
                let ray = b.u32(0);
                let (four, stride, count) =
                    (b.u32(4), b.u32(BOX_WORDS as u32), b.u32(BOXES as u32));
                b.loop_(4, |b| {
                    // Offsets (-+ r/2, -+ r/2) from the ray's two low bits.
                    let bit0 = b.and(ray, one_u);
                    let sh = b.shr(ray, one_u);
                    let bit1 = b.and(sh, one_u);
                    let (p0, p1) = (
                        b.cmp(CmpOp::Ne, bit0, zero_u),
                        b.cmp(CmpOp::Ne, bit1, zero_u),
                    );
                    let ra = b.sel(p0, half_r, nhalf_r);
                    let rb = b.sel(p1, half_r, nhalf_r);
                    let d = disc_dir(b, sun, ra, rb);
                    let blocked = b.copy(zero_u);
                    let k = b.u32(0);
                    b.loop_(BOXES as u32, |b| {
                        let base = b.umul(k, stride);
                        let hit = hits_box(b, base, o, d, t_min, t_max);
                        let nb = b.sel(hit, one_u, blocked);
                        b.mov(blocked, nb);
                        let nk = b.uadd(k, one_u);
                        b.mov(k, nk);
                        let done = b.cmp(CmpOp::Ge, k, count);
                        b.break_if(done);
                    });
                    let hit = b.cmp(CmpOp::Ne, blocked, zero_u);
                    let less = b.fsub(figure, quarter);
                    let nf = b.sel(hit, less, figure);
                    b.mov(figure, nf);
                    let nr = b.uadd(ray, one_u);
                    b.mov(ray, nr);
                    let done = b.cmp(CmpOp::Ge, ray, four);
                    b.break_if(done);
                });
            });
            let share = b.fmul(cov, figure);
            b.mov(out_cov, share);
            b.mov(out_scene, cov);
            b.mov(out_traced, traced);
        });
        let row = b.umul(y, w);
        let at = b.uadd(row, x);
        let two_u = b.u32(2);
        let at4 = b.shl(at, two_u);
        let words = [
            b.fbits(out_dist),
            b.fbits(out_cov),
            b.fbits(out_scene),
            b.fbits(out_traced),
        ];
        b.store(s.out, at4, 0, &words);
    });
    b.finish()
}

/// The lit fragment shader's share of the sun: 1 when the pixel is lit, 0
/// in shadow, between where the mask's four nearest texels disagree (the
/// mask is `Slots::out`: `TEXEL_WORDS` words a texel, the distance and the
/// share first).
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
    lit_factor_ex(b, buf, px, py, dist, tol, low).factor
}

/// What `lit_factor_ex` worked out.
pub struct Filtered {
    pub factor: Reg,
    /// The texels agreed well enough to be filtered.
    pub strong: Reg,
    /// Some texel had a distance.
    pub any: Reg,
    /// The smallest distance error among the four texels, in tolerances.
    pub err: Reg,
    /// The sum of the filter's weights.
    pub weight: Reg,
}

/// `lit_factor`, whether the texels agreed well enough to be filtered
/// (false: the nearest in depth decided), and whether any texel had a
/// distance at all.
pub fn lit_factor_ex(
    b: &mut Builder,
    buf: u8,
    px: Reg,
    py: Reg,
    dist: Reg,
    tol: Reg,
    low: [Reg; 2],
) -> Filtered {
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
            let two_u = b.u32(2);
            let at4 = b.shl(at, two_u);
            let rec = b.load_n(buf, at4, 0, &[Ty::F32; 2]);
            let (s, cov) = (rec[0], rec[1]);
            let valid = b.cmp(CmpOp::Gt, s, zero);
            let e = {
                let diff = b.fsub(s, dist);
                b.fabs(diff)
            };
            let scaled = b.fmul(e, inv_tol);
            let closeness = b.fsub(one, scaled);
            let wd = b.fmax(closeness, zero);
            let (wx, wy) = (if i == 0 { gx } else { fx }, if j == 0 { gy } else { fy });
            let bw = b.fmul(wx, wy);
            let weight = b.fmul(bw, wd);
            let weight = b.sel(valid, weight, zero);
            // An empty texel counts as lit.
            let lit = b.sel(valid, cov, one);
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
    let any = b.cmp(CmpOp::Lt, best_e, huge);
    let err = b.fmul(best_e, inv_tol);
    Filtered {
        factor: b.sel(strong, mean, best_l),
        strong,
        any,
        err,
        weight: sum_w,
    }
}

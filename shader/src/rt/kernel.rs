//! Traversal kernels in the IR. A dispatch of `ceil(rays / GROUP)` groups
//! runs one ray per thread: rays come from a buffer, the answers go to
//! another, the tree and the triangles are two more (`Slots` says which
//! binding slot of the shader each is in). Uniform word 0 is the number of
//! rays. The stack of the tree walk lives in the thread's local memory.
//!
//! A step takes the current reference: a leaf has its triangles tested
//! (Moller-Trumbore, as `trace::intersect`); an inner node has its four
//! boxes tested against the ray's range, the nearest hit child becomes the
//! current reference and the others go on the stack, farthest first.
//! A walk stops after `MAX_STEPS` steps, which only a scene far larger than
//! the ones that fit in memory comes near.

use alloc::vec::Vec;

use super::{HIT_WORDS, LEAF, MISS, NODE_WORDS, RAY_WORDS, STACK_WORDS, TRI_WORDS};
use crate::{Builder, CmpOp, Program, Reg, Stage, Ty};

/// Threads per group.
pub const GROUP: u32 = 64;
pub const MAX_STEPS: u32 = 1024;

/// Which binding slot holds what.
#[derive(Clone, Copy, Debug)]
pub struct Slots {
    pub rays: u8,
    pub hits: u8,
    pub nodes: u8,
    pub tris: u8,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            rays: 0,
            hits: 1,
            nodes: 2,
            tris: 3,
        }
    }
}

type V3 = [Reg; 3];

fn sub(b: &mut Builder, a: V3, c: V3) -> V3 {
    [b.fsub(a[0], c[0]), b.fsub(a[1], c[1]), b.fsub(a[2], c[2])]
}

fn cross(b: &mut Builder, a: V3, c: V3) -> V3 {
    let mut term = |x: Reg, y: Reg, z: Reg, w: Reg| {
        let p = b.fmul(x, y);
        let q = b.fmul(z, w);
        b.fsub(p, q)
    };
    [
        term(a[1], c[2], a[2], c[1]),
        term(a[2], c[0], a[0], c[2]),
        term(a[0], c[1], a[1], c[0]),
    ]
}

fn dot(b: &mut Builder, a: V3, c: V3) -> Reg {
    let x = b.fmul(a[0], c[0]);
    let y = b.fma(a[1], c[1], x);
    b.fma(a[2], c[2], y)
}

/// The nearest hit of every ray: 4 words each (t, u, v, id; `MISS` for
/// none).
pub fn closest_hit(s: Slots) -> Program {
    traversal(s, false)
}

/// Whether anything blocks each ray: 1 word each (1 or 0).
pub fn any_hit(s: Slots) -> Program {
    traversal(s, true)
}

/// The closest-hit kernel with the number of inner nodes and leaves it
/// visited in the hit's `u` and `v` words (a profile aid).
#[cfg(test)]
pub fn closest_hit_counted(s: Slots) -> Program {
    traversal_ex(s, false, true)
}

fn traversal(s: Slots, any: bool) -> Program {
    traversal_ex(s, any, false)
}

fn traversal_ex(s: Slots, any: bool, counted: bool) -> Program {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(GROUP as u16, 1, 1);
    b.local_words(STACK_WORDS as u16);
    let tid = b.builtin(crate::Builtin::LocalIdX);
    let gid = b.builtin(crate::Builtin::GroupIdX);
    let six = b.u32(GROUP.trailing_zeros());
    let g64 = b.shl(gid, six);
    let id = b.uadd(g64, tid);
    let count = b.uniform(0, Ty::U32);
    let live = b.cmp(CmpOp::Lt, id, count);
    b.if_(live, |b| ray(b, s, id, any, counted));
    b.finish()
}

fn ray(b: &mut Builder, s: Slots, id: Reg, any: bool, counted: bool) {
    let three = b.u32(RAY_WORDS.trailing_zeros());
    let rbase = b.shl(id, three);
    let w0 = b.load_n(s.rays, rbase, 0, &[Ty::F32; 4]);
    let w1 = b.load_n(s.rays, rbase, 4, &[Ty::F32; 4]);
    let o = [w0[0], w0[1], w0[2]];
    let d = [w1[0], w1[1], w1[2]];
    let w = walk(b, s.nodes, s.tris, o, d, w0[3], w1[3], any, counted);
    let (best_t, best_u, best_v, best_id, found) = (w.t, w.u, w.v, w.id, w.found);
    let (n_inner, n_leaf) = (w.inner, w.leaf);
    if any {
        b.store(s.hits, id, 0, &[found]);
    } else {
        let two = b.u32(HIT_WORDS.trailing_zeros());
        let hbase = b.shl(id, two);
        let (tb, ub, vb) = (b.fbits(best_t), b.fbits(best_u), b.fbits(best_v));
        if counted {
            b.store(s.hits, hbase, 0, &[tb, n_inner, n_leaf, best_id]);
        } else {
            b.store(s.hits, hbase, 0, &[tb, ub, vb, best_id]);
        }
    }
}

/// What a walk found: the nearest hit (`t`, barycentrics, triangle `id`,
/// `MISS` for none), whether anything at all (any-hit walks stop at the
/// first), and the nodes it visited.
pub struct Walk {
    pub t: Reg,
    pub u: Reg,
    pub v: Reg,
    pub id: Reg,
    pub found: Reg,
    pub inner: Reg,
    pub leaf: Reg,
}

/// One ray through the tree in buffers `nodes` and `tris`: from `o` along
/// `d` (not necessarily unit), distances between `t_min` and `t_max`.
#[allow(clippy::too_many_arguments)]
pub fn walk(
    b: &mut Builder,
    nodes: u8,
    tris: u8,
    o: [Reg; 3],
    d: [Reg; 3],
    t_min: Reg,
    t_max: Reg,
    any: bool,
    counted: bool,
) -> Walk {
    let s = Slots {
        rays: 0,
        hits: 0,
        nodes,
        tris,
    };
    // Slab distances as one FMA each: t = plane * inv + off, with off =
    // -origin * inv. A direction too small to invert is nudged so that
    // inv stays finite (infinities would make NaNs of the products).
    let (zero_f, tiny) = (b.f32(0.0), b.f32(1e-20));
    let mut inv = [zero_f; 3];
    let mut off = [zero_f; 3];
    for a in 0..3 {
        let m = b.fabs(d[a]);
        let small = b.cmp(CmpOp::Lt, m, tiny);
        let safe = b.sel(small, tiny, d[a]);
        inv[a] = b.frcp(safe);
        let p = b.fmul(o[a], inv[a]);
        off[a] = b.fneg(p);
    }
    // A negative direction enters at the max plane and leaves at the min
    // plane: the word offsets of the planes the near and far distances
    // come from, per axis.
    let mut near_at = [b.u32(0); 3];
    let mut far_at = [b.u32(0); 3];
    for a in 0..3 {
        let neg = b.cmp(CmpOp::Lt, inv[a], zero_f);
        let (min_at, max_at) = (b.u32(4 + 4 * a as u32), b.u32(16 + 4 * a as u32));
        near_at[a] = b.sel(neg, max_at, min_at);
        far_at[a] = b.sel(neg, min_at, max_at);
    }
    let guard = b.f32(1.000_000_5);
    let best_t = b.copy(t_max);
    let (best_u, best_v) = (b.f32(0.0), b.f32(0.0));
    let best_id = b.u32(MISS);
    let found = b.u32(0);

    let (zero, one) = (b.u32(0), b.u32(1));
    let inf = b.f32(f32::INFINITY);
    let sp = b.u32(0);
    let cur = b.u32(0);
    let leaf_bit = b.u32(LEAF);
    let need_pop = b.bool(false);
    let (n_inner, n_leaf) = (b.u32(0), b.u32(0));

    b.loop_(MAX_STEPS, |b| {
        let bit = b.and(cur, leaf_bit);
        let is_leaf = b.cmp(CmpOp::Ne, bit, zero);
        b.if_else(
            is_leaf,
            |b| {
                if counted {
                    let nl = b.uadd(n_leaf, one);
                    b.mov(n_leaf, nl);
                }
                // Leaf: LEAF | first << 3 | (count - 1).
                let seven = b.u32(7);
                let three = b.u32(3);
                let nb = b.not(leaf_bit);
                let low = b.and(cur, nb);
                let first = b.shr(low, three);
                let m = b.and(cur, seven);
                let n = b.uadd(m, one);
                let end = b.uadd(first, n);
                let i = b.copy(first);
                b.loop_(super::LEAF_MAX, |b| {
                    let done = b.cmp(CmpOp::Ge, i, end);
                    b.break_if(done);
                    let twelve = b.u32(TRI_WORDS as u32);
                    let tb = b.umul(i, twelve);
                    let a = b.load_n(s.tris, tb, 0, &[Ty::F32; 4]);
                    let c = b.load_n(s.tris, tb, 4, &[Ty::F32; 4]);
                    let e = b.load_n(s.tris, tb, 8, &[Ty::F32, Ty::U32, Ty::U32, Ty::U32]);
                    let v0 = [a[0], a[1], a[2]];
                    let e1 = [a[3], c[0], c[1]];
                    let e2 = [c[2], c[3], e[0]];
                    let p = cross(b, d, e2);
                    let det = dot(b, e1, p);
                    let tv = sub(b, o, v0);
                    let q = cross(b, tv, e1);
                    let rdet = b.frcp(det);
                    let u = {
                        let x = dot(b, tv, p);
                        b.fmul(x, rdet)
                    };
                    let v = {
                        let x = dot(b, d, q);
                        b.fmul(x, rdet)
                    };
                    let t = {
                        let x = dot(b, e2, q);
                        b.fmul(x, rdet)
                    };
                    let eps = b.f32(1e-12);
                    let adet = b.fabs(det);
                    let z = b.f32(0.0);
                    let uv = b.fadd(u, v);
                    let onef = b.f32(1.0);
                    let c0 = b.cmp(CmpOp::Gt, adet, eps);
                    let c1 = b.cmp(CmpOp::Ge, u, z);
                    let c2 = b.cmp(CmpOp::Ge, v, z);
                    let c3 = b.cmp(CmpOp::Le, uv, onef);
                    let c4 = b.cmp(CmpOp::Gt, t, t_min);
                    let c5 = b.cmp(CmpOp::Lt, t, best_t);
                    let h = b.band(c0, c1);
                    let h = b.band(h, c2);
                    let h = b.band(h, c3);
                    let h = b.band(h, c4);
                    let h = b.band(h, c5);
                    let nt = b.sel(h, t, best_t);
                    let nu = b.sel(h, u, best_u);
                    let nv = b.sel(h, v, best_v);
                    let nid = b.sel(h, e[1], best_id);
                    b.mov(best_t, nt);
                    b.mov(best_u, nu);
                    b.mov(best_v, nv);
                    b.mov(best_id, nid);
                    if any {
                        let nf = b.sel(h, one, found);
                        b.mov(found, nf);
                        let hit = b.cmp(CmpOp::Ne, found, zero);
                        b.break_if(hit);
                    }
                    let ni = b.uadd(i, one);
                    b.mov(i, ni);
                });
                let t = b.bool(true);
                b.mov(need_pop, t);
            },
            |b| {
                if counted {
                    let ni = b.uadd(n_inner, one);
                    b.mov(n_inner, ni);
                }
                // Inner node: four boxes.
                let five = b.u32(NODE_WORDS.trailing_zeros());
                let nbase = b.shl(cur, five);
                let refs = b.load_n(s.nodes, nbase, 0, &[Ty::U32; 4]);
                let mut near: Vec<Vec<Reg>> = Vec::new();
                let mut far: Vec<Vec<Reg>> = Vec::new();
                for a in 0..3 {
                    let i = b.uadd(nbase, near_at[a]);
                    near.push(b.load_n(s.nodes, i, 0, &[Ty::F32; 4]));
                    let j = b.uadd(nbase, far_at[a]);
                    far.push(b.load_n(s.nodes, j, 0, &[Ty::F32; 4]));
                }
                let mut key = [inf; 4];
                let mut rf = [zero; 4];
                for k in 0..4 {
                    let (mut tn, mut tf) = (t_min, best_t);
                    for a in 0..3 {
                        let lo = b.fma(near[a][k], inv[a], off[a]);
                        tn = b.fmax(tn, lo);
                        let hi = b.fma(far[a][k], inv[a], off[a]);
                        tf = b.fmin(tf, hi);
                    }
                    let tf = b.fmul(tf, guard);
                    // An empty child's box is (+inf, -inf): never hit.
                    let h = b.cmp(CmpOp::Le, tn, tf);
                    key[k] = b.sel(h, tn, inf);
                    rf[k] = refs[k];
                }
                // Sort by distance: five compare-exchanges.
                for (x, y) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
                    let swap = b.cmp(CmpOp::Gt, key[x], key[y]);
                    let (kx, ky) = (key[x], key[y]);
                    let (rx, ry) = (rf[x], rf[y]);
                    key[x] = b.sel(swap, ky, kx);
                    key[y] = b.sel(swap, kx, ky);
                    rf[x] = b.sel(swap, ry, rx);
                    rf[y] = b.sel(swap, rx, ry);
                }
                // The others on the stack, farthest first; an entry that
                // does not count is overwritten by the next one.
                for k in [3usize, 2, 1] {
                    b.local_store(sp, rf[k]);
                    let valid = b.cmp(CmpOp::Lt, key[k], inf);
                    let step = b.sel(valid, one, zero);
                    let n = b.uadd(sp, step);
                    b.mov(sp, n);
                }
                let any_child = b.cmp(CmpOp::Lt, key[0], inf);
                b.if_else(
                    any_child,
                    |b| {
                        b.mov(cur, rf[0]);
                        let f = b.bool(false);
                        b.mov(need_pop, f);
                    },
                    |b| {
                        let t = b.bool(true);
                        b.mov(need_pop, t);
                    },
                );
            },
        );
        b.if_(need_pop, |b| {
            let none = b.cmp(CmpOp::Eq, sp, zero);
            b.break_if(none);
            let n = b.usub(sp, one);
            b.mov(sp, n);
            let r = b.local_load(sp, Ty::U32);
            b.mov(cur, r);
        });
        if any {
            let hit = b.cmp(CmpOp::Ne, found, zero);
            b.break_if(hit);
        }
    });

    Walk {
        t: best_t,
        u: best_u,
        v: best_v,
        id: best_id,
        found,
        inner: n_inner,
        leaf: n_leaf,
    }
}

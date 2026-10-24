//! The deblocking filter (8.7) for progressive frames, 4:2:0, 8 bits: run
//! over a whole picture once every slice of it is decoded.

use crate::pic::{Kind, MbInfo, Picture, SliceFilter};
use crate::tables::{ALPHA, BETA, TC0};

#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Chroma QP of an index (Table 8-15), as used by the filter.
fn chroma_qp(qpi: i32) -> i32 {
    const T: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    let q = qpi.clamp(0, 51);
    if q < 30 {
        q
    } else {
        T[(q - 30) as usize]
    }
}

/// Boundary strength between 4x4 block `bp` of `p` and block `bq` of `q`.
fn strength(p: &MbInfo, bp: usize, q: &MbInfo, bq: usize, mb_edge: bool) -> u8 {
    if p.kind.is_intra() || q.kind.is_intra() {
        return if mb_edge { 4 } else { 3 };
    }
    if (p.cbf_luma >> bp) & 1 != 0 || (q.cbf_luma >> bq) & 1 != 0 {
        return 2;
    }
    let (b8p, b8q) = (
        ((bp >> 3) << 1) | ((bp & 3) >> 1),
        ((bq >> 3) << 1) | ((bq & 3) >> 1),
    );
    if p.ref_pic[b8p] != q.ref_pic[b8q] {
        return 1;
    }
    let (mp, mq) = (p.mv[bp], q.mv[bq]);
    if (mp[0] as i32 - mq[0] as i32).abs() >= 4 || (mp[1] as i32 - mq[1] as i32).abs() >= 4 {
        return 1;
    }
    0
}

struct Params {
    alpha: i32,
    beta: i32,
    index_a: usize,
}

fn params(qp_p: i32, qp_q: i32, alpha_off: i32, beta_off: i32) -> Params {
    let av = (qp_p + qp_q + 1) >> 1;
    let index_a = (av + alpha_off).clamp(0, 51) as usize;
    let index_b = (av + beta_off).clamp(0, 51) as usize;
    Params {
        alpha: ALPHA[index_a] as i32,
        beta: BETA[index_b] as i32,
        index_a,
    }
}

/// A plane's samples reached through a raw pointer, so that threads can
/// filter different macroblock rows of one picture at the same time (see
/// `deblock_picture`).
#[derive(Clone, Copy)]
struct Px(*mut u8);

// SAFETY: the threads that share a `Px` touch the samples in the order the
// wavefront in `deblock_picture` allows, never the same sample at once.
unsafe impl Send for Px {}
unsafe impl Sync for Px {}

impl Px {
    #[inline]
    fn get(self, i: usize) -> i32 {
        // SAFETY: the callers only name samples inside the plane (edges of
        // a macroblock that has neighbours on that side).
        unsafe { *self.0.add(i) as i32 }
    }

    #[inline]
    fn set(self, i: usize, v: u8) {
        // SAFETY: as in `get`.
        unsafe { *self.0.add(i) = v }
    }
}

/// Filters `len` lines across an edge: `o` is the first q0 sample, `step`
/// moves across the edge (1 for a vertical edge, the stride for a horizontal
/// one), `line` moves along it.
fn luma_edge(d: Px, o: usize, step: usize, line: usize, bs: u8, len: usize, pr: &Params) {
    if bs == 0 || pr.alpha == 0 || pr.beta == 0 {
        return;
    }
    let (alpha, beta) = (pr.alpha, pr.beta);
    let tc0 = if bs < 4 {
        TC0[pr.index_a][bs as usize - 1] as i32
    } else {
        0
    };
    for k in 0..len {
        let q = o + k * line;
        let (p0, q0) = (d.get(q - step), d.get(q));
        if (p0 - q0).abs() >= alpha {
            continue;
        }
        let (p1, q1) = (d.get(q - 2 * step), d.get(q + step));
        if (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
            continue;
        }
        let (p2, q2) = (d.get(q - 3 * step), d.get(q + 2 * step));
        let ap = (p2 - p0).abs() < beta;
        let aq = (q2 - q0).abs() < beta;
        if bs < 4 {
            let tc = tc0 + ap as i32 + aq as i32;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d.set(q - step, clip1(p0 + delta));
            d.set(q, clip1(q0 - delta));
            if ap {
                d.set(
                    q - 2 * step,
                    clip1(p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)),
                );
            }
            if aq {
                d.set(
                    q + step,
                    clip1(q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)),
                );
            }
        } else {
            let strong = (p0 - q0).abs() < ((alpha >> 2) + 2);
            if ap && strong {
                let p3 = d.get(q - 4 * step);
                d.set(
                    q - step,
                    ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8,
                );
                d.set(q - 2 * step, ((p2 + p1 + p0 + q0 + 2) >> 2) as u8);
                d.set(
                    q - 3 * step,
                    ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8,
                );
            } else {
                d.set(q - step, ((2 * p1 + p0 + q1 + 2) >> 2) as u8);
            }
            if aq && strong {
                let q3 = d.get(q + 3 * step);
                d.set(q, ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8);
                d.set(q + step, ((p0 + q0 + q1 + q2 + 2) >> 2) as u8);
                d.set(
                    q + 2 * step,
                    ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8,
                );
            } else {
                d.set(q, ((2 * q1 + q0 + p1 + 2) >> 2) as u8);
            }
        }
    }
}

/// Chroma edge: the same for `len` lines, one sample modified on each side.
fn chroma_edge(d: Px, o: usize, step: usize, line: usize, bs: u8, len: usize, pr: &Params) {
    if bs == 0 || pr.alpha == 0 || pr.beta == 0 {
        return;
    }
    let tc = if bs < 4 {
        TC0[pr.index_a][bs as usize - 1] as i32 + 1
    } else {
        0
    };
    for k in 0..len {
        let q = o + k * line;
        let (p1, p0, q0, q1) = (
            d.get(q - 2 * step),
            d.get(q - step),
            d.get(q),
            d.get(q + step),
        );
        if (p0 - q0).abs() >= pr.alpha || (p1 - p0).abs() >= pr.beta || (q1 - q0).abs() >= pr.beta {
            continue;
        }
        if bs < 4 {
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d.set(q - step, clip1(p0 + delta));
            d.set(q, clip1(q0 - delta));
        } else {
            d.set(q - step, ((2 * p1 + p0 + q1 + 2) >> 2) as u8);
            d.set(q, ((2 * q1 + q0 + p1 + 2) >> 2) as u8);
        }
    }
}

/// What filtering a macroblock needs of the picture.
struct Ctx<'a> {
    y: Px,
    u: Px,
    v: Px,
    ys: usize,
    cs: usize,
    mb_w: usize,
    mbs: &'a [MbInfo],
    slices: &'a [SliceFilter],
    chroma_qp_offset: [i32; 2],
}

/// Filters macroblock `(mx, my)`.
fn deblock_mb(c: &Ctx, mx: usize, my: usize) {
    let w = c.mb_w;
    let idx = my * w + mx;
    let cur = c.mbs[idx];
    if cur.slice == 0 {
        return;
    }
    let sf = c.slices[cur.slice as usize - 1];
    if sf.disable == 1 {
        return;
    }
    let qp_of = |m: &MbInfo| if m.kind == Kind::Pcm { 0 } else { m.qp as i32 };
    let left =
        (mx > 0 && !(sf.disable == 2 && c.mbs[idx - 1].slice != cur.slice)).then(|| c.mbs[idx - 1]);
    let top =
        (my > 0 && !(sf.disable == 2 && c.mbs[idx - w].slice != cur.slice)).then(|| c.mbs[idx - w]);
    let t8 = cur.transform_8x8;

    // bS[dir][edge][group]: dir 0 vertical edges, 1 horizontal.
    let mut bs = [[[0u8; 4]; 4]; 2];
    for g in 0..4usize {
        // Vertical edges touch blocks (e, g), horizontal ones (g, e), in 4x4 units.
        if let Some(l) = &left {
            bs[0][0][g] = strength(l, g * 4 + 3, &cur, g * 4, true);
        }
        if let Some(t) = &top {
            bs[1][0][g] = strength(t, 12 + g, &cur, g, true);
        }
        for e in 1..4usize {
            if t8 && e & 1 == 1 {
                continue;
            }
            bs[0][e][g] = strength(&cur, g * 4 + e - 1, &cur, g * 4 + e, false);
            bs[1][e][g] = strength(&cur, (e - 1) * 4 + g, &cur, e * 4 + g, false);
        }
    }

    let ys = c.ys;
    let (x0, y0) = (mx * 16, my * 16);
    // Luma: vertical edges, then horizontal edges.
    for e in 0..4 {
        let pm = if e == 0 { left } else { Some(cur) };
        if let Some(pm) = pm {
            let pr = params(qp_of(&pm), qp_of(&cur), sf.alpha_offset, sf.beta_offset);
            for g in 0..4 {
                luma_edge(
                    c.y,
                    (y0 + g * 4) * ys + x0 + e * 4,
                    1,
                    ys,
                    bs[0][e][g],
                    4,
                    &pr,
                );
            }
        }
    }
    for e in 0..4 {
        let pm = if e == 0 { top } else { Some(cur) };
        if let Some(pm) = pm {
            let pr = params(qp_of(&pm), qp_of(&cur), sf.alpha_offset, sf.beta_offset);
            for g in 0..4 {
                luma_edge(
                    c.y,
                    (y0 + e * 4) * ys + x0 + g * 4,
                    ys,
                    1,
                    bs[1][e][g],
                    4,
                    &pr,
                );
            }
        }
    }
    // Chroma: edges 0 and 2 of luma, each bS covering two chroma lines.
    let cs = c.cs;
    let (cx0, cy0) = (mx * 8, my * 8);
    for comp in 0..2 {
        let off = c.chroma_qp_offset[comp];
        let cq = |m: &MbInfo| chroma_qp(qp_of(m) + off);
        // All the vertical edges first, then the horizontal ones.
        for ce in 0..2 {
            let pm = if ce == 0 { left } else { Some(cur) };
            if let Some(pm) = pm {
                let pr = params(cq(&pm), cq(&cur), sf.alpha_offset, sf.beta_offset);
                let plane = if comp == 0 { c.u } else { c.v };
                for g in 0..4 {
                    chroma_edge(
                        plane,
                        (cy0 + g * 2) * cs + cx0 + ce * 4,
                        1,
                        cs,
                        bs[0][ce * 2][g],
                        2,
                        &pr,
                    );
                }
            }
        }
        for ce in 0..2 {
            let pm = if ce == 0 { top } else { Some(cur) };
            if let Some(pm) = pm {
                let pr = params(cq(&pm), cq(&cur), sf.alpha_offset, sf.beta_offset);
                let plane = if comp == 0 { c.u } else { c.v };
                for g in 0..4 {
                    chroma_edge(
                        plane,
                        (cy0 + ce * 4) * cs + cx0 + g * 2,
                        cs,
                        1,
                        bs[1][ce * 2][g],
                        2,
                        &pr,
                    );
                }
            }
        }
    }
}

/// Filters every macroblock of the picture. With several threads, rows of
/// macroblocks are filtered at once as a wavefront: macroblock `(x, y)` waits
/// until row `y - 1` has filtered the macroblocks up to `x + 1` (its top
/// edge reads and changes samples that macroblock's left edge changes
/// too); a row's own macroblocks go in order.
pub fn deblock_picture(pic: &mut Picture, threads: usize) {
    let (w, h) = (pic.mb_width, pic.mb_height);
    let ctx = Ctx {
        y: Px(pic.y.data.as_mut_ptr()),
        u: Px(pic.u.data.as_mut_ptr()),
        v: Px(pic.v.data.as_mut_ptr()),
        ys: pic.y.stride,
        cs: pic.u.stride,
        mb_w: w,
        mbs: &pic.mbs,
        slices: &pic.slices,
        chroma_qp_offset: pic.chroma_qp_offset,
    };
    let workers = threads.clamp(1, h);
    if workers == 1 {
        for my in 0..h {
            for mx in 0..w {
                deblock_mb(&ctx, mx, my);
            }
        }
        return;
    }
    let progress: Vec<std::sync::atomic::AtomicUsize> = (0..h)
        .map(|_| std::sync::atomic::AtomicUsize::new(0))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
    let work = || loop {
        let my = next.fetch_add(1, Relaxed);
        if my >= h {
            break;
        }
        for mx in 0..w {
            if my > 0 {
                let need = (mx + 2).min(w);
                let mut spins = 0u32;
                while progress[my - 1].load(Acquire) < need {
                    spins += 1;
                    if spins < 200 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
            deblock_mb(&ctx, mx, my);
            progress[my].store(mx + 1, Release);
        }
    };
    std::thread::scope(|sc| {
        for _ in 1..workers {
            sc.spawn(work);
        }
        work();
    });
}

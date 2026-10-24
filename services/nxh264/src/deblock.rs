//! The deblocking filter (8.7) for progressive frames, 4:2:0, 8 bits: run
//! over a whole picture once every slice of it is decoded.

use crate::pic::{Kind, MbInfo, Picture};
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

/// Filters `len` lines across an edge: `o` is the first q0 sample, `step`
/// moves across the edge (1 for a vertical edge, the stride for a horizontal
/// one), `line` moves along it.
fn luma_edge(d: &mut [u8], o: usize, step: usize, line: usize, bs: u8, len: usize, pr: &Params) {
    if bs == 0 || pr.alpha == 0 || pr.beta == 0 {
        return;
    }
    let tc0 = if bs < 4 {
        TC0[pr.index_a][bs as usize - 1] as i32
    } else {
        0
    };
    for k in 0..len {
        let q = o + k * line;
        let s = |i: isize| d[(q as isize + i * step as isize) as usize] as i32;
        let (p3, p2, p1, p0, q0, q1, q2, q3) = (s(-4), s(-3), s(-2), s(-1), s(0), s(1), s(2), s(3));
        if !((p0 - q0).abs() < pr.alpha && (p1 - p0).abs() < pr.beta && (q1 - q0).abs() < pr.beta) {
            continue;
        }
        let ap = (p2 - p0).abs();
        let aq = (q2 - q0).abs();
        if bs < 4 {
            let tc = tc0 + (ap < pr.beta) as i32 + (aq < pr.beta) as i32;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d[q - step] = clip1(p0 + delta);
            d[q] = clip1(q0 - delta);
            if ap < pr.beta {
                d[q - 2 * step] =
                    clip1(p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0));
            }
            if aq < pr.beta {
                d[q + step] =
                    clip1(q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0));
            }
        } else {
            let strong = (p0 - q0).abs() < ((pr.alpha >> 2) + 2);
            if ap < pr.beta && strong {
                d[q - step] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8;
                d[q - 2 * step] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u8;
                d[q - 3 * step] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8;
            } else {
                d[q - step] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
            }
            if aq < pr.beta && strong {
                d[q] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8;
                d[q + step] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u8;
                d[q + 2 * step] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8;
            } else {
                d[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
            }
        }
    }
}

/// Chroma edge: the same for `len` lines, one sample modified on each side.
fn chroma_edge(d: &mut [u8], o: usize, step: usize, line: usize, bs: u8, len: usize, pr: &Params) {
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
        let s = |i: isize| d[(q as isize + i * step as isize) as usize] as i32;
        let (p1, p0, q0, q1) = (s(-2), s(-1), s(0), s(1));
        if !((p0 - q0).abs() < pr.alpha && (p1 - p0).abs() < pr.beta && (q1 - q0).abs() < pr.beta) {
            continue;
        }
        if bs < 4 {
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            d[q - step] = clip1(p0 + delta);
            d[q] = clip1(q0 - delta);
        } else {
            d[q - step] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
            d[q] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
        }
    }
}

/// Filters macroblock `(mx, my)`.
pub fn deblock_mb(pic: &mut Picture, mx: usize, my: usize) {
    let w = pic.mb_width;
    let idx = my * w + mx;
    let cur = pic.mbs[idx];
    if cur.slice == 0 {
        return;
    }
    let sf = pic.slices[cur.slice as usize - 1];
    if sf.disable == 1 {
        return;
    }
    let qp_of = |m: &MbInfo| if m.kind == Kind::Pcm { 0 } else { m.qp as i32 };
    let left = (mx > 0 && !(sf.disable == 2 && pic.mbs[idx - 1].slice != cur.slice))
        .then(|| pic.mbs[idx - 1]);
    let top = (my > 0 && !(sf.disable == 2 && pic.mbs[idx - w].slice != cur.slice))
        .then(|| pic.mbs[idx - w]);
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

    let ys = pic.y.stride;
    let (x0, y0) = (mx * 16, my * 16);
    // Luma: vertical edges, then horizontal edges.
    for e in 0..4 {
        let pm = if e == 0 { left } else { Some(cur) };
        if let Some(pm) = pm {
            let pr = params(qp_of(&pm), qp_of(&cur), sf.alpha_offset, sf.beta_offset);
            for g in 0..4 {
                luma_edge(
                    &mut pic.y.data,
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
                    &mut pic.y.data,
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
    let cs = pic.u.stride;
    let (cx0, cy0) = (mx * 8, my * 8);
    for comp in 0..2 {
        let off = pic.chroma_qp_offset[comp];
        let cq = |m: &MbInfo| chroma_qp(qp_of(m) + off);
        // All the vertical edges first, then the horizontal ones.
        for ce in 0..2 {
            let pm = if ce == 0 { left } else { Some(cur) };
            if let Some(pm) = pm {
                let pr = params(cq(&pm), cq(&cur), sf.alpha_offset, sf.beta_offset);
                let plane = if comp == 0 { &mut pic.u } else { &mut pic.v };
                for g in 0..4 {
                    chroma_edge(
                        &mut plane.data,
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
                let plane = if comp == 0 { &mut pic.u } else { &mut pic.v };
                for g in 0..4 {
                    chroma_edge(
                        &mut plane.data,
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

/// Filters every macroblock of the picture in raster order.
pub fn deblock_picture(pic: &mut Picture) {
    for my in 0..pic.mb_height {
        for mx in 0..pic.mb_width {
            deblock_mb(pic, mx, my);
        }
    }
}

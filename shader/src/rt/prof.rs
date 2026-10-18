//! A profile of the traversal kernels on the SASS simulator: dynamic
//! instructions per thread, against the nodes and triangles the CPU walk
//! visits, and the spread inside a warp (`cargo test --release profile --
//! --ignored --nocapture`).

use std::vec;
use std::vec::Vec;

use super::demo::{rays, soup};
use super::*;
use crate::interp::NoTextures;
use crate::lower::{compile, CB_BUFFERS, CB_TEXTURES, CB_UNIFORMS};
use crate::sass::{SR_CTAID_X, SR_TID_X};
use crate::sassim::Sim;

fn counts(bvh: &bvh::Bvh, ray: &Ray) -> (u32, u32, u32) {
    let f = f32::from_bits;
    let inv = [1.0 / ray.dir[0], 1.0 / ray.dir[1], 1.0 / ray.dir[2]];
    let mut t_max = ray.t_max;
    let mut stack: Vec<u32> = vec![0];
    let (mut nodes, mut tris, mut leaves) = (0u32, 0u32, 0u32);
    while let Some(r) = stack.pop() {
        if r & LEAF != 0 {
            let (first, count) = ((r & !LEAF) >> 3, (r & 7) + 1);
            leaves += 1;
            for i in first..first + count {
                tris += 1;
                let w = &bvh.tris[i as usize * TRI_WORDS..][..TRI_WORDS];
                let v0 = [f(w[0]), f(w[1]), f(w[2])];
                let e1 = [f(w[3]), f(w[4]), f(w[5])];
                let e2 = [f(w[6]), f(w[7]), f(w[8])];
                if let Some((t, _, _)) =
                    trace::intersect(v0, e1, e2, ray.origin, ray.dir, ray.t_min, t_max)
                {
                    t_max = t;
                }
            }
            continue;
        }
        nodes += 1;
        let n = &bvh.nodes[r as usize * NODE_WORDS..][..NODE_WORDS];
        let mut near = Vec::new();
        for c in 0..4 {
            if n[c] == EMPTY {
                continue;
            }
            let (mut lo_t, mut hi_t) = (ray.t_min, t_max);
            for a in 0..3 {
                let lo = (f(n[4 + 4 * a + c]) - ray.origin[a]) * inv[a];
                let hi = (f(n[16 + 4 * a + c]) - ray.origin[a]) * inv[a];
                lo_t = lo_t.max(lo.min(hi));
                hi_t = hi_t.min(lo.max(hi));
            }
            if lo_t <= hi_t {
                near.push((lo_t, n[c]));
            }
        }
        near.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(core::cmp::Ordering::Equal));
        stack.extend(near.into_iter().map(|(_, r)| r));
    }
    (nodes, tris, leaves)
}

/// The kernel's algorithm on the CPU: (inner nodes, leaves, triangles).
fn kernel_walk(bvh: &bvh::Bvh, ray: &Ray, any: bool) -> (u32, u32, u32) {
    let f = f32::from_bits;
    let tiny = 1e-20f32;
    let mut inv = [0f32; 3];
    let mut off = [0f32; 3];
    for a in 0..3 {
        let safe = if ray.dir[a].abs() < tiny {
            tiny
        } else {
            ray.dir[a]
        };
        inv[a] = 1.0 / safe;
        off[a] = -(ray.origin[a] * inv[a]);
    }
    let mut best_t = ray.t_max;
    let (mut inner, mut leaves, mut tris) = (0, 0, 0);
    let mut stack: Vec<u32> = Vec::new();
    let mut cur = 0u32;
    'walk: loop {
        let mut pop = false;
        if cur & LEAF != 0 {
            leaves += 1;
            let (first, count) = ((cur & !LEAF) >> 3, (cur & 7) + 1);
            for i in first..first + count {
                tris += 1;
                let w = &bvh.tris[i as usize * TRI_WORDS..][..TRI_WORDS];
                let v0 = [f(w[0]), f(w[1]), f(w[2])];
                let e1 = [f(w[3]), f(w[4]), f(w[5])];
                let e2 = [f(w[6]), f(w[7]), f(w[8])];
                if let Some((t, _, _)) =
                    trace::intersect(v0, e1, e2, ray.origin, ray.dir, ray.t_min, best_t)
                {
                    best_t = t;
                    if any {
                        break 'walk;
                    }
                }
            }
            pop = true;
        } else {
            inner += 1;
            let n = &bvh.nodes[cur as usize * NODE_WORDS..][..NODE_WORDS];
            let mut kids: Vec<(f32, u32)> = Vec::new();
            for k in 0..4 {
                let (mut tn, mut tf) = (ray.t_min, best_t);
                for a in 0..3 {
                    let (mn, mx) = (f(n[4 + 4 * a + k]), f(n[16 + 4 * a + k]));
                    let (np, fp) = if inv[a] < 0.0 { (mx, mn) } else { (mn, mx) };
                    tn = tn.max(np.mul_add(inv[a], off[a]));
                    tf = tf.min(fp.mul_add(inv[a], off[a]));
                }
                let tf = tf * 1.000_000_5;
                if tn <= tf && n[k] != EMPTY {
                    kids.push((tn, n[k]));
                }
            }
            kids.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            if kids.is_empty() {
                pop = true;
            } else {
                cur = kids[0].1;
                for k in kids[1..].iter().rev() {
                    stack.push(k.1);
                }
            }
        }
        if pop {
            match stack.pop() {
                Some(r) => cur = r,
                None => break,
            }
        }
    }
    (inner, leaves, tris)
}

fn profile(name: &str, p: &crate::Program, n: usize) {
    let tris = soup(4000, 3.0, 1);
    let bvh = build(&tris).unwrap();
    let rs = rays(n, 2);
    let c = compile(p).unwrap();
    let rays_w: Vec<u32> = rs.iter().flat_map(|r| r.words()).collect();
    let (mut sum, mut warp_max, mut nodes, mut tri_tests, mut leaf_visits) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut per = Vec::new();
    let (mut cycles, mut hist) = (0u64, vec![0u64; c.code.len() / 4 * 3]);
    for t in 0..n {
        let bufs = [
            rays_w.clone(),
            vec![0; n * HIT_WORDS],
            bvh.nodes.clone(),
            bvh.tris.clone(),
        ];
        let mut tex = NoTextures;
        let mut s = Sim::new(&c.code, &mut tex);
        s.cbuf[CB_UNIFORMS as usize] = vec![n as u32];
        let mut desc = Vec::new();
        for (i, b) in bufs.iter().enumerate() {
            let base = 0x2_0000_0000u64 + 0x100_0000 * i as u64;
            desc.extend([base as u32, (base >> 32) as u32, b.len() as u32, 0]);
        }
        s.cbuf[CB_BUFFERS as usize] = desc;
        s.cbuf[CB_TEXTURES as usize] = (0..8u32).collect();
        for (i, b) in bufs.iter().enumerate() {
            s.mem
                .push((0x2_0000_0000u64 + 0x100_0000 * i as u64, b.clone()));
        }
        s.sregs.insert(SR_TID_X, (t % 64) as u32);
        s.sregs.insert(SR_CTAID_X, (t / 64) as u32);
        for k in 1..3 {
            s.sregs.insert(SR_TID_X + k, 0);
            s.sregs.insert(SR_CTAID_X + k, 0);
        }
        s.regs[255] = 0;
        s.local = vec![0xDEAD_BEEF; p.local_words as usize];
        s.run().unwrap();
        per.push(s.steps);
        cycles += s.cycles;
        for (h, x) in hist.iter_mut().zip(&s.hist) {
            *h += *x as u64;
        }
        sum += s.steps;
        let (nd, tr, lv) = counts(&bvh, &rs[t]);
        leaf_visits += lv as u64;
        nodes += nd as u64;
        tri_tests += tr as u64;
    }
    for w in per.chunks(32) {
        warp_max += *w.iter().max().unwrap();
    }
    std::println!(
        "{name}: {:.0} modelled cycles per ray",
        cycles as f64 / n as f64
    );
    let each: Vec<String> = hist
        .iter()
        .enumerate()
        .map(|(i, h)| std::format!("{}:{:.1}", i, *h as f64 / n as f64))
        .collect();
    std::println!("{name}: per instruction: {}", each.join(" "));
    // Where the instructions are: runs of 32 instructions.
    let line: Vec<String> = hist
        .chunks(32)
        .map(|c| std::format!("{:.0}", c.iter().sum::<u64>() as f64 / n as f64))
        .collect();
    std::println!(
        "{name}: instructions per ray by 32-instruction run: {}",
        line.join(" ")
    );
    std::println!(
        "{name}: CPU walk: {:.1} leaves",
        leaf_visits as f64 / n as f64
    );
    let k: Vec<(u32, u32, u32)> = rs
        .iter()
        .map(|r| kernel_walk(&bvh, r, name == "any"))
        .collect();
    std::println!(
        "{name}: the kernel's algorithm on the CPU: {:.1} inner, {:.1} leaves, {:.1} triangles",
        k.iter().map(|x| x.0 as f64).sum::<f64>() / n as f64,
        k.iter().map(|x| x.1 as f64).sum::<f64>() / n as f64,
        k.iter().map(|x| x.2 as f64).sum::<f64>() / n as f64
    );
    std::println!(
        "{name}: {n} rays, {:.0} thread instructions per ray, {:.0} warp-lockstep, {:.1} nodes, {:.1} triangles; {} registers, {} local bytes, {} code words",
        sum as f64 / n as f64,
        warp_max as f64 * 32.0 / n as f64,
        nodes as f64 / n as f64,
        tri_tests as f64 / n as f64,
        c.gprs,
        c.local_bytes,
        c.code.len()
    );
}

/// Inner nodes and leaves per ray of the counting kernel, on the
/// reference machine.
#[test]
#[ignore]
fn interpreter_visits() {
    use crate::interp::{run, Machine};
    use crate::{Builtin, MAX_ATTRS};
    let tris = soup(4000, 3.0, 1);
    let bvh = build(&tris).unwrap();
    let n = 512;
    let rs = rays(n, 2);
    let p = kernel::closest_hit_counted(kernel::Slots::default());
    let mut bufs = vec![Vec::new(); crate::MAX_BUFFERS];
    bufs[0] = rs.iter().flat_map(|r| r.words()).collect();
    bufs[1] = vec![0; n * HIT_WORDS];
    bufs[2] = bvh.nodes.clone();
    bufs[3] = bvh.tris.clone();
    let uniforms = [n as u32];
    for t in 0..n {
        let mut m = Machine {
            inputs: [[0; 4]; MAX_ATTRS],
            builtins: [0; 11],
            uniforms: &uniforms,
            buffers: &mut bufs,
            sampler: &mut NoTextures,
        };
        m.builtins[Builtin::LocalIdX as usize] = t as u32 % 64;
        m.builtins[Builtin::GroupIdX as usize] = t as u32 / 64;
        run(&p, &mut m);
    }
    let (mut inner, mut leaves, mut cpu_inner, mut cpu_leaves) = (0f64, 0f64, 0f64, 0f64);
    for (t, r) in rs.iter().enumerate() {
        inner += bufs[1][t * 4 + 1] as f64;
        leaves += bufs[1][t * 4 + 2] as f64;
        let k = kernel_walk(&bvh, r, false);
        cpu_inner += k.0 as f64;
        cpu_leaves += k.1 as f64;
    }
    std::println!(
        "reference machine: {:.1} inner, {:.1} leaves per ray; the algorithm on the CPU: {:.1} and {:.1}",
        inner / n as f64,
        leaves / n as f64,
        cpu_inner / n as f64,
        cpu_leaves / n as f64
    );
}

#[test]
#[ignore]
fn profile_kernels() {
    profile(
        "closest",
        &kernel::closest_hit(kernel::Slots::default()),
        2048,
    );
    profile("any", &kernel::any_hit(kernel::Slots::default()), 2048);
}

/// The compiled kernels, run thread by thread on the SASS simulator,
/// against the reference machine (the interpreter): a few rays grazing an
/// edge may fall either way, the approximate reciprocal being what it is.
#[test]
fn compiled_kernels_match_the_reference() {
    let tris = soup(4000, 3.0, 1);
    let bvh = build(&tris).unwrap();
    let n = 256;
    let rs = rays(n, 2);
    let rays_w: Vec<u32> = rs.iter().flat_map(|r| r.words()).collect();
    for (name, p, words) in [
        (
            "closest",
            kernel::closest_hit(kernel::Slots::default()),
            HIT_WORDS,
        ),
        ("any", kernel::any_hit(kernel::Slots::default()), 1),
    ] {
        let c = compile(&p).unwrap();
        let mut rbufs = vec![Vec::new(); crate::MAX_BUFFERS];
        rbufs[0] = rays_w.clone();
        rbufs[1] = vec![0; n * words];
        rbufs[2] = bvh.nodes.clone();
        rbufs[3] = bvh.tris.clone();
        {
            use crate::interp::{run, Machine};
            use crate::{Builtin, MAX_ATTRS};
            let uniforms = [n as u32];
            for t in 0..n {
                let mut m = Machine {
                    inputs: [[0; 4]; MAX_ATTRS],
                    builtins: [0; 11],
                    uniforms: &uniforms,
                    buffers: &mut rbufs,
                    sampler: &mut NoTextures,
                };
                m.builtins[Builtin::LocalIdX as usize] = t as u32 % 64;
                m.builtins[Builtin::GroupIdX as usize] = t as u32 / 64;
                run(&p, &mut m);
            }
        }
        let mut wrong = 0;
        for t in 0..n {
            let bufs = [
                rays_w.clone(),
                vec![0; n * words],
                bvh.nodes.clone(),
                bvh.tris.clone(),
            ];
            let mut tex = NoTextures;
            let mut s = Sim::new(&c.code, &mut tex);
            s.cbuf[CB_UNIFORMS as usize] = vec![n as u32];
            let mut desc = Vec::new();
            for (i, b) in bufs.iter().enumerate() {
                let base = 0x2_0000_0000u64 + 0x100_0000 * i as u64;
                desc.extend([base as u32, (base >> 32) as u32, b.len() as u32, 0]);
            }
            s.cbuf[CB_BUFFERS as usize] = desc;
            s.cbuf[CB_TEXTURES as usize] = (0..8u32).collect();
            for (i, b) in bufs.iter().enumerate() {
                s.mem
                    .push((0x2_0000_0000u64 + 0x100_0000 * i as u64, b.clone()));
            }
            s.sregs.insert(SR_TID_X, (t % 64) as u32);
            s.sregs.insert(SR_CTAID_X, (t / 64) as u32);
            for k in 1..3 {
                s.sregs.insert(SR_TID_X + k, 0);
                s.sregs.insert(SR_CTAID_X + k, 0);
            }
            s.regs[255] = 0;
            s.local = vec![0xDEAD_BEEF; p.local_words as usize];
            s.run().unwrap();
            let got = &s.mem[1].1[t * words..(t + 1) * words];
            let want = &rbufs[1][t * words..(t + 1) * words];
            let same = if words == 1 {
                got == want
            } else {
                let (a, b) = (f32::from_bits(got[0]), f32::from_bits(want[0]));
                (got[3] == want[3] || (a - b).abs() <= 1e-3 * b.abs().max(1.0))
                    && (a - b).abs() <= 1e-3 * b.abs().max(1.0)
            };
            wrong += !same as u32;
        }
        assert!(wrong <= 2, "{name}: {wrong} of {n} rays differ");
    }
}

/// Opcode histogram of a region of the closest-hit kernel.
#[test]
#[ignore]
fn opcodes() {
    let c = compile(&kernel::closest_hit(kernel::Slots::default())).unwrap();
    let n = c.code.len() / 4 * 3;
    let word = |i: usize| c.code[i / 3 * 4 + 1 + i % 3];
    for (name, lo, hi) in [("prologue", 0, 99), ("leaf", 107, 254), ("inner", 258, 502)] {
        let mut h = std::collections::BTreeMap::new();
        for i in lo..hi.min(n) {
            *h.entry((word(i) >> 52) as u16).or_insert(0u32) += 1;
        }
        let mut v: Vec<_> = h.into_iter().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        std::println!("{name}: {:x?}", v);
    }
}

/// A rough listing of the inner-node path.
#[test]
#[ignore]
fn listing() {
    let c = compile(&kernel::closest_hit(kernel::Slots::default())).unwrap();
    let word = |i: usize| c.code[i / 3 * 4 + 1 + i % 3];
    let (lo, hi): (usize, usize) = (
        std::env::var("LO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(258),
        std::env::var("HI")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(330),
    );
    for i in lo..hi {
        let w = word(i);
        std::println!(
            "{i:4} {:04x} d=R{:<3} a=R{:<3} b=R{:<3} g={} {:016x}",
            w >> 48,
            w & 0xFF,
            w >> 8 & 0xFF,
            w >> 20 & 0xFF,
            w >> 16 & 0xF,
            w
        );
    }
}

#[test]
#[ignore]
fn shadow_kernel_size() {
    let p = shadow::kernel(shadow::Slots::default());
    let w = crate::wire::encode(&p);
    let c = compile(&p).unwrap();
    std::println!("shadow kernel: {} wire words, {} registers, {} code words", w.len(), c.gprs, c.code.len());
}

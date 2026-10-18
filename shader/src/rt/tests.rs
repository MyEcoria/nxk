use std::vec;
use std::vec::Vec;

use super::*;

use super::demo::{rays, soup};

fn same(a: Option<Hit>, b: Option<Hit>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => (x.t - y.t).abs() <= 1e-4 * x.t.abs().max(1.0),
        _ => false,
    }
}

#[test]
fn tree_matches_brute_force() {
    for (n, size, seed) in [
        (1, 3.0, 1),
        (2, 3.0, 2),
        (9, 4.0, 3),
        (300, 2.0, 4),
        (3000, 1.0, 5),
        (3000, 6.0, 6),
    ] {
        let tris = soup(n, size, seed);
        let bvh = build(&tris).unwrap();
        assert_eq!(bvh.tri_count as usize, n);
        assert!(bvh.depth <= MAX_DEPTH);
        let mut hits = 0;
        for ray in rays(400, seed + 100) {
            let a = trace::trace(&bvh, &ray);
            let b = trace::brute_force(&tris, &ray);
            assert!(
                same(a, b),
                "n {n}: tree {a:?} brute force {b:?} for {ray:?}"
            );
            hits += a.is_some() as u32;
            assert_eq!(trace::occluded(&bvh, &ray), b.is_some());
        }
        std::println!(
            "n {n} size {size}: depth {}, nodes {}, {hits}/400 rays hit",
            bvh.depth,
            bvh.node_count
        );
    }
}

#[test]
fn every_triangle_is_in_one_leaf_and_boxes_contain_their_children() {
    let tris = soup(2000, 2.0, 9);
    let bvh = build(&tris).unwrap();
    let mut seen = vec![0u32; tris.len()];
    let mut stack = vec![(0u32, bvh.bounds)];
    while let Some((n, bx)) = stack.pop() {
        let w = &bvh.nodes[n as usize * NODE_WORDS..][..NODE_WORDS];
        for c in 0..4 {
            if w[c] == EMPTY {
                continue;
            }
            let cmin: Vec<f32> = (0..3).map(|a| f32::from_bits(w[4 + 4 * a + c])).collect();
            let cmax: Vec<f32> = (0..3).map(|a| f32::from_bits(w[16 + 4 * a + c])).collect();
            for a in 0..3 {
                assert!(cmin[a] >= bx.0[a] - 1e-4 && cmax[a] <= bx.1[a] + 1e-4);
            }
            if w[c] & LEAF != 0 {
                let (first, count) = ((w[c] & !LEAF) >> 3, (w[c] & 7) + 1);
                assert!(count <= LEAF_MAX);
                for i in first..first + count {
                    seen[i as usize] += 1;
                    // The triangle lies in its leaf's box.
                    let t = &bvh.tris[i as usize * TRI_WORDS..][..9];
                    let v0: Vec<f32> = t[..3].iter().map(|&x| f32::from_bits(x)).collect();
                    for a in 0..3 {
                        assert!(v0[a] >= cmin[a] - 1e-4 && v0[a] <= cmax[a] + 1e-4);
                    }
                }
            } else {
                stack.push((
                    w[c],
                    ([cmin[0], cmin[1], cmin[2]], [cmax[0], cmax[1], cmax[2]]),
                ));
            }
        }
    }
    assert!(seen.iter().all(|&s| s == 1));
}

#[test]
fn degenerate_inputs() {
    assert_eq!(build(&[]).err(), Some(BuildError::Empty));
    // The same triangle twenty times; collinear centroids.
    let t = Triangle {
        v: [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        id: 7,
    };
    let tris = vec![t; 20];
    let bvh = build(&tris).unwrap();
    let ray = Ray {
        origin: [0.2, 0.2, 1.0],
        t_min: 0.0,
        dir: [0.0, 0.0, -1.0],
        t_max: 10.0,
    };
    assert_eq!(trace::trace(&bvh, &ray).map(|h| h.id), Some(7));
    let nan = Triangle {
        v: [[f32::NAN, 0.0, 0.0]; 3],
        id: 0,
    };
    assert_eq!(build(&[nan]).err(), Some(BuildError::NotFinite));
    // A long thin chain of triangles still builds within the depth limit.
    let chain: Vec<Triangle> = (0..5000)
        .map(|i| {
            let x = i as f32 * 0.37;
            Triangle {
                v: [[x, 0.0, 0.0], [x + 0.1, 0.0, 0.0], [x, 0.1, 0.0]],
                id: i,
            }
        })
        .collect();
    let bvh = build(&chain).unwrap();
    assert!(bvh.depth <= MAX_DEPTH, "depth {}", bvh.depth);
}

use crate::interp::{run, Machine, NoTextures};
use crate::{validate, Builtin, MAX_ATTRS, MAX_BUFFERS};

/// The kernel's answers for `rays` on `bvh`, thread by thread on the
/// reference machine.
fn run_kernel(p: &crate::Program, bvh: &Bvh, rays: &[Ray], hit_words: usize) -> Vec<u32> {
    let mut bufs = vec![Vec::new(); MAX_BUFFERS];
    bufs[0] = rays.iter().flat_map(|r| r.words()).collect();
    bufs[1] = vec![0xAAAA_AAAA; rays.len() * hit_words];
    bufs[2] = bvh.nodes.clone();
    bufs[3] = bvh.tris.clone();
    let uniforms = [rays.len() as u32];
    for t in 0..rays.len().div_ceil(kernel::GROUP as usize) * kernel::GROUP as usize {
        let mut m = Machine {
            inputs: [[0; 4]; MAX_ATTRS],
            builtins: [0; 11],
            uniforms: &uniforms,
            buffers: &mut bufs,
            sampler: &mut NoTextures,
        };
        m.builtins[Builtin::LocalIdX as usize] = t as u32 % kernel::GROUP;
        m.builtins[Builtin::GroupIdX as usize] = t as u32 / kernel::GROUP;
        run(p, &mut m);
    }
    bufs.swap_remove(1)
}

#[test]
fn kernels_validate_and_compile() {
    for p in [
        kernel::closest_hit(kernel::Slots::default()),
        kernel::any_hit(kernel::Slots::default()),
    ] {
        let info = validate(&p).expect("valid");
        let c = crate::lower::compile(&p).expect("compiles");
        std::println!(
            "kernel: {} instructions, {} registers, {} local bytes, buffers {:#b}, code {} words",
            info.insts,
            c.gprs,
            c.local_bytes,
            info.buffers,
            c.code.len()
        );
    }
}

#[test]
fn the_closest_hit_kernel_matches_the_cpu() {
    let p = kernel::closest_hit(kernel::Slots::default());
    for (n, size, seed) in [(5, 4.0, 11), (300, 3.0, 12), (2500, 5.0, 13)] {
        let tris = soup(n, size, seed);
        let bvh = build(&tris).unwrap();
        let rays = rays(200, seed + 1);
        let out = run_kernel(&p, &bvh, &rays, HIT_WORDS);
        let mut hits = 0;
        for (i, r) in rays.iter().enumerate() {
            let want = trace::brute_force(&tris, r);
            let w = &out[i * HIT_WORDS..][..HIT_WORDS];
            let got = (w[3] != MISS).then(|| Hit {
                t: f32::from_bits(w[0]),
                u: f32::from_bits(w[1]),
                v: f32::from_bits(w[2]),
                id: w[3],
            });
            assert!(
                same(got, want),
                "n {n} ray {i}: kernel {got:?}, brute force {want:?}"
            );
            hits += got.is_some() as u32;
        }
        std::println!("n {n}: {hits}/200 rays hit");
    }
}

#[test]
fn the_any_hit_kernel_matches_the_cpu() {
    let p = kernel::any_hit(kernel::Slots::default());
    let tris = soup(1500, 4.0, 21);
    let bvh = build(&tris).unwrap();
    let mut rs = rays(300, 22);
    // Shadow-style rays: short ones too.
    for (i, r) in rs.iter_mut().enumerate() {
        if i % 2 == 0 {
            r.t_max = 3.0;
        }
    }
    let out = run_kernel(&p, &bvh, &rs, 1);
    for (i, r) in rs.iter().enumerate() {
        assert_eq!(out[i] != 0, trace::occluded(&bvh, r), "ray {i}");
    }
}

#[test]
fn the_compiled_kernels_match_the_reference_machine() {
    use crate::lower_tests::check_t;
    let tris = soup(800, 4.0, 31);
    let bvh = build(&tris).unwrap();
    // The simulator runs thread 7 * 64 + tid of the launch.
    let rs = rays(520, 32);
    let rays_buf: Vec<u32> = rs.iter().flat_map(|r| r.words()).collect();
    for (p, words) in [
        (kernel::closest_hit(kernel::Slots::default()), HIT_WORDS),
        (kernel::any_hit(kernel::Slots::default()), 1),
    ] {
        for tid in [0u32, 5, 63] {
            let r = check_t(
                &p,
                &[rs.len() as u32],
                &[
                    rays_buf.clone(),
                    vec![0xAAAA_AAAA; rs.len() * words],
                    bvh.nodes.clone(),
                    bvh.tris.clone(),
                ],
                tid,
            );
            assert!(r.is_ok(), "thread {tid}: {}", r.unwrap_err());
        }
    }
}

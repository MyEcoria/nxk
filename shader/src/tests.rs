use alloc::vec;
use alloc::vec::Vec;

use crate::interp::{run, Machine, NoTextures, Outputs};
use crate::*;

fn exec(
    p: &Program,
    inputs: &[(usize, usize, f32)],
    uniforms: &[u32],
    bufs: &mut [Vec<u32>],
) -> Outputs {
    validate(p).expect("valid");
    let mut m = Machine {
        inputs: [[0; 4]; MAX_ATTRS],
        builtins: [0; 11],
        uniforms,
        buffers: bufs,
        sampler: &mut NoTextures,
    };
    for &(a, c, v) in inputs {
        m.inputs[a][c] = v.to_bits();
    }
    run(p, &mut m)
}

fn no_bufs() -> Vec<Vec<u32>> {
    vec![Vec::new(); MAX_BUFFERS]
}

#[test]
fn arithmetic_and_select() {
    let mut b = Builder::new(Stage::Fragment);
    let x = b.input(0, 0);
    let two = b.f32(2.0);
    let one = b.f32(1.0);
    let y = b.fma(x, two, one); // 2x + 1
    let big = b.cmp(CmpOp::Gt, y, two);
    let hi = b.fmul(y, y);
    let r = b.sel(big, hi, y);
    b.output(OutKind::Color(0), 0, r);
    let n = b.fneg(r);
    b.output(OutKind::Color(0), 1, n);
    let p = b.finish();
    let o = exec(&p, &[(0, 0, 3.0)], &[], &mut no_bufs());
    assert_eq!(o.colors[0][0], 49.0);
    assert_eq!(o.colors[0][1], -49.0);
    let o = exec(&p, &[(0, 0, 0.25)], &[], &mut no_bufs());
    assert_eq!(o.colors[0][0], 1.5);
}

#[test]
fn loop_with_break_and_continue() {
    // Sum of the odd numbers below 10, stopping at 7.
    let mut b = Builder::new(Stage::Compute);
    let sum = b.u32(0);
    let i = b.u32(0);
    let one = b.u32(1);
    let seven = b.u32(7);
    b.loop_(100, |b| {
        let n = b.uadd(i, one);
        b.mov(i, n);
        let stop = b.cmp(CmpOp::Gt, i, seven);
        b.break_if(stop);
        let low = b.and(i, one);
        let zero = b.u32(0);
        let even = b.cmp(CmpOp::Eq, low, zero);
        b.continue_if(even);
        let s = b.uadd(sum, i);
        b.mov(sum, s);
    });
    let z = b.u32(0);
    b.store(0, z, 0, &[sum, i]);
    let p = b.finish();
    let mut bufs = no_bufs();
    bufs[0] = vec![0; 4];
    exec(&p, &[], &[], &mut bufs);
    assert_eq!(&bufs[0][..2], &[1 + 3 + 5 + 7, 8]);
}

#[test]
fn loop_limit_ends_the_loop() {
    let mut b = Builder::new(Stage::Compute);
    let n = b.u32(0);
    let one = b.u32(1);
    b.loop_(5, |b| {
        let s = b.uadd(n, one);
        b.mov(n, s);
    });
    let z = b.u32(0);
    b.store(0, z, 0, &[n]);
    let p = b.finish();
    let mut bufs = no_bufs();
    bufs[0] = vec![0];
    exec(&p, &[], &[], &mut bufs);
    assert_eq!(bufs[0][0], 5);
}

#[test]
fn buffer_bounds_and_alignment() {
    let mut b = Builder::new(Stage::Compute);
    let i = b.u32(2);
    let v = b.load_n(0, i, 0, &[Ty::U32, Ty::U32, Ty::U32, Ty::U32]); // misaligned: zeros
    let j = b.u32(4);
    let w = b.load_n(0, j, 0, &[Ty::U32, Ty::U32]); // 4..6 of 6 words: fine
    let k = b.u32(6);
    let x = b.load(0, k, 0, Ty::U32); // outside: 0
    let z = b.u32(0);
    b.store(1, z, 0, &[v[0], v[3], w[0], w[1]]);
    b.store(1, k, 0, &[x]); // dropped
    let p = b.finish();
    let mut bufs = no_bufs();
    bufs[0] = vec![10, 11, 12, 13, 14, 15];
    bufs[1] = vec![9; 6];
    exec(&p, &[], &[], &mut bufs);
    assert_eq!(bufs[1], vec![0, 0, 14, 15, 9, 9]);
}

#[test]
fn atomics_and_local_memory() {
    let mut b = Builder::new(Stage::Compute);
    b.local_words(4);
    let z = b.u32(0);
    let three = b.u32(3);
    let one = b.u32(1);
    b.local_store(three, one);
    let back = b.local_load(three, Ty::U32);
    let old = b.atomic_add(0, z, back);
    let old2 = b.atomic_add(0, z, three);
    let two = b.u32(2);
    b.store(0, two, 0, &[old, old2]);
    let p = b.finish();
    let mut bufs = no_bufs();
    bufs[0] = vec![10, 0, 0, 0];
    exec(&p, &[], &[], &mut bufs);
    assert_eq!(bufs[0], vec![14, 0, 10, 11]);
}

#[test]
fn nested_if_and_discard() {
    let mut b = Builder::new(Stage::Fragment);
    let x = b.input(0, 0);
    let zero = b.f32(0.0);
    let neg = b.cmp(CmpOp::Lt, x, zero);
    let out = b.f32(0.0);
    b.if_else(
        neg,
        |b| b.discard(),
        |b| {
            let half = b.f32(0.5);
            let big = b.cmp(CmpOp::Gt, x, half);
            b.if_else(
                big,
                |b| {
                    let one = b.f32(1.0);
                    b.mov(out, one);
                },
                |b| {
                    let two = b.f32(2.0);
                    b.mov(out, two);
                },
            );
        },
    );
    b.output(OutKind::Color(0), 0, out);
    let p = b.finish();
    assert!(exec(&p, &[(0, 0, -1.0)], &[], &mut no_bufs()).discarded);
    assert_eq!(
        exec(&p, &[(0, 0, 0.9)], &[], &mut no_bufs()).colors[0][0],
        1.0
    );
    assert_eq!(
        exec(&p, &[(0, 0, 0.1)], &[], &mut no_bufs()).colors[0][0],
        2.0
    );
}

#[test]
fn integer_ops() {
    let mut b = Builder::new(Stage::Compute);
    let a = b.i32(-7);
    let two = b.u32(1);
    let s = b.sar(a, two);
    let m = b.iabs(a);
    let mn = b.imin(a, s);
    let u = b.u32(0x8000_0000);
    let sh = b.u32(33); // taken modulo 32 = 1
    let r = b.shr(u, sh);
    let f = b.i2f(a);
    let g = b.f2u(f); // negative saturates to 0
    let z = b.u32(0);
    let sb = b.ubits(s);
    let mb = b.ubits(m);
    let nb = b.ubits(mn);
    b.store(0, z, 0, &[sb, mb, nb, r]);
    let four = b.u32(4);
    b.store(0, four, 0, &[g]);
    let p = b.finish();
    let mut bufs = no_bufs();
    bufs[0] = vec![0; 5];
    exec(&p, &[], &[], &mut bufs);
    assert_eq!(
        bufs[0],
        vec![(-4i32) as u32, 7, (-7i32) as u32, 0x4000_0000, 0]
    );
}

#[test]
fn validation_rejects() {
    // Read before assignment.
    let mut b = Builder::new(Stage::Compute);
    let r = b.reg(Ty::U32);
    let z = b.u32(0);
    b.store(0, z, 0, &[r]);
    assert_eq!(
        validate(&b.finish()).unwrap_err().0,
        "register read before it is assigned"
    );
    // Assigned in one branch only.
    let mut b = Builder::new(Stage::Compute);
    let r = b.reg(Ty::U32);
    let t = b.bool(true);
    b.if_(t, |b| {
        let one = b.u32(1);
        b.mov(r, one);
    });
    let z = b.u32(0);
    b.store(0, z, 0, &[r]);
    assert!(validate(&b.finish()).is_err());
    // Assigned in both: fine.
    let mut b = Builder::new(Stage::Compute);
    let r = b.reg(Ty::U32);
    let t = b.bool(true);
    b.if_else(
        t,
        |b| {
            let one = b.u32(1);
            b.mov(r, one);
        },
        |b| {
            let two = b.u32(2);
            b.mov(r, two);
        },
    );
    let z = b.u32(0);
    b.store(0, z, 0, &[r]);
    assert!(validate(&b.finish()).is_ok());
    // Stage rules.
    let mut b = Builder::new(Stage::Compute);
    let x = b.f32(0.0);
    b.output(OutKind::Position, 0, x);
    assert!(validate(&b.finish()).is_err());
    let mut b = Builder::new(Stage::Vertex);
    b.discard();
    assert!(validate(&b.finish()).is_err());
    // Break outside a loop, a zero trip limit.
    let mut b = Builder::new(Stage::Compute);
    b.break_();
    assert!(validate(&b.finish()).is_err());
    let mut b = Builder::new(Stage::Compute);
    b.loop_(0, |_| {});
    assert!(validate(&b.finish()).is_err());
    // Implicit derivatives outside a fragment shader.
    let mut b = Builder::new(Stage::Compute);
    let u = b.f32(0.5);
    b.sample(0, u, u);
    assert!(validate(&b.finish()).is_err());
}

#[test]
fn interface_is_reported() {
    let mut b = Builder::new(Stage::Fragment);
    let u = b.input(0, 0);
    let v = b.input_as(0, 1, Interp::Linear);
    let t = b.sample(3, u, v);
    let w = b.uniform(40, Ty::F32);
    let r = b.fmul(t[0], w);
    b.output(OutKind::Color(0), 0, r);
    let i = validate(&b.finish()).unwrap();
    assert_eq!(i.textures, 1 << 3);
    assert_eq!(i.uniform_words, 41);
    assert_eq!(i.inputs.len(), 2);
    assert_eq!(i.outputs, vec![(OutKind::Color(0), 0)]);
}

#[test]
fn wire_round_trip() {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(8, 4, 1);
    b.local_words(16);
    let z = b.u32(0);
    let acc = b.f32(0.0);
    b.loop_(10, |b| {
        let x = b.load(0, z, 0, Ty::F32);
        let s = b.fadd(acc, x);
        b.mov(acc, s);
        let big = b.cmp(CmpOp::Gt, acc, x);
        b.if_else(big, |b| b.break_(), |b| b.continue_());
    });
    let t = b.sample_lod(1, acc, acc, acc);
    let ab = b.fbits(t[2]);
    b.store(0, z, 3, &[ab]);
    b.atomic_add(0, z, z);
    let p = b.finish();
    let w = wire::encode(&p);
    assert_eq!(wire::decode(&w).unwrap(), p);
}

#[test]
fn decode_survives_garbage() {
    let mut b = Builder::new(Stage::Compute);
    let z = b.u32(0);
    b.loop_(4, |b| {
        let c = b.cmp(CmpOp::Eq, z, z);
        b.if_(c, |b| b.break_());
    });
    let w = wire::encode(&b.finish());
    // Every truncation and every single-word corruption: an error or a
    // program, never a panic.
    for n in 0..w.len() {
        let _ = wire::decode(&w[..n]);
    }
    let mut seed = 0x1234_5678u32;
    for _ in 0..4000 {
        let mut c = w.clone();
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let at = (seed >> 8) as usize % c.len();
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        c[at] = seed;
        if let Ok(p) = wire::decode(&c) {
            let _ = validate(&p);
        }
    }
}

#[test]
fn math_accuracy() {
    use crate::fmath::*;
    let close = |a: f32, b: f32, tol: f32| {
        assert!((a - b).abs() <= tol * b.abs().max(1.0), "{a} vs {b}");
    };
    for k in -200..=200 {
        let x = k as f32 * 0.37;
        close(sin(x), x.sin(), 2e-5);
        close(cos(x), x.cos(), 2e-5);
    }
    for k in -120..=120 {
        let x = k as f32 * 0.9;
        close(ex2(x), x.exp2(), 3e-6);
    }
    for k in 1..2000 {
        let x = k as f32 * 0.173;
        close(lg2(x), x.log2(), 3e-6);
        close(sqrt(x), x.sqrt(), 2e-6);
        close(rsq(x), 1.0 / x.sqrt(), 2e-6);
    }
    for x in [-3.5f32, -1.0, -0.2, 0.0, 0.7, 2.0, 1e7, -1e7] {
        assert_eq!(floor(x), x.floor());
    }
}

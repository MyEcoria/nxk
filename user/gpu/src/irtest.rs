//! The shader compiler checked on the GPU itself: random programs of the
//! IR run by 64 threads each, against the reference machine on the CPU,
//! and a vertex and fragment shader pair drawing a triangle. The
//! emulator runs no shaders, so this is for a console; a failure names
//! the seed and the thread.

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::proto::gpu as proto;
use libnx::println;
use nxk_shader::fuzz::{random_program, Rng};
use nxk_shader::interp::{self, Machine, NoTextures};
use nxk_shader::lower::{self, CB_BUFFERS, CB_TEXTURES};
use nxk_shader::{
    fmath, Builder, CmpOp, Interp, OutKind, Program, Stage, Ty, MAX_ATTRS, MAX_BUFFERS,
};

use crate::gfx::{self, attr_word, Custom, Layout};
use crate::hw::Dma;
use crate::mmu;
use crate::push::Push;
use crate::qmd::{Launch3, Qmd};
use crate::{Gpu, BATCH_TIMEOUT_US};

type Res<T> = core::result::Result<T, i64>;

const SEEDS: u64 = 16;
const THREADS: u32 = 64;
const OUT_WORDS: usize = 64 * 64;
const GARBAGE: u32 = 0xAAAA_AAAA;

/// Programs with a transcendental function: the multi-function unit is
/// approximate (a couple of ulps, more near a zero or a pole), and the
/// random chains after it amplify that.
fn uses_mufu(p: &Program) -> bool {
    let mut found = false;
    nxk_shader::walk(&p.body, &mut |i| {
        if let nxk_shader::Inst::Un { op, .. } = i {
            found |= matches!(
                op,
                nxk_shader::UnOp::FSin
                    | nxk_shader::UnOp::FCos
                    | nxk_shader::UnOp::FEx2
                    | nxk_shader::UnOp::FLg2
                    | nxk_shader::UnOp::FRcp
                    | nxk_shader::UnOp::FRsq
                    | nxk_shader::UnOp::FSqrt
            );
        }
    });
    found
}

fn same(a: u32, b: u32, tol: f32) -> bool {
    if a == b {
        return true;
    }
    let (x, y) = (f32::from_bits(a), f32::from_bits(b));
    if x.is_nan() && y.is_nan() {
        return true;
    }
    if x == y {
        return true;
    }
    if x.is_infinite() || y.is_infinite() {
        return false;
    }
    fmath::abs(x - y) <= tol * fmath::abs(x).max(fmath::abs(y)).max(1e-3)
}

/// The random programs of the IR on the GPU and the CPU.
pub fn compute(g: &mut Gpu) -> Res<(bool, u64)> {
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let d = g.dev(0)?;
    let inb = Dma::new(64).map_err(e)?;
    let out = Dma::new(OUT_WORDS * 4).map_err(e)?;
    let atom = Dma::new(16).map_err(e)?;
    let vas = [
        d.vm.map_cached(&inb).map_err(e)?,
        d.vm.map_cached(&out).map_err(e)?,
        d.vm.map_cached(&atom).map_err(e)?,
    ];
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let (mut ok, mut ns, mut bad_seeds) = (true, 0u64, 0u32);
    for seed in 1..=SEEDS {
        let prog = random_program(seed, true);
        let c = match lower::compile(&prog) {
            Ok(c) => c,
            Err(err) => {
                println!("[gpu] IR seed {}: does not compile: {:?}", seed, err);
                ok = false;
                continue;
            }
        };
        // Inputs: four floats, four unsigned, four signed, four free.
        let mut rng = Rng(seed * 31);
        let mut b0: Vec<u32> = (0..4).map(|_| rng.float().to_bits()).collect();
        b0.extend((0..4).map(|_| rng.next_u32() % 40));
        b0.extend((0..4).map(|_| (rng.next_u32() % 200).wrapping_sub(100)));
        b0.extend((0..4).map(|_| rng.next_u32()));
        inb.words(0, &b0);
        for i in 0..OUT_WORDS {
            out.w32(i * 4, GARBAGE);
        }
        atom.words(0, &[0; 4]);

        // Reference, thread by thread: with exact transcendentals, and
        // with the error of the hardware's approximate unit added. An
        // output the noise moves is not a test of the compiler.
        let reference = |err: f32| {
            use core::sync::atomic::Ordering;
            interp::MUFU_ERROR_BITS.store(err.to_bits(), Ordering::Relaxed);
            let mut bufs: Vec<Vec<u32>> = alloc::vec![Vec::new(); MAX_BUFFERS];
            bufs[0] = b0.clone();
            bufs[1] = alloc::vec![GARBAGE; OUT_WORDS];
            bufs[2] = alloc::vec![0; 4];
            for tid in 0..THREADS {
                let mut m = Machine {
                    inputs: [[0; 4]; MAX_ATTRS],
                    builtins: [0; 11],
                    uniforms: &[],
                    buffers: &mut bufs,
                    sampler: &mut NoTextures,
                };
                m.builtins[nxk_shader::Builtin::LocalIdX as usize] = tid;
                interp::run(&prog, &mut m);
            }
            interp::MUFU_ERROR_BITS.store(0, Ordering::Relaxed);
            bufs.swap_remove(1)
        };
        let want = reference(0.0);
        let noisy = if uses_mufu(&prog) {
            reference(4e-6)
        } else {
            want.clone()
        };

        // The GPU.
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        let Some((at, len)) = d.compute.install(&c.code) else {
            println!("[gpu] IR seed {}: no room for the code", seed);
            return Err(-libnx::abi::err::ENOSPC);
        };
        let mut p = Push::default();
        d.gfx.begin(&mut p);
        let mut desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
        for (k, (va, words)) in vas.iter().zip([16u32, OUT_WORDS as u32, 4]).enumerate() {
            desc[4 * k] = *va as u32;
            desc[4 * k + 1] = (*va >> 32) as u32;
            desc[4 * k + 2] = words;
        }
        let handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
        let stage = |gfx: &mut gfx::Gfx, w: &[u32]| gfx.stage(w).ok_or(-libnx::abi::err::ENOSPC);
        let cbs = [
            stage(&mut d.gfx, &[0; 64])?,
            stage(&mut d.gfx, &[0])?,
            stage(&mut d.gfx, &desc)?,
            stage(&mut d.gfx, &handles)?,
        ];
        debug_assert_eq!((CB_BUFFERS, CB_TEXTURES), (2, 3));
        let gprs = c.gprs.next_multiple_of(8).max(8);
        let (qat, qva) = d
            .gfx
            .alloc_ubo(crate::qmd::SIZE)
            .ok_or(-libnx::abi::err::ENOSPC)?;
        let q = Qmd::new3(&Launch3 {
            program: at,
            regs: gprs,
            grid: [1, 1, 1],
            block: [THREADS, 1, 1],
            cbs: &cbs,
            local_bytes: c.local_bytes,
        });
        d.gfx.ubo_write(qat, &q.bytes());
        d.gfx.barrier(&mut p);
        d.compute.launch(&mut p, qva);
        d.gfx.barrier(&mut p);
        let t = g.run(p, BATCH_TIMEOUT_US);
        let first: Vec<u32> = (0..OUT_WORDS).map(|i| out.r32(i * 4)).collect();
        // The same launch again: the results must not change from run to
        // run.
        let mut repeat = 0;
        if t.is_ok() {
            for i in 0..OUT_WORDS {
                out.w32(i * 4, GARBAGE);
            }
            let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
            let mut p2 = Push::default();
            d.gfx.barrier(&mut p2);
            d.compute.launch(&mut p2, qva);
            d.gfx.barrier(&mut p2);
            if g.run(p2, BATCH_TIMEOUT_US).is_ok() {
                repeat = (0..OUT_WORDS)
                    .filter(|&i| out.r32(i * 4) != first[i])
                    .count();
            }
        }
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        d.compute.uninstall(at, len);
        ns += t?;
        if repeat != 0 {
            println!(
                "[gpu] IR seed {}: {} words differ between two runs of the same launch",
                seed, repeat
            );
        }

        let tol = if uses_mufu(&prog) { 2e-3 } else { 1e-4 };
        let wrong: Vec<usize> = (0..OUT_WORDS)
            .filter(|&i| {
                let got = out.r32(i * 4);
                // Right if it matches either reference, or the noise
                // alone moves the word more than the tolerance.
                !(same(got, want[i], tol)
                    || same(got, noisy[i], tol)
                    || !same(want[i], noisy[i], 10.0 * tol))
            })
            .collect();
        if let Some(&i) = wrong.first() {
            println!(
                "[gpu] IR seed {}: {} of {} words wrong, first [{}] (thread {}, word {}) {:#x}, expected {:#x} ({} registers)",
                seed,
                wrong.len(),
                OUT_WORDS,
                i,
                i / 64,
                i % 64,
                out.r32(i * 4),
                want[i],
                c.gprs
            );
            ok = false;
            bad_seeds += 1;
        }
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    for (va, len) in [
        (vas[0], inb.len()),
        (vas[1], out.len()),
        (vas[2], atom.len()),
    ] {
        let _ = d.vm.unmap(va, len as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    println!(
        "[gpu] IR compute: {} of {} programs match the reference on {} threads",
        SEEDS as u32 - bad_seeds,
        SEEDS,
        THREADS
    );
    Ok((ok, ns))
}

/// A shader of a draw case: a built-in one (program header offset,
/// registers) or one of the IR.
enum Sh<'a> {
    Builtin(u32, u32),
    Ir(&'a Program),
}

/// One triangle through a custom pipeline, the centre pixel against
/// `want`. A hang resets the GPU: the next case starts it again.
fn draw_case(g: &mut Gpu, name: &str, vs: Sh, fs: Sh, u: [f32; 4], want: u32) -> Res<(bool, u64)> {
    const W: u32 = 64;
    const H: u32 = 64;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let compile = |s: &Sh| -> core::result::Result<Option<lower::Compiled>, lower::Error> {
        match s {
            Sh::Builtin(..) => Ok(None),
            Sh::Ir(p) => lower::compile(p).map(Some),
        }
    };
    let (cv, cf) = match (compile(&vs), compile(&fs)) {
        (Ok(v), Ok(f)) => (v, f),
        (v, f) => {
            println!(
                "[gpu] IR draw ({}): does not compile: vertex {:?}, fragment {:?}",
                name,
                v.err(),
                f.err()
            );
            return Ok((false, 0));
        }
    };
    let d = g.dev(0)?;
    let img = Dma::new((W * H * 4) as usize).map_err(e)?;
    let vb = Dma::new(4096).map_err(e)?;
    let tex_bytes = proto::tiled_bytes(8, 8, proto::tex_block_log2(8));
    let tex = Dma::new(tex_bytes).map_err(e)?;
    // Every texel the same: (255, 128, 64, 255) in R, G, B, A order.
    for i in 0..tex_bytes / 4 {
        tex.w32(i * 4, 0xFF40_80FF);
    }
    let s = crate::engine::Surface {
        va: d.vm.map(&img, false).map_err(e)?,
        width: W,
        height: H,
        pitch: W * 4,
        format: proto::FMT_A8B8G8R8 as u32,
    };
    let vbv = d.vm.map_cached(&vb).map_err(e)?;
    let tva =
        d.vm.map_kind(&tex, false, mmu::KIND_GENERIC_16BX2, 1 << 17)
            .map_err(e)?;
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let img8 = gfx::TexImage {
        va: tva,
        width: 8,
        height: 8,
        format: proto::TF_RGBA8,
        block_log2: proto::tex_block_log2(8),
        levels: 1,
    };
    d.gfx.set_texture(0, &img8, 0);
    // Installed shaders: (header, registers, block, length).
    let place = |d: &mut crate::Dev, sh: &Sh, c: &Option<lower::Compiled>| match (sh, c) {
        (Sh::Builtin(at, gprs), _) => Some((*at, *gprs, 0, 0)),
        (_, Some(c)) => d
            .gfx
            .install(&c.header, &c.code)
            .map(|(h, b, l)| (h, c.gprs.next_multiple_of(8).max(8), b, l)),
        _ => None,
    };
    let (Some(v), Some(f)) = (place(d, &vs, &cv), place(d, &fs, &cf)) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut attrs = Vec::new();
    attrs.push(attr_word(0, proto::AF_F32X4).unwrap());
    attrs.push(attr_word(16, proto::AF_F32X4).unwrap());
    attrs.resize(proto::MAX_ATTRIBUTES, gfx::ATTR_INACTIVE);
    let custom = Custom {
        vs: v.0,
        vs_gprs: v.1,
        fs: f.0,
        fs_gprs: f.1,
        stride: 32,
        attrs,
        inst_stride: 0,
        flags: 0,
        blend: proto::BLEND_NONE,
        depth: proto::DEPTH_LESS,
    };
    // One big triangle with a flat colour.
    let tri: [f32; 24] = [
        -1.0, -1.0, 0.5, 1.0, 0.8, 0.4, 0.6, 1.0, //
        3.0, -1.0, 0.5, 1.0, 0.8, 0.4, 0.6, 1.0, //
        -1.0, 3.0, 0.5, 1.0, 0.8, 0.4, 0.6, 1.0,
    ];
    for (i, x) in tri.iter().enumerate() {
        vb.w32(i * 4, x.to_bits());
    }
    let target = gfx::Target {
        color: gfx::Color::Linear(s),
        depth: None,
    };
    img.words(0, &alloc::vec![0xDEAD_BEEF; (W * H) as usize]);
    let mut p = Push::default();
    d.gfx.begin(&mut p);
    d.gfx.clear(&mut p, &target, [0.1, 0.2, 0.3, 1.0]);
    d.gfx.pipeline_custom(&mut p, &target, &custom);
    let bits: Vec<u32> = u.iter().map(|x| x.to_bits()).collect();
    let desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
    let mut handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
    handles[0] = gfx::Gfx::texture_handle(0);
    let staged = d.gfx.uniforms(&mut p, &bits)
        && d.gfx.cbuf(&mut p, 2, &desc)
        && d.gfx.cbuf(&mut p, 3, &handles);
    if !staged {
        return Err(-libnx::abi::err::ENOSPC);
    }
    let lay = Layout {
        stride: custom.stride,
        attrs: &custom.attrs,
        inst_stride: 0,
    };
    d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
    let near = |a: u32, b: u32, tol: u32| {
        (0..4).all(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF) <= tol)
    };
    match g.run(p, BATCH_TIMEOUT_US) {
        Ok(ns) => {
            let got = img.r32(((H / 2 * W + W / 2) * 4) as usize);
            println!(
                "[gpu] IR draw ({}): centre {:#010x}, expected {:#010x}",
                name, got, want
            );
            let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
            for sh in [v, f] {
                if sh.3 != 0 {
                    d.gfx.uninstall(sh.2, sh.3);
                }
            }
            d.gfx.clear_texture(0);
            let _ = d.vm.unmap(s.va, img.len() as u64);
            let _ = d.vm.unmap(vbv, vb.len() as u64);
            let _ = d.vm.unmap(tva, tex.len() as u64);
            let _ = mmu::invalidate(&regs, &d.vm);
            Ok((near(got, want, 2), ns))
        }
        Err(err) => {
            let n = |c: &Option<lower::Compiled>| c.as_ref().map_or(0, |c| c.code.len());
            println!(
                "[gpu] IR draw ({}): the GPU hung ({}); vertex {} words, fragment {} words",
                name,
                err,
                n(&cv),
                n(&cf)
            );
            Ok((false, 0))
        }
    }
}

/// Vertex and fragment shaders of the IR drawing a triangle, from the
/// built-in pair through the custom path up to varying, uniform, texture
/// and discard: the first that fails (or hangs the GPU) names the cause.
pub fn draw(g: &mut Gpu) -> Res<(bool, u64)> {
    // Vertex shaders: position only, or position and colour through.
    let mut b = Builder::new(Stage::Vertex);
    for c in 0..4u8 {
        let p = b.input(0, c);
        b.output(OutKind::Position, c, p);
    }
    let vs_pos = b.finish();
    let mut b = Builder::new(Stage::Vertex);
    for c in 0..4u8 {
        let p = b.input(0, c);
        b.output(OutKind::Position, c, p);
        let col = b.input(1, c);
        b.output(OutKind::Varying(0), c, col);
    }
    let vs_full = b.finish();
    // Fragment shaders, each adding a feature.
    let frag = |f: &dyn Fn(&mut Builder)| {
        let mut b = Builder::new(Stage::Fragment);
        f(&mut b);
        b.finish()
    };
    let fs_const = frag(&|b| {
        for (c, v) in [0.4f32, 0.2, 0.1, 1.0].into_iter().enumerate() {
            let r = b.f32(v);
            b.output(OutKind::Color(0), c as u8, r);
        }
    });
    let fs_varying = frag(&|b| {
        for c in 0..4u8 {
            let v = b.input(0, c);
            b.output(OutKind::Color(0), c, v);
        }
    });
    let fs_uniform = frag(&|b| {
        for c in 0..4u8 {
            let v = b.input(0, c);
            let u = b.uniform(c as u16, Ty::F32);
            let r = b.fmul(v, u);
            b.output(OutKind::Color(0), c, r);
        }
    });
    let fs_texture = frag(&|b| {
        let half = b.f32(0.5);
        let t = b.sample(0, half, half);
        for c in 0..4u8 {
            b.output(OutKind::Color(0), c, t[c as usize]);
        }
    });
    let fs_full = frag(&|b| {
        let zero = b.f32(0.0);
        let u0 = b.uniform(0, Ty::F32);
        let dead = b.cmp(CmpOp::Lt, u0, zero);
        b.discard_if(dead);
        let half = b.f32(0.5);
        let t = b.sample(0, half, half);
        for c in 0..4u8 {
            let v = b.input_as(0, c, Interp::Perspective);
            let u = b.uniform(c as u16, Ty::F32);
            let m = b.fmul(v, u);
            let r = b.fmul(m, t[c as usize]);
            b.output(OutKind::Color(0), c, r);
        }
    });
    let hv = Sh::Builtin(gfx::FLAT_VS_AT, gfx::FLAT_GPRS);
    let hf = Sh::Builtin(gfx::FLAT_FS_AT, gfx::FLAT_GPRS);
    let rgb = |r: u32, g: u32, b: u32| 0xFF00_0000 | b << 16 | g << 8 | r;
    let uni = [0.5f32, 1.0, 0.25, 1.0];
    let clear_px = rgb(26, 51, 77);
    let (mut ok, mut ns) = (true, 0u64);
    // Each case builds on the one before.
    let cases: [(&str, Sh, Sh, [f32; 4], u32); 9] = [
        (
            "built-in pair, custom path",
            hv,
            hf,
            uni,
            rgb(204, 102, 153),
        ),
        (
            "IR vertex, built-in fragment",
            Sh::Ir(&vs_full),
            Sh::Builtin(gfx::FLAT_FS_AT, gfx::FLAT_GPRS),
            uni,
            rgb(204, 102, 153),
        ),
        (
            "built-in vertex, IR fragment",
            Sh::Builtin(gfx::FLAT_VS_AT, gfx::FLAT_GPRS),
            Sh::Ir(&fs_varying),
            uni,
            rgb(204, 102, 153),
        ),
        (
            "constant colour",
            Sh::Ir(&vs_pos),
            Sh::Ir(&fs_const),
            uni,
            rgb(102, 51, 26),
        ),
        (
            "varying",
            Sh::Ir(&vs_full),
            Sh::Ir(&fs_varying),
            uni,
            rgb(204, 102, 153),
        ),
        (
            "uniform",
            Sh::Ir(&vs_full),
            Sh::Ir(&fs_uniform),
            uni,
            rgb(102, 102, 38),
        ),
        (
            "texture",
            Sh::Ir(&vs_full),
            Sh::Ir(&fs_texture),
            uni,
            rgb(255, 128, 64),
        ),
        (
            "all together",
            Sh::Ir(&vs_full),
            Sh::Ir(&fs_full),
            uni,
            rgb(102, 51, 10),
        ),
        (
            "discard",
            Sh::Ir(&vs_full),
            Sh::Ir(&fs_full),
            [-1.0, 1.0, 0.25, 1.0],
            clear_px,
        ),
    ];
    for (name, vs, fs, u, want) in cases {
        let (pass, t) = draw_case(g, name, vs, fs, u, want)?;
        ok &= pass;
        ns += t;
    }
    Ok((ok, ns))
}

/// Four instances of one triangle: the vertex shader moves each by its
/// own offset (attribute 1, read from the instance stream) and colors it
/// from the instance's data and from `InstanceId`.
pub fn instanced(g: &mut Gpu) -> Res<(bool, u64)> {
    const W: u32 = 64;
    const H: u32 = 64;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;

    let mut b = Builder::new(Stage::Vertex);
    let (x, y) = (b.input(0, 0), b.input(0, 1));
    let (dx, dy) = (b.input(1, 0), b.input(1, 1));
    let (px, py) = (b.fadd(x, dx), b.fadd(y, dy));
    let (z, one, quarter) = (b.f32(0.5), b.f32(1.0), b.f32(0.25));
    for (c, r) in [px, py, z, one].into_iter().enumerate() {
        b.output(OutKind::Position, c as u8, r);
    }
    let id = b.builtin(nxk_shader::Builtin::InstanceId);
    let idf = b.u2f(id);
    let blue = b.fmul(idf, quarter);
    for (c, r) in [b.input(1, 2), b.input(1, 3), blue, one]
        .into_iter()
        .enumerate()
    {
        b.output(OutKind::Varying(0), c as u8, r);
    }
    let vs = b.finish();
    let mut b = Builder::new(Stage::Fragment);
    for c in 0..4u8 {
        let v = b.input_as(0, c, Interp::Perspective);
        b.output(OutKind::Color(0), c, v);
    }
    let fs = b.finish();
    let (cv, cf) = match (lower::compile(&vs), lower::compile(&fs)) {
        (Ok(v), Ok(f)) => (v, f),
        (v, f) => {
            println!(
                "[gpu] IR instanced: does not compile: vertex {:?}, fragment {:?}",
                v.err(),
                f.err()
            );
            return Ok((false, 0));
        }
    };

    let d = g.dev(0)?;
    let img = Dma::new((W * H * 4) as usize).map_err(e)?;
    let vb = Dma::new(4096).map_err(e)?;
    let ib = Dma::new(4096).map_err(e)?;
    let s = crate::engine::Surface {
        va: d.vm.map(&img, false).map_err(e)?,
        width: W,
        height: H,
        pitch: W * 4,
        format: proto::FMT_A8B8G8R8 as u32,
    };
    let (vbv, ibv) = (
        d.vm.map_cached(&vb).map_err(e)?,
        d.vm.map_cached(&ib).map_err(e)?,
    );
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let Some((vh, vblock, vlen)) = d.gfx.install(&cv.header, &cv.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((fh, fblock, flen)) = d.gfx.install(&cf.header, &cf.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut attrs = Vec::new();
    attrs.push(attr_word(0, proto::AF_F32X2).unwrap());
    attrs.push(attr_word(0, proto::AF_F32X4).unwrap() | gfx::INSTANCE_STREAM);
    attrs.resize(proto::MAX_ATTRIBUTES, gfx::ATTR_INACTIVE);
    let custom = Custom {
        vs: vh,
        vs_gprs: cv.gprs.next_multiple_of(8).max(8),
        fs: fh,
        fs_gprs: cf.gprs.next_multiple_of(8).max(8),
        stride: 8,
        attrs,
        inst_stride: 16,
        flags: 0,
        blend: proto::BLEND_NONE,
        depth: proto::DEPTH_LESS,
    };
    // A small triangle; each instance: offset (x, y), then two color
    // channels.
    let tri: [f32; 6] = [-0.2, -0.2, 0.2, -0.2, 0.0, 0.2];
    for (i, v) in tri.iter().enumerate() {
        vb.w32(i * 4, v.to_bits());
    }
    let inst: [[f32; 4]; 4] = [
        [-0.5, -0.5, 1.0, 0.0],
        [0.5, -0.5, 0.0, 1.0],
        [-0.5, 0.5, 1.0, 1.0],
        [0.5, 0.5, 0.5, 0.5],
    ];
    for (i, v) in inst.iter().flatten().enumerate() {
        ib.w32(i * 4, v.to_bits());
    }
    let target = gfx::Target {
        color: gfx::Color::Linear(s),
        depth: None,
    };
    img.words(0, &alloc::vec![0xDEAD_BEEF; (W * H) as usize]);
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    let mut p = Push::default();
    d.gfx.begin(&mut p);
    d.gfx.clear(&mut p, &target, [0.1, 0.2, 0.3, 1.0]);
    d.gfx.pipeline_custom(&mut p, &target, &custom);
    let desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
    let handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
    let staged = d.gfx.uniforms(&mut p, &[0])
        && d.gfx.cbuf(&mut p, 2, &desc)
        && d.gfx.cbuf(&mut p, 3, &handles);
    if !staged {
        return Err(-libnx::abi::err::ENOSPC);
    }
    let lay = Layout {
        stride: custom.stride,
        attrs: &custom.attrs,
        inst_stride: custom.inst_stride,
    };
    let instances = gfx::Instances { va: ibv, count: 4 };
    d.gfx
        .draw_layout_instanced(&mut p, &lay, vbv, 3, 0, 3, &instances);
    let ns = g.run(p, BATCH_TIMEOUT_US)?;
    let px = |x: u32, y: u32| img.r32(((y * W + x) * 4) as usize);
    let near = |a: u32, b: u32| {
        (0..4).all(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF) <= 2)
    };
    // The triangle of instance i is around (16 + 32 x, 16 + 32 y); the
    // pixel word is A, B, G, R from the top byte.
    let word = |r: u32, g: u32, b: u32| 0xFF00_0000 | b << 16 | g << 8 | r;
    let mut ok = true;
    let clear_px = word(26, 51, 77);
    for (x, y, want, name) in [
        (16, 16, word(255, 0, 0), "instance 0"),
        (48, 16, word(0, 255, 64), "instance 1"),
        (16, 48, word(255, 255, 128), "instance 2"),
        (48, 48, word(128, 128, 191), "instance 3"),
        (32, 32, clear_px, "between"),
        (2, 2, clear_px, "corner"),
    ] {
        let got = px(x, y);
        println!(
            "[gpu] IR instanced ({}): {:#010x}, expected {:#010x}",
            name, got, want
        );
        if !near(got, want) {
            ok = false;
        }
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    d.gfx.uninstall(vblock, vlen);
    d.gfx.uninstall(fblock, flen);
    for (va, len) in [(s.va, img.len()), (vbv, vb.len()), (ibv, ib.len())] {
        let _ = d.vm.unmap(va, len as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    Ok((ok, ns))
}

/// Render to a float target, then sample it: a pass fills a block-linear
/// R32F, RGBA16F or RGBA32F image (values past 1.0 included) with a
/// gradient, a barrier, and a second pass reads it back through a
/// texture of the same memory into an RGBA8 image, scaled down.
fn targets_one(g: &mut Gpu, tf1: u64, name1: &'static str) -> Res<(bool, u64)> {
    const N: u32 = 16;
    const PITCH: u32 = 256;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;

    // First pass: (4u, 4v, 2, 0.5) from the fullscreen triangle's uv.
    let vs = nxk_shader::stock::fullscreen_vertex();
    let mut b = Builder::new(Stage::Fragment);
    let four = b.f32(4.0);
    let (u, v) = (
        b.input_as(0, 0, Interp::Perspective),
        b.input_as(0, 1, Interp::Perspective),
    );
    let (r, gr) = (b.fmul(u, four), b.fmul(v, four));
    let (two, half) = (b.f32(2.0), b.f32(0.5));
    for (c, x) in [r, gr, two, half].into_iter().enumerate() {
        b.output(OutKind::Color(0), c as u8, x);
    }
    let fs1 = b.finish();
    // Second pass: the texel at the pixel's uv, divided by 4 (alpha as is).
    let mut b = Builder::new(Stage::Fragment);
    let quarter = b.f32(0.25);
    let (u, v) = (
        b.input_as(0, 0, Interp::Perspective),
        b.input_as(0, 1, Interp::Perspective),
    );
    let t = b.sample(0, u, v);
    for c in 0..3u8 {
        let x = b.fmul(t[c as usize], quarter);
        b.output(OutKind::Color(0), c, x);
    }
    b.output(OutKind::Color(0), 3, t[3]);
    let fs2 = b.finish();
    let (cv, c1, c2) = match (
        lower::compile(&vs),
        lower::compile(&fs1),
        lower::compile(&fs2),
    ) {
        (Ok(v), Ok(a), Ok(b)) => (v, a, b),
        (v, a, b) => {
            println!(
                "[gpu] IR targets: does not compile: vertex {:?}, fragments {:?} {:?}",
                v.err(),
                a.err(),
                b.err()
            );
            return Ok((false, 0));
        }
    };

    let d = g.dev(0)?;
    // A pitch-linear target wants a pitch of 256 bytes or more (a 64-byte
    // one, 16 pixels, stopped the front end at the first draw).
    let img = Dma::new((N * PITCH) as usize).map_err(e)?;
    let vb = Dma::new(4096).map_err(e)?;
    let s = crate::engine::Surface {
        va: d.vm.map(&img, false).map_err(e)?,
        width: N,
        height: N,
        pitch: PITCH,
        format: proto::FMT_A8B8G8R8 as u32,
    };
    let vbv = d.vm.map_cached(&vb).map_err(e)?;
    for (i, x) in [-1.0f32, -1.0, 3.0, -1.0, -1.0, 3.0].iter().enumerate() {
        vb.w32(i * 4, x.to_bits());
    }
    let Some((vh, vblock, vlen)) = d.gfx.install(&cv.header, &cv.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((f1, f1block, f1len)) = d.gfx.install(&c1.header, &c1.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((f2, f2block, f2len)) = d.gfx.install(&c2.header, &c2.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut attrs = Vec::new();
    attrs.push(attr_word(0, proto::AF_F32X2).unwrap());
    attrs.resize(proto::MAX_ATTRIBUTES, gfx::ATTR_INACTIVE);
    let pipe = |fs: u32, gprs: u32| Custom {
        vs: vh,
        vs_gprs: cv.gprs.next_multiple_of(8).max(8),
        fs,
        fs_gprs: gprs.next_multiple_of(8).max(8),
        stride: 8,
        attrs: attrs.clone(),
        inst_stride: 0,
        flags: proto::PF_NO_DEPTH_TEST | proto::PF_NO_DEPTH_WRITE,
        blend: proto::BLEND_NONE,
        depth: proto::DEPTH_LESS,
    };
    let (p1, p2) = (pipe(f1, c1.gprs), pipe(f2, c2.gprs));
    let lay = Layout {
        stride: 8,
        attrs: &p1.attrs,
        inst_stride: 0,
    };
    let px = |x: u32, y: u32| img.r32((y * PITCH + x * 4) as usize);
    let near = |a: u32, b: u32| {
        (0..4).all(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF) <= 3)
    };
    let word = |r: u32, g: u32, b: u32, a: u32| a << 24 | b << 16 | g << 8 | r;
    let mut ok = true;
    let mut ns = 0;
    for (tf, name) in [(tf1, name1)] {
        println!("[gpu] IR targets ({}): starting", name);
        let bpp = proto::tf_bytes(tf);
        let bl = proto::tex_block_log2(N);
        let fmt = gfx::color_format(tf).ok_or(-libnx::abi::err::EINVAL)?;
        let mem =
            Dma::new(proto::tiled_bytes_bpp(N, N, bpp, bl).next_multiple_of(4096)).map_err(e)?;
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        // The same memory twice, as the client's two attaches make it.
        let kind = mmu::KIND_GENERIC_16BX2;
        let (tva, xva) = (
            d.vm.map_kind(&mem, false, kind, 1 << 17).map_err(e)?,
            d.vm.map_kind(&mem, false, kind, 1 << 17).map_err(e)?,
        );
        mmu::invalidate(&regs, &d.vm).map_err(e)?;
        let tiled = gfx::Tiled {
            va: tva,
            width: N,
            height: N,
            format: fmt,
            bpp,
            block_log2: bl,
        };
        let image = gfx::TexImage {
            va: xva,
            width: N,
            height: N,
            format: tf,
            block_log2: bl,
            levels: 1,
        };
        d.gfx.set_texture(0, &image, proto::TEX_CLAMP);
        let first = gfx::Target {
            color: gfx::Color::Tiled(tiled),
            depth: None,
        };
        let second = gfx::Target {
            color: gfx::Color::Linear(s),
            depth: None,
        };
        img.words(0, &alloc::vec![0xDEAD_BEEF; (N * PITCH / 4) as usize]);
        let mut p = Push::default();
        d.gfx.begin(&mut p);
        let desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
        let mut handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
        handles[0] = gfx::Gfx::texture_handle(0);
        d.gfx.pipeline_custom(&mut p, &first, &p1);
        let staged = d.gfx.uniforms(&mut p, &[0])
            && d.gfx.cbuf(&mut p, 2, &desc)
            && d.gfx.cbuf(&mut p, 3, &handles);
        if !staged {
            return Err(-libnx::abi::err::ENOSPC);
        }
        d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
        // Two batches while a hang is hunted: the first pass alone.
        match g.run(p, BATCH_TIMEOUT_US) {
            Ok(t) => ns += t,
            Err(_) => {
                println!("[gpu] IR targets ({}): the first pass hung", name);
                ok = false;
                continue;
            }
        }
        println!("[gpu] IR targets ({}): first pass done", name);
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        let mut p = Push::default();
        d.gfx.begin(&mut p);
        d.gfx.pipeline_custom(&mut p, &second, &p2);
        let staged = d.gfx.uniforms(&mut p, &[0])
            && d.gfx.cbuf(&mut p, 2, &desc)
            && d.gfx.cbuf(&mut p, 3, &handles);
        if !staged {
            return Err(-libnx::abi::err::ENOSPC);
        }
        d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
        match g.run(p, BATCH_TIMEOUT_US) {
            Ok(t) => ns += t,
            Err(_) => {
                println!("[gpu] IR targets ({}): the second pass hung", name);
                ok = false;
                continue;
            }
        }
        let mut bad = 0;
        for (x, y) in [(0u32, 0u32), (3, 2), (8, 9), (15, 15), (5, 12)] {
            let (u, v) = ((x as f32 + 0.5) / N as f32, (y as f32 + 0.5) / N as f32);
            let (r8, g8) = ((u * 255.0 + 0.5) as u32, (v * 255.0 + 0.5) as u32);
            // R32F reads as (r, 0, 0, 1).
            let want = if tf == proto::TF_R32F {
                word(r8, 0, 0, 255)
            } else {
                word(r8, g8, 128, 128)
            };
            let got = px(x, y);
            if !near(got, want) {
                println!(
                    "[gpu] IR targets {}: pixel ({}, {}) {:#010x}, expected {:#010x}",
                    name, x, y, got, want
                );
                bad += 1;
            }
        }
        if bad == 0 {
            println!(
                "[gpu] IR targets {}: rendered, sampled back, as expected",
                name
            );
        } else {
            ok = false;
        }
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        d.gfx.clear_texture(0);
        for va in [tva, xva] {
            let _ = d.vm.unmap(va, mem.len() as u64);
        }
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    for (at, len) in [(vblock, vlen), (f1block, f1len), (f2block, f2len)] {
        d.gfx.uninstall(at, len);
    }
    for (va, len) in [(s.va, img.len()), (vbv, vb.len())] {
        let _ = d.vm.unmap(va, len as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    Ok((ok, ns))
}

/// Render to a float target, then sample it, for each float format; a hang
/// in one leaves the others to run.
pub fn targets(g: &mut Gpu) -> Res<(bool, u64)> {
    let (mut ok, mut ns) = (true, 0);
    for (tf, name) in [
        (proto::TF_RGBA16F, "RGBA16F"),
        (proto::TF_RGBA32F, "RGBA32F"),
        (proto::TF_R32F, "R32F"),
    ] {
        let (o, t) = targets_one(g, tf, name)?;
        ok &= o;
        ns += t;
    }
    Ok((ok, ns))
}

/// The data path of ray-traced shadows in one batch: a pass draws a
/// gradient into an RGBA32F target; after a barrier a compute kernel samples
/// the target (TEX at an explicit lod in a compute shader) into a buffer;
/// after a barrier a draw loads that buffer in its fragment shader and
/// writes the pixels.
pub fn path(g: &mut Gpu) -> Res<(bool, u64)> {
    const N: u32 = 16;
    const PITCH: u32 = 256;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let tf = proto::TF_RGBA32F;

    let vs = nxk_shader::stock::fullscreen_vertex();
    // Pass 1: (4u, 4v, 2, 0.5).
    let mut b = Builder::new(Stage::Fragment);
    let four = b.f32(4.0);
    let (u, v) = (
        b.input_as(0, 0, Interp::Perspective),
        b.input_as(0, 1, Interp::Perspective),
    );
    let (r, gr) = (b.fmul(u, four), b.fmul(v, four));
    let (two, half) = (b.f32(2.0), b.f32(0.5));
    for (c, x) in [r, gr, two, half].into_iter().enumerate() {
        b.output(OutKind::Color(0), c as u8, x);
    }
    let fs1 = b.finish();
    // Compute: thread i samples the texel (i % 16, i / 16) of texture 0 at
    // lod 0 and stores its four floats at words 4i.
    let mut b = Builder::new(Stage::Compute);
    b.local_size(64, 1, 1);
    let tid = b.builtin(nxk_shader::Builtin::LocalIdX);
    let gid = b.builtin(nxk_shader::Builtin::GroupIdX);
    let six = b.u32(6);
    let g64 = b.shl(gid, six);
    let id = b.uadd(g64, tid);
    let (mask, four_u) = (b.u32(15), b.u32(4));
    let x = b.and(id, mask);
    let y = b.shr(id, four_u);
    let (xf, yf) = (b.u2f(x), b.u2f(y));
    let (step, centre) = (b.f32(1.0 / N as f32), b.f32(0.5 / N as f32));
    let u = b.fma(xf, step, centre);
    let v = b.fma(yf, step, centre);
    let lod = b.f32(0.0);
    let t = b.sample_lod(0, u, v, lod);
    let bits = [b.fbits(t[0]), b.fbits(t[1]), b.fbits(t[2]), b.fbits(t[3])];
    let two_u = b.u32(2);
    let at = b.shl(id, two_u);
    b.store(0, at, 0, &bits);
    let kernel = b.finish();
    // Pass 3: the buffer's four floats at the pixel's index, times 1/4
    // (alpha as is).
    let mut b = Builder::new(Stage::Fragment);
    let (u, v) = (
        b.input_as(0, 0, Interp::Perspective),
        b.input_as(0, 1, Interp::Perspective),
    );
    let n = b.f32(N as f32);
    let (un, vn) = (b.fmul(u, n), b.fmul(v, n));
    let (ix, iy) = (b.f2u(un), b.f2u(vn));
    let (four_u, two_u) = (b.u32(4), b.u32(2));
    let row = b.shl(iy, four_u);
    let idx = b.uadd(row, ix);
    let at = b.shl(idx, two_u);
    let t = b.load_n(0, at, 0, &[Ty::F32; 4]);
    let quarter = b.f32(0.25);
    for c in 0..3u8 {
        let x = b.fmul(t[c as usize], quarter);
        b.output(OutKind::Color(0), c, x);
    }
    b.output(OutKind::Color(0), 3, t[3]);
    let fs3 = b.finish();
    let (cv, c1, ck, c3) = match (
        lower::compile(&vs),
        lower::compile(&fs1),
        lower::compile(&kernel),
        lower::compile(&fs3),
    ) {
        (Ok(v), Ok(a), Ok(k), Ok(c)) => (v, a, k, c),
        (v, a, k, c) => {
            println!(
                "[gpu] IR path: does not compile: {:?} {:?} {:?} {:?}",
                v.err(),
                a.err(),
                k.err(),
                c.err()
            );
            return Ok((false, 0));
        }
    };

    let d = g.dev(0)?;
    let img = Dma::new((N * PITCH) as usize).map_err(e)?;
    let vb = Dma::new(4096).map_err(e)?;
    let outb = Dma::new((N * N * 16) as usize).map_err(e)?;
    let s = crate::engine::Surface {
        va: d.vm.map(&img, false).map_err(e)?,
        width: N,
        height: N,
        pitch: PITCH,
        format: proto::FMT_A8B8G8R8 as u32,
    };
    let vbv = d.vm.map_cached(&vb).map_err(e)?;
    let outv = d.vm.map_cached(&outb).map_err(e)?;
    for (i, x) in [-1.0f32, -1.0, 3.0, -1.0, -1.0, 3.0].iter().enumerate() {
        vb.w32(i * 4, x.to_bits());
    }
    outb.words(0, &alloc::vec![0xAAAA_AAAA; (N * N * 4) as usize]);
    let Some((vh, vblock, vlen)) = d.gfx.install(&cv.header, &cv.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((f1, f1block, f1len)) = d.gfx.install(&c1.header, &c1.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((f3, f3block, f3len)) = d.gfx.install(&c3.header, &c3.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((kat, klen)) = d.compute.install(&ck.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut attrs = Vec::new();
    attrs.push(attr_word(0, proto::AF_F32X2).unwrap());
    attrs.resize(proto::MAX_ATTRIBUTES, gfx::ATTR_INACTIVE);
    let pipe = |fs: u32, gprs: u32| Custom {
        vs: vh,
        vs_gprs: cv.gprs.next_multiple_of(8).max(8),
        fs,
        fs_gprs: gprs.next_multiple_of(8).max(8),
        stride: 8,
        attrs: attrs.clone(),
        inst_stride: 0,
        flags: proto::PF_NO_DEPTH_TEST | proto::PF_NO_DEPTH_WRITE,
        blend: proto::BLEND_NONE,
        depth: proto::DEPTH_LESS,
    };
    let (p1, p3) = (pipe(f1, c1.gprs), pipe(f3, c3.gprs));
    let lay = Layout {
        stride: 8,
        attrs: &p1.attrs,
        inst_stride: 0,
    };

    let bpp = proto::tf_bytes(tf);
    let bl = proto::tex_block_log2(N);
    let fmt = gfx::color_format(tf).ok_or(-libnx::abi::err::EINVAL)?;
    let mem = Dma::new(proto::tiled_bytes_bpp(N, N, bpp, bl).next_multiple_of(4096)).map_err(e)?;
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    let kind = mmu::KIND_GENERIC_16BX2;
    let (tva, xva) = (
        d.vm.map_kind(&mem, false, kind, 1 << 17).map_err(e)?,
        d.vm.map_kind(&mem, false, kind, 1 << 17).map_err(e)?,
    );
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let first = gfx::Target {
        color: gfx::Color::Tiled(gfx::Tiled {
            va: tva,
            width: N,
            height: N,
            format: fmt,
            bpp,
            block_log2: bl,
        }),
        depth: None,
    };
    let third = gfx::Target {
        color: gfx::Color::Linear(s),
        depth: None,
    };
    d.gfx.set_texture(
        0,
        &gfx::TexImage {
            va: xva,
            width: N,
            height: N,
            format: tf,
            block_log2: bl,
            levels: 1,
        },
        proto::TEX_CLAMP,
    );
    img.words(0, &alloc::vec![0xDEAD_BEEF; (N * PITCH / 4) as usize]);

    // Buffer slot 0 is the kernel's answers, then the fragment shader's
    // input.
    let mut desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
    desc[0] = outv as u32;
    desc[1] = (outv >> 32) as u32;
    desc[2] = N * N * 4;
    let mut handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
    handles[0] = gfx::Gfx::texture_handle(0);
    let mut p = Push::default();
    d.gfx.begin(&mut p);
    let nospc = || -libnx::abi::err::ENOSPC;
    d.gfx.pipeline_custom(&mut p, &first, &p1);
    let staged = d.gfx.uniforms(&mut p, &[0])
        && d.gfx.cbuf(&mut p, 2, &desc)
        && d.gfx.cbuf(&mut p, 3, &handles);
    if !staged {
        return Err(nospc());
    }
    d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
    d.gfx.barrier(&mut p);
    let cbs = [
        d.gfx.stage(&[0; 64]).ok_or_else(nospc)?,
        d.gfx.stage(&[N * N]).ok_or_else(nospc)?,
        d.gfx.stage(&desc).ok_or_else(nospc)?,
        d.gfx.stage(&handles).ok_or_else(nospc)?,
    ];
    let (qat, qva) = d.gfx.alloc_ubo(crate::qmd::SIZE).ok_or_else(nospc)?;
    let q = Qmd::new3(&Launch3 {
        program: kat,
        regs: ck.gprs.next_multiple_of(8).max(8),
        grid: [(N * N).div_ceil(64), 1, 1],
        block: [64, 1, 1],
        cbs: &cbs,
        local_bytes: ck.local_bytes,
    });
    d.gfx.ubo_write(qat, &q.bytes());
    d.compute.launch(&mut p, qva);
    d.gfx.barrier(&mut p);
    d.gfx.pipeline_custom(&mut p, &third, &p3);
    let staged = d.gfx.uniforms(&mut p, &[0])
        && d.gfx.cbuf(&mut p, 2, &desc)
        && d.gfx.cbuf(&mut p, 3, &handles);
    if !staged {
        return Err(nospc());
    }
    d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
    let t = g.run(p, BATCH_TIMEOUT_US);
    let ns = match t {
        Ok(t) => t,
        Err(_) => {
            println!("[gpu] IR path: the batch hung");
            return Ok((false, 0));
        }
    };

    let near = |a: u32, b: u32| {
        (0..4).all(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF) <= 3)
    };
    let word = |r: u32, g: u32, b: u32, a: u32| a << 24 | b << 16 | g << 8 | r;
    let mut bad = 0;
    // The kernel's own words first.
    for (x, y) in [(0u32, 0u32), (3, 2), (15, 15)] {
        let i = (y * N + x) as usize;
        let t: [f32; 4] = core::array::from_fn(|k| f32::from_bits(outb.r32((i * 4 + k) * 4)));
        let (u, v) = ((x as f32 + 0.5) / N as f32, (y as f32 + 0.5) / N as f32);
        let want = [4.0 * u, 4.0 * v, 2.0, 0.5];
        if (0..4).any(|k| fmath::abs(t[k] - want[k]) > 0.02) {
            println!(
                "[gpu] IR path: kernel texel ({}, {}) {:?}, expected {:?}",
                x, y, t, want
            );
            bad += 1;
        }
    }
    for (x, y) in [(0u32, 0u32), (3, 2), (8, 9), (15, 15), (5, 12)] {
        let (u, v) = ((x as f32 + 0.5) / N as f32, (y as f32 + 0.5) / N as f32);
        let want = word((u * 255.0 + 0.5) as u32, (v * 255.0 + 0.5) as u32, 128, 128);
        let got = img.r32((y * PITCH + x * 4) as usize);
        if !near(got, want) {
            println!(
                "[gpu] IR path: pixel ({}, {}) {:#010x}, expected {:#010x}",
                x, y, got, want
            );
            bad += 1;
        }
    }
    if bad == 0 {
        println!("[gpu] IR path: target, compute sample, fragment load, as expected");
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    d.gfx.clear_texture(0);
    d.compute.uninstall(kat, klen);
    for (at, len) in [(vblock, vlen), (f1block, f1len), (f3block, f3len)] {
        d.gfx.uninstall(at, len);
    }
    for va in [tva, xva] {
        let _ = d.vm.unmap(va, mem.len() as u64);
    }
    for (va, len) in [(s.va, img.len()), (vbv, vb.len()), (outv, outb.len())] {
        let _ = d.vm.unmap(va, len as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    Ok((bad == 0, ns))
}

/// The ray-traced shadows' kernel on the GPU against the reference
/// machine: a valley with boxes seen by a camera (the pre-pass's positions
/// worked out on the CPU, sampled from an RGBA32F texture), frames without
/// and with the sun's jitter, then with history from a camera that moved.
pub fn shadows(g: &mut Gpu) -> Res<(bool, u64)> {
    use nxk_shader::rt::{shadow, shadow_demo as sd};
    const W: usize = 80;
    const H: usize = 60;
    let c = match lower::compile(&shadow::kernel(shadow::Slots::default())) {
        Ok(c) => c,
        Err(err) => {
            println!("[gpu] IR shadows: does not compile: {:?}", err);
            return Ok((false, 0));
        }
    };
    let sc = sd::scene();
    let (eye, eye2, pitch) = ([0.0, 12.0, -34.0], [0.6, 12.2, -33.4], 0.3);
    let (mut first, _) = sd::positions(&sc, W, H, eye, pitch);
    let (mut second, _) = sd::positions(&sc, W, H, eye2, pitch);
    let frame = |eye, number, jitter, reset| sd::Frame {
        eye,
        prev: ([0.0, 12.0, -34.0], 0.3),
        number,
        jitter,
        reset,
    };
    let zeros = alloc::vec![0u32; 2 * W * H];
    let (mut ok, mut ns) = (true, 0);
    // The reference's masks of the first two frames are the history of
    // the last two.
    let mut kept: Vec<Vec<u32>> = Vec::new();
    for (name, f, later, hist) in [
        ("no jitter", frame(eye, 0, 0.0, true), false, None),
        ("jitter", frame(eye, 5, 0.026, true), false, None),
        ("history, no jitter", frame(eye2, 7, 0.0, false), true, Some(0)),
        ("history, jitter", frame(eye2, 6, 0.026, false), true, Some(1)),
    ] {
        let pos = if later { &mut second } else { &mut first };
        let hist = hist.map_or(&zeros, |k: usize| &kept[k]).clone();
        let un = sd::uniforms(&sc, W, H, &f);
        let want = sd::reference(&sc, pos, &hist, &un);
        let Some((got, t)) = shadow_frame(g, &c, pos, &hist, &un, &sc.bvh)? else {
            return Ok((false, ns));
        };
        ns += t;
        let mut wrong = Vec::new();
        for i in 0..W * H {
            let (gd, gc) = (f32::from_bits(got[2 * i]), f32::from_bits(got[2 * i + 1]));
            let (wd, wc) = (f32::from_bits(want[2 * i]), f32::from_bits(want[2 * i + 1]));
            let off = fmath::abs(gd - wd) > 1e-3 * wd.max(1.0) || !(fmath::abs(gc - wc) <= 0.05);
            if off {
                wrong.push(i);
            }
        }
        // Whole groups of 8 x 4 texels gone wrong together.
        let (gw, gh) = (shadow::GROUP[0] as usize, shadow::GROUP[1] as usize);
        let mut groups = 0;
        for gy in 0..H / gh {
            for gx in 0..W / gw {
                let at = |k: usize| (gy * gh + k / gw) * W + gx * gw + k % gw;
                let bad = |k: usize| wrong.binary_search(&at(k)).is_ok();
                let all = (0..gw * gh).all(|k| bad(k) || pos.texels[at(k)][3] <= 0.0);
                groups += (all && (0..gw * gh).any(bad)) as usize;
            }
        }
        // A ray grazing an edge may fall either way: a few in a hundred.
        let pass = wrong.len() * 50 <= W * H && groups == 0;
        println!(
            "[gpu] IR shadows ({}): {} of {} texels differ, {} whole groups; {} us ({} registers)",
            name,
            wrong.len(),
            W * H,
            groups,
            t / 1000,
            c.gprs
        );
        for &i in wrong.iter().take(4) {
            println!(
                "[gpu]   texel ({}, {}): {:?} / {:?}, expected {:?} / {:?}",
                i % W,
                i / W,
                f32::from_bits(got[2 * i]),
                f32::from_bits(got[2 * i + 1]),
                f32::from_bits(want[2 * i]),
                f32::from_bits(want[2 * i + 1])
            );
        }
        ok &= pass;
        kept.push(want);
    }
    Ok((ok, ns))
}

/// One launch of the shadow kernel: its mask and the time it took, or
/// None if the GPU hung.
fn shadow_frame(
    g: &mut Gpu,
    c: &lower::Compiled,
    pos: &nxk_shader::rt::shadow_demo::Positions,
    hist: &[u32],
    un: &[u32],
    bvh: &nxk_shader::rt::Bvh,
) -> Res<Option<(Vec<u32>, u64)>> {
    use nxk_shader::rt::shadow;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let (w, h) = (pos.w as u32, pos.h as u32);
    let mk = |words: usize| Dma::new((words.max(1) * 4).next_multiple_of(4096));
    let out_m = mk(hist.len()).map_err(e)?;
    let hist_m = mk(hist.len()).map_err(e)?;
    let nodes_m = mk(bvh.nodes.len()).map_err(e)?;
    let tris_m = mk(bvh.tris.len()).map_err(e)?;
    out_m.words(0, &alloc::vec![GARBAGE; hist.len()]);
    hist_m.words(0, hist);
    nodes_m.words(0, &bvh.nodes);
    tris_m.words(0, &bvh.tris);
    // The positions as a block-linear RGBA32F image.
    let (tf, bpp) = (proto::TF_RGBA32F, 16);
    let bl = proto::tex_block_log2(h);
    let tex = Dma::new(proto::tiled_bytes_bpp(w, h, bpp, bl).next_multiple_of(4096)).map_err(e)?;
    for y in 0..h {
        for x in 0..w {
            let t = pos.texels[(y * w + x) as usize];
            let at = proto::tiled_offset(w, x, y, bpp, bl);
            tex.words(at, &t.map(f32::to_bits));
        }
    }
    let d = g.dev(0)?;
    let mems = [&out_m, &hist_m, &nodes_m, &tris_m];
    let lens = [hist.len(), hist.len(), bvh.nodes.len(), bvh.tris.len()];
    let mut vas = [0u64; 4];
    for (k, m) in mems.iter().enumerate() {
        vas[k] = d.vm.map_cached(m).map_err(e)?;
    }
    let tva = d
        .vm
        .map_kind(&tex, false, mmu::KIND_GENERIC_16BX2, 1 << 17)
        .map_err(e)?;
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let mut desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
    for k in 0..4 {
        desc[4 * k] = vas[k] as u32;
        desc[4 * k + 1] = (vas[k] >> 32) as u32;
        desc[4 * k + 2] = lens[k] as u32;
    }
    let mut handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
    handles[0] = gfx::Gfx::texture_handle(0);
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    d.gfx.set_texture(
        0,
        &gfx::TexImage {
            va: tva,
            width: w,
            height: h,
            format: tf,
            block_log2: bl,
            levels: 1,
        },
        proto::TEX_CLAMP | proto::TEX_NEAREST,
    );
    let Some((at, len)) = d.compute.install(&c.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut p = Push::default();
    d.gfx.begin(&mut p);
    let nospc = || -libnx::abi::err::ENOSPC;
    let cbs = [
        d.gfx.stage(&[0; 64]).ok_or_else(nospc)?,
        d.gfx.stage(un).ok_or_else(nospc)?,
        d.gfx.stage(&desc).ok_or_else(nospc)?,
        d.gfx.stage(&handles).ok_or_else(nospc)?,
    ];
    let (qat, qva) = d.gfx.alloc_ubo(crate::qmd::SIZE).ok_or_else(nospc)?;
    let q = Qmd::new3(&Launch3 {
        program: at,
        regs: c.gprs.next_multiple_of(8).max(8),
        grid: [w.div_ceil(shadow::GROUP[0]), h.div_ceil(shadow::GROUP[1]), 1],
        block: [shadow::GROUP[0], shadow::GROUP[1], 1],
        cbs: &cbs,
        local_bytes: c.local_bytes,
    });
    d.gfx.ubo_write(qat, &q.bytes());
    d.gfx.barrier(&mut p);
    d.compute.launch(&mut p, qva);
    d.gfx.barrier(&mut p);
    let t = g.run(p, BATCH_TIMEOUT_US);
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    d.compute.uninstall(at, len);
    d.gfx.clear_texture(0);
    for (k, m) in mems.iter().enumerate() {
        let _ = d.vm.unmap(vas[k], m.len() as u64);
    }
    let _ = d.vm.unmap(tva, tex.len() as u64);
    let _ = mmu::invalidate(&regs, &d.vm);
    let t = match t {
        Ok(t) => t,
        Err(err) => {
            println!("[gpu] IR shadows: the GPU hung ({})", err);
            return Ok(None);
        }
    };
    let got = (0..hist.len()).map(|i| out_m.r32(i * 4)).collect();
    Ok(Some((got, t)))
}

/// Closest-hit and any-hit traversal of a wide BVH on the GPU: 65536
/// rays through 4000 triangles against the CPU walk of the same tree,
/// with the rate it manages: random rays, then a camera's and the
/// shadow rays from what it sees (neighbours in a warp are neighbours on
/// the screen).
pub fn rays(g: &mut Gpu) -> Res<(bool, u64)> {
    use nxk_shader::rt::{self, demo, kernel};
    let tris = demo::soup(4000, 3.0, 1);
    let bvh = match rt::build(&tris) {
        Ok(b) => b,
        Err(err) => {
            println!("[gpu] IR rays: the tree does not build: {:?}", err);
            return Ok((false, 0));
        }
    };
    let slots = kernel::Slots::default();
    let compiled = match (
        lower::compile(&kernel::closest_hit(slots)),
        lower::compile(&kernel::any_hit(slots)),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            println!(
                "[gpu] IR rays: does not compile: {:?} {:?}",
                a.err(),
                b.err()
            );
            return Ok((false, 0));
        }
    };
    let sun = [0.3, 0.8, 0.5];
    let l = fmath::sqrt(sun[0] * sun[0] + sun[1] * sun[1] + sun[2] * sun[2]);
    let sun = [sun[0] / l, sun[1] / l, sun[2] / l];
    let (mut ok, mut ns) = (true, 0);
    // The same rays in a fixed random order tell coherence from ease.
    let shuffled = |v: &[rt::Ray]| {
        let mut v = v.to_vec();
        let mut r = Rng(7);
        for i in (1..v.len()).rev() {
            v.swap(i, r.next_u32() as usize % (i + 1));
        }
        v
    };
    let camera = demo::camera_rays(256, 256);
    let shadow = demo::shadow_rays(&bvh, &camera, sun);
    let mut sets = alloc::vec![
        ("soup random", demo::rays(65_536, 2)),
        ("soup shuffled camera", shuffled(&camera)),
        ("soup shuffled shadow", shuffled(&shadow)),
        ("soup camera", camera),
        ("soup shadow", shadow),
    ];
    for (set, rs) in sets.drain(..) {
        let (o, t) = rays_set(g, set, &rs, &bvh, &compiled)?;
        ok &= o;
        ns += t;
    }
    // A height field of the size of Lumen's valley (about 41,000 triangles).
    let valley = demo::valley(128, 1000, 5);
    let vbvh = match rt::build(&valley) {
        Ok(b) => b,
        Err(err) => {
            println!("[gpu] IR rays: the valley does not build: {:?}", err);
            return Ok((false, ns));
        }
    };
    let vcam = demo::camera_rays_from(256, 256, [0.0, 14.0, -62.0], 0.25);
    let vshadow = demo::shadow_rays(&vbvh, &vcam, sun);
    for (set, rs) in [
        ("valley camera", vcam.clone()),
        ("valley shadow", vshadow.clone()),
        ("valley shuffled shadow", shuffled(&vshadow)),
    ] {
        let (o, t) = rays_set(g, set, &rs, &vbvh, &compiled)?;
        ok &= o;
        ns += t;
    }
    if let Err(e) = crate::probe::run(g) {
        println!("[gpu] probe: errno {}", -e);
    }
    Ok((ok, ns))
}

fn rays_set(
    g: &mut Gpu,
    set: &str,
    rs: &[nxk_shader::rt::Ray],
    bvh: &nxk_shader::rt::Bvh,
    kernels: &(lower::Compiled, lower::Compiled),
) -> Res<(bool, u64)> {
    use nxk_shader::rt::{self, kernel, trace};
    let n_rays = rs.len();
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let ray_words: Vec<u32> = rs.iter().flat_map(|r| r.words()).collect();

    let d = g.dev(0)?;
    let mk = |words: usize| Dma::new(words.max(1) * 4);
    let rays_m = mk(ray_words.len()).map_err(e)?;
    let hits_m = mk(n_rays * rt::HIT_WORDS).map_err(e)?;
    let nodes_m = mk(bvh.nodes.len()).map_err(e)?;
    let tris_m = mk(bvh.tris.len()).map_err(e)?;
    rays_m.words(0, &ray_words);
    nodes_m.words(0, &bvh.nodes);
    tris_m.words(0, &bvh.tris);
    let mems = [&rays_m, &hits_m, &nodes_m, &tris_m];
    let lens = [
        ray_words.len(),
        n_rays * rt::HIT_WORDS,
        bvh.nodes.len(),
        bvh.tris.len(),
    ];
    let mut vas = [0u64; 4];
    for (k, m) in mems.iter().enumerate() {
        vas[k] = d.vm.map_cached(m).map_err(e)?;
    }
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let mut desc = alloc::vec![0u32; proto::SHADER_BUFFERS * 4];
    for k in 0..4 {
        desc[4 * k] = vas[k] as u32;
        desc[4 * k + 1] = (vas[k] >> 32) as u32;
        desc[4 * k + 2] = lens[k] as u32;
    }
    let handles = alloc::vec![0u32; proto::SHADER_TEXTURES];
    let (mut ok, mut ns_total) = (true, 0u64);
    for (name, c, words) in [
        ("closest", &kernels.0, rt::HIT_WORDS),
        ("any", &kernels.1, 1),
    ] {
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        let Some((at, len)) = d.compute.install(&c.code) else {
            return Err(-libnx::abi::err::ENOSPC);
        };
        for i in 0..n_rays * words {
            hits_m.w32(i * 4, GARBAGE);
        }
        let mut p = Push::default();
        d.gfx.begin(&mut p);
        let stage = |gfx: &mut gfx::Gfx, w: &[u32]| gfx.stage(w).ok_or(-libnx::abi::err::ENOSPC);
        let cbs = [
            stage(&mut d.gfx, &[0; 64])?,
            stage(&mut d.gfx, &[n_rays as u32])?,
            stage(&mut d.gfx, &desc)?,
            stage(&mut d.gfx, &handles)?,
        ];
        let (qat, qva) = d
            .gfx
            .alloc_ubo(crate::qmd::SIZE)
            .ok_or(-libnx::abi::err::ENOSPC)?;
        let q = Qmd::new3(&Launch3 {
            program: at,
            regs: c.gprs.next_multiple_of(8).max(8),
            grid: [(n_rays as u32).div_ceil(kernel::GROUP), 1, 1],
            block: [kernel::GROUP, 1, 1],
            cbs: &cbs,
            local_bytes: c.local_bytes,
        });
        d.gfx.ubo_write(qat, &q.bytes());
        d.gfx.barrier(&mut p);
        d.compute.launch(&mut p, qva);
        d.gfx.barrier(&mut p);
        let t = g.run(p, BATCH_TIMEOUT_US);
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        d.compute.uninstall(at, len);
        let t = match t {
            Ok(t) => t,
            Err(err) => {
                println!("[gpu] IR rays ({}): the GPU hung ({})", name, err);
                return Ok((false, ns_total));
            }
        };
        ns_total += t;
        // Against the CPU walk of the same tree.
        let mut wrong = 0usize;
        let mut hits = 0usize;
        for (i, r) in rs.iter().enumerate() {
            if name == "any" {
                let want = trace::occluded(bvh, r);
                let got = hits_m.r32(i * 4) != 0;
                hits += got as usize;
                wrong += (want != got) as usize;
            } else {
                let want = trace::trace(bvh, r);
                let w: [u32; 4] = core::array::from_fn(|k| hits_m.r32((i * 4 + k) * 4));
                let got = (w[3] != rt::MISS).then(|| (f32::from_bits(w[0]), w[3]));
                hits += got.is_some() as usize;
                let same = match (want, got) {
                    (None, None) => true,
                    (Some(a), Some((t, id))) => {
                        fmath::abs(a.t - t) <= 1e-3 * fmath::abs(a.t).max(1.0)
                            || a.id == id && fmath::abs(a.t - t) <= 1e-2 * fmath::abs(a.t).max(1.0)
                    }
                    _ => false,
                };
                wrong += !same as usize;
            }
        }
        // Rays grazing an edge may fall either way (the hardware's
        // reciprocal is approximate): a few in a thousand.
        let pass = wrong * 500 <= n_rays;
        println!(
            "[gpu] IR rays {} ({}): {} of {} rays differ from the CPU, {} hit; {} us, {}.{:02} Mrays/s ({} registers)",
            set,
            name,
            wrong,
            n_rays,
            hits,
            t / 1000,
            n_rays as u64 * 100 / (t / 1000).max(1) / 100,
            n_rays as u64 * 100 / (t / 1000).max(1) % 100,
            c.gprs
        );
        ok &= pass;
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    for (k, m) in mems.iter().enumerate() {
        let _ = d.vm.unmap(vas[k], m.len() as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    Ok((ok, ns_total))
}

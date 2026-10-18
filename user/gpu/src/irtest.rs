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
use nxk_shader::{fmath, Builder, CmpOp, Interp, OutKind, Stage, Ty, MAX_ATTRS, MAX_BUFFERS};

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

fn same(a: u32, b: u32) -> bool {
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
    fmath::abs(x - y) <= 2e-3 * fmath::abs(x).max(fmath::abs(y)).max(1e-3)
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
        d.vm.map(&inb, false).map_err(e)?,
        d.vm.map(&out, false).map_err(e)?,
        d.vm.map(&atom, false).map_err(e)?,
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

        // Reference, thread by thread.
        let mut bufs: Vec<Vec<u32>> = alloc::vec![Vec::new(); MAX_BUFFERS];
        bufs[0] = b0;
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
        });
        d.gfx.ubo_write(qat, &q.bytes());
        d.gfx.barrier(&mut p);
        d.compute.launch(&mut p, qva);
        d.gfx.barrier(&mut p);
        let t = g.run(p, BATCH_TIMEOUT_US);
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        d.compute.uninstall(at, len);
        ns += t?;

        let wrong: Vec<usize> = (0..OUT_WORDS)
            .filter(|&i| !same(out.r32(i * 4), bufs[1][i]))
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
                bufs[1][i],
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

/// A vertex and a fragment shader of the IR: a triangle whose color is
/// the vertex color times a uniform times a bound texture's texel.
pub fn draw(g: &mut Gpu) -> Res<(bool, u64)> {
    const W: u32 = 64;
    const H: u32 = 64;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;

    // Vertex: position and color through, color times the uniform's xyz.
    let mut b = Builder::new(Stage::Vertex);
    for c in 0..4u8 {
        let p = b.input(0, c);
        b.output(OutKind::Position, c, p);
        let col = b.input(1, c);
        b.output(OutKind::Varying(0), c, col);
    }
    let vs = b.finish();
    // Fragment: varying * uniform * texel (at the center of the texture);
    // the pixel is dropped when the red uniform is negative.
    let mut b = Builder::new(Stage::Fragment);
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
    let fs = b.finish();
    let (cv, cf) = match (lower::compile(&vs), lower::compile(&fs)) {
        (Ok(v), Ok(f)) => (v, f),
        (v, f) => {
            println!(
                "[gpu] IR draw: does not compile: vertex {:?}, fragment {:?}",
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
    let vbv = d.vm.map(&vb, false).map_err(e)?;
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
    };
    d.gfx.set_texture(0, &img8, 0);
    let Some((vh, vblock, vlen)) = d.gfx.install(&cv.header, &cv.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let Some((fh, fblock, flen)) = d.gfx.install(&cf.header, &cf.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut attrs = Vec::new();
    attrs.push(attr_word(0, proto::AF_F32X4).unwrap());
    attrs.push(attr_word(16, proto::AF_F32X4).unwrap());
    attrs.resize(proto::MAX_ATTRIBUTES, gfx::ATTR_INACTIVE);
    let custom = Custom {
        vs: vh,
        vs_gprs: cv.gprs.next_multiple_of(8).max(8),
        fs: fh,
        fs_gprs: cf.gprs.next_multiple_of(8).max(8),
        stride: 32,
        attrs,
        flags: 0,
        blend: proto::BLEND_NONE,
        depth: proto::DEPTH_LESS,
    };
    // One big triangle with a flat color.
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
    let px = |x: u32, y: u32| img.r32(((y * W + x) * 4) as usize);
    let near = |a: u32, b: u32, tol: u32| {
        (0..4).all(|i| ((a >> (8 * i)) & 0xFF).abs_diff((b >> (8 * i)) & 0xFF) <= tol)
    };
    let mut ok = true;
    let mut ns = 0;
    // (uniform red, expected pixel) in memory order R, G, B, A.
    let clear = [0.1f32, 0.2, 0.3, 1.0];
    let clear_px = 0xFF00_0000 | 77 << 16 | 51 << 8 | 26;
    for (u, want, name) in [
        (
            [0.5f32, 1.0, 0.25, 1.0],
            0xFF00_0000 | 10 << 16 | 51 << 8 | 102,
            "drawn",
        ),
        ([-1.0, 1.0, 0.25, 1.0], clear_px, "discarded"),
    ] {
        img.words(0, &alloc::vec![0xDEAD_BEEF; (W * H) as usize]);
        let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
        let mut p = Push::default();
        d.gfx.begin(&mut p);
        d.gfx.clear(&mut p, &target, clear);
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
        };
        d.gfx.draw_layout(&mut p, &lay, vbv, 3, 0, 3);
        ns += g.run(p, BATCH_TIMEOUT_US)?;
        let got = px(W / 2, H / 2);
        println!(
            "[gpu] IR draw ({}): centre {:#010x}, corner {:#010x}, expected {:#010x}",
            name,
            got,
            px(1, 1),
            want
        );
        if !near(got, want, 2) {
            ok = false;
        }
    }
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    d.gfx.clear_texture(0);
    d.gfx.uninstall(vblock, vlen);
    d.gfx.uninstall(fblock, flen);
    let _ = d.vm.unmap(s.va, img.len() as u64);
    let _ = d.vm.unmap(vbv, vb.len() as u64);
    let _ = d.vm.unmap(tva, tex.len() as u64);
    let _ = mmu::invalidate(&regs, &d.vm);
    Ok((ok, ns))
}

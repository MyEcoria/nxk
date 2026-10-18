//! Timing probes for the compute path: what an empty launch, a chain of
//! ALU operations and a chain of dependent loads (from cached pages, 16 KB
//! then 4 MB) cost. They show where a kernel's time goes; nothing is
//! checked, the numbers are in the log.

extern crate alloc;

use alloc::vec::Vec;

use libnx::println;
use nxk_shader::lower::{self, Compiled};
use nxk_shader::{Builder, CmpOp, Stage, Ty};

use crate::hw::Dma;
use crate::mmu;
use crate::push::Push;
use crate::qmd::{Launch3, Qmd};
use crate::{Gpu, BATCH_TIMEOUT_US};

type Res<T> = core::result::Result<T, i64>;

const GROUP: u32 = 64;

/// Uniform words: 0 = threads, 1 = a float, 2 = mask of the table.
fn empty() -> nxk_shader::Program {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(GROUP as u16, 1, 1);
    let tid = b.builtin(nxk_shader::Builtin::LocalIdX);
    let gid = b.builtin(nxk_shader::Builtin::GroupIdX);
    let six = b.u32(GROUP.trailing_zeros());
    let g64 = b.shl(gid, six);
    let id = b.uadd(g64, tid);
    b.store(0, id, 0, &[id]);
    b.finish()
}

/// 256 turns of four independent chains of FMAs.
fn alu() -> nxk_shader::Program {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(GROUP as u16, 1, 1);
    let tid = b.builtin(nxk_shader::Builtin::LocalIdX);
    let gid = b.builtin(nxk_shader::Builtin::GroupIdX);
    let six = b.u32(GROUP.trailing_zeros());
    let g64 = b.shl(gid, six);
    let id = b.uadd(g64, tid);
    let a = b.uniform(1, Ty::F32);
    let c = b.uniform(3, Ty::F32);
    let acc: Vec<_> = (0..4).map(|_| b.copy(a)).collect();
    let (i, one, n) = (b.u32(0), b.u32(1), b.u32(256));
    b.loop_(256, |b| {
        for &x in &acc {
            let y = b.fma(x, a, c);
            b.mov(x, y);
        }
        let ni = b.uadd(i, one);
        b.mov(i, ni);
        let done = b.cmp(CmpOp::Ge, i, n);
        b.break_if(done);
    });
    let s0 = b.fadd(acc[0], acc[1]);
    let s1 = b.fadd(acc[2], acc[3]);
    let s = b.fadd(s0, s1);
    let bits = b.fbits(s);
    b.store(0, id, 0, &[bits]);
    b.finish()
}

/// 32 dependent loads from a table in buffer 1: the next index is the
/// word read.
fn chase() -> nxk_shader::Program {
    let mut b = Builder::new(Stage::Compute);
    b.local_size(GROUP as u16, 1, 1);
    let tid = b.builtin(nxk_shader::Builtin::LocalIdX);
    let gid = b.builtin(nxk_shader::Builtin::GroupIdX);
    let six = b.u32(GROUP.trailing_zeros());
    let g64 = b.shl(gid, six);
    let id = b.uadd(g64, tid);
    let mask = b.uniform(2, Ty::U32);
    let mul = b.u32(2_654_435_761);
    let h = b.umul(id, mul);
    let idx = {
        let m = b.and(h, mask);
        b.copy(m)
    };
    let (i, one, n) = (b.u32(0), b.u32(1), b.u32(32));
    b.loop_(32, |b| {
        let next = b.load(1, idx, 0, Ty::U32);
        b.mov(idx, next);
        let ni = b.uadd(i, one);
        b.mov(i, ni);
        let done = b.cmp(CmpOp::Ge, i, n);
        b.break_if(done);
    });
    b.store(0, id, 0, &[idx]);
    b.finish()
}

/// One launch of `c` on `threads` threads, with buffers 0 (the answers)
/// and 1 (a table); nanoseconds.
fn launch(
    g: &mut Gpu,
    c: &Compiled,
    out: &Dma,
    table: &Dma,
    uniforms: &[u32],
    threads: u32,
) -> Res<u64> {
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let d = g.dev(0)?;
    let spare = Dma::new(4096).map_err(e)?;
    let mems = [out, table, &spare, &spare];
    let mut vas = [0u64; 4];
    for (k, m) in mems.iter().enumerate() {
        vas[k] = d.vm.map_cached(m).map_err(e)?;
    }
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let mut desc = alloc::vec![0u32; libnx::abi::proto::gpu::SHADER_BUFFERS * 4];
    for k in 0..4 {
        desc[4 * k] = vas[k] as u32;
        desc[4 * k + 1] = (vas[k] >> 32) as u32;
        desc[4 * k + 2] = (mems[k].len() / 4) as u32;
    }
    let handles = alloc::vec![0u32; libnx::abi::proto::gpu::SHADER_TEXTURES];
    let d = g.dev.as_mut().ok_or(-libnx::abi::err::ENODEV)?;
    let Some((at, len)) = d.compute.install(&c.code) else {
        return Err(-libnx::abi::err::ENOSPC);
    };
    let mut p = Push::default();
    d.gfx.begin(&mut p);
    let nospc = || -libnx::abi::err::ENOSPC;
    let cbs = [
        d.gfx.stage(&[0; 64]).ok_or_else(nospc)?,
        d.gfx.stage(uniforms).ok_or_else(nospc)?,
        d.gfx.stage(&desc).ok_or_else(nospc)?,
        d.gfx.stage(&handles).ok_or_else(nospc)?,
    ];
    let (qat, qva) = d.gfx.alloc_ubo(crate::qmd::SIZE).ok_or_else(nospc)?;
    let q = Qmd::new3(&Launch3 {
        program: at,
        regs: c.gprs.next_multiple_of(8).max(8),
        grid: [threads.div_ceil(GROUP), 1, 1],
        block: [GROUP, 1, 1],
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
    for (k, m) in mems.iter().enumerate() {
        let _ = d.vm.unmap(vas[k], m.len() as u64);
    }
    let _ = mmu::invalidate(&regs, &d.vm);
    t
}

pub fn run(g: &mut Gpu) -> Res<()> {
    let e = |(e, _): (i64, &str)| -e;
    const N: u32 = 65_536;
    let out = Dma::new(N as usize * 4).map_err(e)?;
    let small = Dma::new(4096).map_err(e)?;
    let compile = |p: nxk_shader::Program| lower::compile(&p).map_err(|_| -libnx::abi::err::EINVAL);
    let (c_empty, c_alu, c_chase) = (compile(empty())?, compile(alu())?, compile(chase())?);
    let f = 1.0001f32.to_bits();
    let c = 0.5f32.to_bits();
    for threads in [64, 4096, N] {
        // Twice: the first launch may pay for a cold start.
        let mut t = 0;
        for _ in 0..2 {
            t = launch(g, &c_empty, &out, &small, &[N, f, 0, c], threads)?;
        }
        println!("[gpu] probe empty {} threads: {} us", threads, t / 1000);
    }
    for threads in [64, N] {
        let t = launch(g, &c_alu, &out, &small, &[N, f, 0, c], threads)?;
        let flops = threads as u64 * 256 * 4 * 2;
        println!(
            "[gpu] probe alu {} threads: {} us, {} MFLOP/s",
            threads,
            t / 1000,
            flops * 1000 / t.max(1)
        );
    }
    // Tables of 16 KB and 4 MB (past the L2): a pseudo-random cycle.
    for words in [4096u32, 1 << 20] {
        let table = Dma::new(words as usize * 4).map_err(e)?;
        let mut x = 12345u32;
        for i in 0..words as usize {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            table.w32(i * 4, (x >> 8) & (words - 1));
        }
        for threads in [64, N] {
            let t = launch(g, &c_chase, &out, &table, &[N, f, words - 1, c], threads)?;
            println!(
                "[gpu] probe chase {} KB, {} threads: {} us ({} ns per dependent load)",
                words / 256,
                threads,
                t / 1000,
                t / 32
            );
        }
    }
    Ok(())
}

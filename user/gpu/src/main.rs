//! gpu: the GM20B (Maxwell, 2 SMs) of the Tegra X1. Off until asked;
//! POWER or a 2D / compute request brings it up: rail and clocks, GPCPLL
//! at a fixed low clock, MMU and BAR1, NVIDIA's secure boot of FECS
//! through the PMU (firmware from linux-firmware, on the SD card), GR
//! init and golden context, one channel. Then 2D fills and copies,
//! built-in compute kernels, a self-test and an FFMA benchmark.

#![no_std]
#![no_main]

extern crate alloc;

mod acr;
mod engine;
mod falcon;
mod fifo;
mod fw;
mod gr;
mod hw;
mod kernels;
mod mmu;
mod pll;
mod power;
mod push;
mod qmd;
mod sass;

use alloc::vec::Vec;

use libnx::abi::err::{EAGAIN, EINVAL, ENODEV};
use libnx::abi::proto::{clk, gpu as proto};
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg};
use libnx::{fs, println, sys, time};

use engine::{Compute, Kernel, Surface};
use fifo::Chan;
use hw::{Dma, Regs, Result};
use mmu::Vm;
use push::Push;

const BAR0: u64 = 0x5700_0000;
const BAR1: u64 = 0x5800_0000;
/// BAR1 mapped by the CPU: the USERD page is all it touches.
const BAR1_MAPPED: u64 = 0x10_0000;
const FUSE_PAGE: u64 = 0x7000_F000;
const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;
/// Where `tools/gm20b-fw.sh` output lands on the card.
const FW_DIR: &str = "/firmware/nvidia/gm20b/";
/// Allocations start past BAR1's window.
const FIRST_VA: u64 = fifo::BAR1_SIZE;
/// Unknown chips keep the bypass clock at NVIDIA's highest Vmin.
const DEFAULT_UV: u32 = 950_000 + pll::MARGIN_UV;
const MAX_SURFACES: usize = 16;
const BATCH_TIMEOUT_US: u64 = 2_000_000;

/// Everything that exists while the GPU is on.
struct Dev {
    vm: Vm,
    _bar1_inst: Dma,
    _fb_pages: [Dma; 3],
    chan: Chan,
    _global: gr::Global,
    _ctx: gr::ChanCtx,
    compute: Compute,
    topo: gr::Topo,
    surfaces: Vec<Option<(Surface, u32)>>,
}

struct Gpu {
    regs: Regs,
    bar1: Mmio,
    i2c: I2c,
    fuse: Option<Mmio>,
    misc: Option<Mmio>,
    state: u64,
    stage: u64,
    err: i64,
    rail: u32,
    chip: power::Chip,
    clock: Option<power::Clock>,
    boot0: u32,
    dev: Option<Dev>,
}

fn read(name: &str) -> Result<Vec<u8>> {
    let mut path = alloc::string::String::from(FW_DIR);
    path.push_str(name);
    fs::read(&path).map_err(|e| {
        println!("[gpu] {}: {}", path, libnx::abi::err::name(e));
        (-e, "GPU firmware missing (tools/gm20b-fw.sh)")
    })
}

impl Gpu {
    fn status(&self) -> Msg {
        let mut m = Msg::new(0, 6, 0);
        m.mr[0] = self.state | self.stage << 8;
        m.mr[1] = self.clock.as_ref().map_or(0, |c| c.khz as u64);
        m.mr[2] = power::rail_uv(&self.i2c) as u64;
        m.mr[3] = self.boot0 as u64;
        m.mr[4] =
            self.chip.speedo as u32 as u64 | (self.chip.speedo_id.map_or(0xFF, |i| i as u64)) << 32;
        m.mr[5] = self.err as u64;
        m
    }

    /// Stage `s` begins.
    fn at(&mut self, s: u64) {
        self.stage = s;
    }

    fn power_on(&mut self) -> Result<()> {
        if self.state == proto::ON {
            return Ok(());
        }
        let t0 = time::now_ns();
        self.err = 0;
        let r = self.bring_up();
        match r {
            Ok(()) => {
                self.state = proto::ON;
                self.stage = proto::STAGE_READY;
                println!("[gpu] ready in {} ms", (time::now_ns() - t0) / 1_000_000);
            }
            Err((e, what)) => {
                drv::report(
                    "gpu",
                    &alloc::format!(
                        "power-up failed at stage {}: {} ({})",
                        self.stage,
                        what,
                        libnx::abi::err::name(-e)
                    ),
                );
                let stage = self.stage;
                self.power_down();
                self.state = proto::FAILED;
                self.stage = stage;
                self.err = -e;
            }
        }
        r
    }

    fn bring_up(&mut self) -> Result<()> {
        let r = self.regs;
        self.at(proto::STAGE_RAIL);
        // T210B01 (Mariko) has its GPU rail on a MAX77812 phase instead.
        if self
            .misc
            .is_some_and(|m| (m.r32(GP_HIDREV) >> 4) & 0xF != 1)
        {
            return Err((ENODEV, "Mariko GPU rail not supported"));
        }
        if libnx::clk::ready(clk::FUSE) {
            self.chip = power::chip(self.fuse.as_ref());
        }
        self.rail = match self.chip.speedo_id {
            Some(id) => pll::rail_uv(self.chip.speedo, id),
            None => pll::rail_step(pll::rail_code(DEFAULT_UV)),
        };
        power::rail_on(&self.i2c, self.rail)?;
        match self.chip.speedo_id {
            Some(id) => println!(
                "[gpu] rail {} mV (GPU speedo {}, ID {})",
                self.rail / 1000,
                self.chip.speedo,
                id
            ),
            None => println!(
                "[gpu] rail {} mV (not a known Erista SKU)",
                self.rail / 1000
            ),
        }

        self.at(proto::STAGE_CLOCKS);
        libnx::clk::enable(clk::GPU).map_err(|e| (-e, "GPU clocks and clamps"))?;
        self.boot0 = power::identify(&r)?;

        // Units on, priv ring reset (`nv50_mc_init`, `gk20a_privring`).
        self.at(proto::STAGE_PLL);
        r.wr(0x200, 0xFFFF_FFFF);
        r.rd(0x200);
        r.mask(0x137250, 0x3F, 0);
        r.mask(0x200, 0x20, 0);
        time::sleep_us(20);
        r.mask(0x200, 0, 0x20);
        r.wr(0x12004C, 0x4);
        r.wr(0x122204, 0x2);
        r.rd(0x122204);
        r.wr(0x122354, 0x800);
        r.wr(0x128328, 0x800);
        r.wr(0x124320, 0x800);
        self.clock = power::clock(&r, &self.chip, self.rail)?;

        self.at(proto::STAGE_MMU);
        let mut vm = Vm::new(FIRST_VA)?;
        // FB: system memory flush page, MMU debug buffers, big page size
        // per instance (`gf100_fb_sysmem_flush_page_init`, `gm200_fb_init`).
        let flush = Dma::new(4096)?;
        let dbg_wr = Dma::new(128 << 10)?;
        let dbg_rd = Dma::new(128 << 10)?;
        r.wr(0x100C10, (flush.pa >> 8) as u32);
        r.wr(0x100CC8, (dbg_wr.pa >> 8) as u32);
        r.wr(0x100CCC, (dbg_rd.pa >> 8) as u32);
        r.mask(0x100CC4, 0x0006_0000, 2 << 17);
        r.mask(0x100C80, 0x1800, 0x1800);
        // L2: no compression tags without video memory (`gm200_ltc_init`).
        r.wr(0x17E278, 0);
        // BAR1: the same address space, limited to its 16 MiB window.
        let bar1_inst = Dma::new(4096)?;
        vm.join(&bar1_inst, fifo::BAR1_SIZE);
        r.l2_invalidate()?;
        r.wr(0x1704, 0x8000_0000 | (bar1_inst.pa >> 12) as u32);
        r.wr(0x70000, 1);
        r.wait(0x70000, 2, 0, 2_000_000, "BAR flush")?;

        self.at(proto::STAGE_FIRMWARE);
        let files = acr::Files {
            load: read("acr/ucode_load.bin")?,
            bl: read("acr/bl.bin")?,
            fecs_bl: read("gr/fecs_bl.bin")?,
            fecs_inst: read("gr/fecs_inst.bin")?,
            fecs_data: read("gr/fecs_data.bin")?,
            fecs_sig: read("gr/fecs_sig.bin")?,
        };
        let gpccs_inst = read("gr/gpccs_inst.bin")?;
        let gpccs_data = read("gr/gpccs_data.bin")?;
        let nets = gr::Nets {
            nonctx: read("gr/sw_nonctx.bin")?,
            ctx: read("gr/sw_ctx.bin")?,
            bundle: read("gr/sw_bundle_init.bin")?,
            method: read("gr/sw_method_init.bin")?,
        };

        self.at(proto::STAGE_ACR);
        acr::boot(&r, &mut vm, &files)?;

        self.at(proto::STAGE_FECS);
        let topo = gr::topology(&r)?;
        println!(
            "[gpu] {} GPC, {} TPC, {} LTC",
            topo.gpc_nr,
            topo.tpc_total,
            r.rd(0x12006C)
        );
        let sizes = gr::init(&r, &topo, &gpccs_inst, &gpccs_data, &nets)?;

        self.at(proto::STAGE_CONTEXT);
        let pbdmas = fifo::init(&r);
        let runl = fifo::gr_runlist(&r);
        let mut chan = Chan::new(&mut vm, 0, runl)?;
        let global = gr::global(&mut vm, &topo)?;
        let golden = gr::golden(
            &r,
            &mut vm,
            &chan.inst,
            &topo,
            &global,
            &nets,
            sizes.image.max(4),
        )?;
        let ctx = gr::channel(&mut vm, &chan.inst, &topo, &global, &golden)?;
        mmu::invalidate(&r, &vm)?;
        println!(
            "[gpu] golden context {} bytes, {} PBDMA, runlist {}",
            golden.len(),
            pbdmas,
            runl
        );

        self.at(proto::STAGE_CHANNEL);
        let compute = Compute::new(&mut vm)?;
        mmu::invalidate(&r, &vm)?;
        chan.schedule(&r)?;
        let mut p = Push::default();
        compute.setup(&mut p);
        chan.run(&r, &self.bar1, p, BATCH_TIMEOUT_US)?;
        self.dev = Some(Dev {
            vm,
            _bar1_inst: bar1_inst,
            _fb_pages: [flush, dbg_wr, dbg_rd],
            chan,
            _global: global,
            _ctx: ctx,
            compute,
            topo,
            surfaces: Vec::new(),
        });
        Ok(())
    }

    fn power_down(&mut self) {
        let r = self.regs;
        if let Some(d) = self.dev.take() {
            for (_, h) in d.surfaces.iter().flatten() {
                let _ = sys::handle_close(*h);
            }
            d.chan.unschedule(&r);
        }
        if self.stage >= proto::STAGE_PLL {
            if let Some(c) = self.clock.take() {
                power::clock_off(&r, &c);
            }
        }
        if self.stage >= proto::STAGE_CLOCKS {
            let _ = libnx::clk::disable(clk::GPU);
        }
        power::rail_off(&self.i2c);
        self.clock = None;
        self.state = proto::OFF;
        self.stage = 0;
        println!("[gpu] off");
    }

    /// The device, powered up first unless the request says `IF_ON`.
    fn dev(&mut self, flags: u64) -> core::result::Result<&mut Dev, i64> {
        if self.state != proto::ON {
            if flags & proto::IF_ON != 0 {
                return Err(-EAGAIN);
            }
            self.power_on().map_err(|(e, _)| -e)?;
        }
        Ok(self.dev.as_mut().unwrap())
    }

    /// Run a batch and wait for it. A GPU that does not finish is reset
    /// (powered down): nothing keeps running on it.
    fn run(&mut self, p: Push, timeout_us: u64) -> core::result::Result<u64, i64> {
        let d = self.dev.as_mut().ok_or(-ENODEV)?;
        let t0 = time::now_ns();
        if let Err((e, what)) = d.chan.run(&self.regs, &self.bar1, p, timeout_us) {
            drv::report("gpu", &alloc::format!("{}: powering the GPU down", what));
            self.power_down();
            self.state = proto::FAILED;
            self.stage = proto::STAGE_READY;
            self.err = -e;
            return Err(-e);
        }
        Ok(time::now_ns() - t0)
    }
}

/// Physical pages of `len` bytes of a client's shm (DMA right).
fn pages(h: u32, len: u64) -> core::result::Result<Vec<u64>, i64> {
    if len == 0 {
        return Err(-EINVAL);
    }
    sys::shm_paddr(h, len - 1)?;
    (0..len.div_ceil(mmu::PAGE))
        .map(|k| sys::shm_paddr(h, k * mmu::PAGE))
        .collect()
}

fn attach(g: &mut Gpu, m: &Msg) -> core::result::Result<u64, i64> {
    let h = ipc::received_handle(0);
    let r = (|| {
        let (w, ht) = (m.mr[0] as u32, (m.mr[0] >> 32) as u32);
        let (pitch, format) = (m.mr[1] as u32, m.mr[2] as u32);
        if w == 0 || ht == 0 || pitch < w * 4 || pitch % 4 != 0 {
            return Err(-EINVAL);
        }
        if ![proto::FMT_A8R8G8B8, proto::FMT_A8B8G8R8].contains(&(format as u64)) {
            return Err(-EINVAL);
        }
        let regs = g.regs;
        let d = g.dev(0)?;
        let s0 = Surface {
            va: 0,
            width: w,
            height: ht,
            pitch,
            format,
        };
        let pg = pages(h, s0.bytes())?;
        let slot = match d.surfaces.iter().position(|s| s.is_none()) {
            Some(i) => i,
            None if d.surfaces.len() < MAX_SURFACES => {
                d.surfaces.push(None);
                d.surfaces.len() - 1
            }
            None => return Err(-libnx::abi::err::ENOSPC),
        };
        let va =
            d.vm.alloc(pg.len() as u64 * mmu::PAGE, 1 << 17)
                .map_err(|(e, _)| -e)?;
        d.vm.map_at(va, &pg, false).map_err(|(e, _)| -e)?;
        mmu::invalidate(&regs, &d.vm).map_err(|(e, _)| -e)?;
        d.surfaces[slot] = Some((Surface { va, ..s0 }, h));
        Ok(slot as u64)
    })();
    if r.is_err() {
        let _ = sys::handle_close(h);
    }
    r
}

fn surface(d: &Dev, id: u64) -> core::result::Result<Surface, i64> {
    d.surfaces
        .get(id as usize)
        .and_then(|s| s.as_ref())
        .map(|(s, _)| *s)
        .ok_or(-EINVAL)
}

fn inside(s: &Surface, x: u32, y: u32, w: u32, h: u32) -> bool {
    w > 0
        && h > 0
        && x.checked_add(w).is_some_and(|e| e <= s.width)
        && y.checked_add(h).is_some_and(|e| e <= s.height)
}

fn lo(v: u64) -> u32 {
    v as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

fn fill(g: &mut Gpu, m: &Msg) -> core::result::Result<u64, i64> {
    let d = g.dev(m.mr[4])?;
    let s = surface(d, m.mr[0])?;
    let (x, y, w, h) = (lo(m.mr[1]), hi(m.mr[1]), lo(m.mr[2]), hi(m.mr[2]));
    if !inside(&s, x, y, w, h) {
        return Err(-EINVAL);
    }
    let mut p = Push::default();
    engine::fill(&mut p, &s, x, y, w, h, m.mr[3] as u32);
    g.run(p, BATCH_TIMEOUT_US)
}

fn copy(g: &mut Gpu, m: &Msg) -> core::result::Result<u64, i64> {
    let d = g.dev(m.mr[4])?;
    let (dst, src) = (
        surface(d, lo(m.mr[0]) as u64)?,
        surface(d, hi(m.mr[0]) as u64)?,
    );
    let (dx, dy, sx, sy) = (lo(m.mr[1]), hi(m.mr[1]), lo(m.mr[2]), hi(m.mr[2]));
    let (w, h) = (lo(m.mr[3]), hi(m.mr[3]));
    if !inside(&dst, dx, dy, w, h) || !inside(&src, sx, sy, w, h) || dst.format != src.format {
        return Err(-EINVAL);
    }
    let mut p = Push::default();
    engine::copy(&mut p, &dst, &src, dx, dy, sx, sy, w, h);
    g.run(p, BATCH_TIMEOUT_US)
}

/// A client's buffers mapped for one launch.
fn compute(g: &mut Gpu, m: &Msg) -> core::result::Result<u64, i64> {
    let handles: Vec<u32> = (0..m.handles()).map(ipc::received_handle).collect();
    let r = (|| {
        let (k, count, value) = (m.mr[0], m.mr[1] as u32, m.mr[2] as u32);
        let (kernel, bufs) = match k {
            proto::K_FILL => (Kernel::Fill, 1),
            proto::K_COPY => (Kernel::Copy, 2),
            proto::K_SAXPY => (Kernel::Saxpy, 2),
            _ => return Err(-EINVAL),
        };
        if handles.len() != bufs || count == 0 {
            return Err(-EINVAL);
        }
        let regs = g.regs;
        let d = g.dev(m.mr[3])?;
        let mut vas = [0u64; 2];
        let mut mapped = Vec::new();
        for (i, h) in handles.iter().enumerate() {
            let pg = pages(*h, count as u64 * 4)?;
            let len = pg.len() as u64 * mmu::PAGE;
            let va = d.vm.alloc(len, mmu::PAGE).map_err(|(e, _)| -e)?;
            d.vm.map_at(va, &pg, false).map_err(|(e, _)| -e)?;
            vas[i] = va;
            mapped.push((va, len));
        }
        mmu::invalidate(&regs, &d.vm).map_err(|(e, _)| -e)?;
        let mut p = Push::default();
        d.compute.dispatch(
            &mut p,
            kernel,
            &engine::args(vas[0], vas[1], count, value, 0),
            count,
        );
        let t = g.run(p, BATCH_TIMEOUT_US);
        let d = g.dev.as_mut().ok_or(-ENODEV)?;
        for (va, len) in mapped {
            let _ = d.vm.unmap(va, len);
        }
        let _ = mmu::invalidate(&regs, &d.vm);
        t
    })();
    for h in handles {
        let _ = sys::handle_close(h);
    }
    r
}

/// Self-test on the service's own memory, checked by the CPU.
fn test(g: &mut Gpu) -> core::result::Result<(u64, u64, u64), i64> {
    const W: u32 = 256;
    const H: u32 = 64;
    const N: u32 = 4096;
    let e = |(e, _): (i64, &str)| -e;
    let regs = g.regs;
    let d = g.dev(0)?;
    let img = Dma::new((W * H * 4) as usize).map_err(e)?;
    let x = Dma::new(N as usize * 4).map_err(e)?;
    let y = Dma::new(N as usize * 4).map_err(e)?;
    let s = Surface {
        va: d.vm.map(&img, false).map_err(e)?,
        width: W,
        height: H,
        pitch: W * 4,
        format: proto::FMT_A8R8G8B8 as u32,
    };
    let (xv, yv) = (
        d.vm.map(&x, false).map_err(e)?,
        d.vm.map(&y, false).map_err(e)?,
    );
    mmu::invalidate(&regs, &d.vm).map_err(e)?;
    let px = |x: u32, y: u32| img.r32(((y * W + x) * 4) as usize);
    let (mut pass, mut ns) = (0, 0);

    // 2D: a rectangle, then a copy of it next to it.
    let mut p = Push::default();
    engine::fill(&mut p, &s, 16, 8, 100, 40, 0x1234_5678);
    ns += g.run(p, BATCH_TIMEOUT_US)?;
    if px(16, 8) == 0x1234_5678 && px(115, 47) == 0x1234_5678 && px(15, 8) == 0 && px(116, 48) == 0
    {
        pass |= proto::T_FILL2D;
    }
    let mut p = Push::default();
    engine::copy(&mut p, &s, &s, 130, 12, 16, 8, 100, 40);
    ns += g.run(p, BATCH_TIMEOUT_US)?;
    if px(130, 12) == 0x1234_5678 && px(229, 51) == 0x1234_5678 && px(230, 12) == 0 {
        pass |= proto::T_COPY2D;
    }

    // Kernels: fill, copy, then y = 2x + 1 on small integers (exact).
    let d = g.dev.as_mut().ok_or(-ENODEV)?;
    let mut p = Push::default();
    d.compute.dispatch(
        &mut p,
        Kernel::Fill,
        &engine::args(xv, 0, N, 0xA5A5_0001, 0),
        N,
    );
    ns += g.run(p, BATCH_TIMEOUT_US)?;
    if (0..N as usize).all(|i| x.r32(i * 4) == 0xA5A5_0001) {
        pass |= proto::T_KFILL;
    }
    for i in 0..N as usize {
        x.w32(i * 4, i as u32);
    }
    let d = g.dev.as_mut().ok_or(-ENODEV)?;
    let mut p = Push::default();
    d.compute
        .dispatch(&mut p, Kernel::Copy, &engine::args(xv, yv, N, 0, 0), N);
    ns += g.run(p, BATCH_TIMEOUT_US)?;
    if (0..N as usize).all(|i| y.r32(i * 4) == i as u32) {
        pass |= proto::T_KCOPY;
    }
    for i in 0..N as usize {
        x.w32(i * 4, (i as f32).to_bits());
        y.w32(i * 4, 1.0f32.to_bits());
    }
    let d = g.dev.as_mut().ok_or(-ENODEV)?;
    let mut p = Push::default();
    d.compute.dispatch(
        &mut p,
        Kernel::Saxpy,
        &engine::args(xv, yv, N, 2.0f32.to_bits(), 0),
        N,
    );
    ns += g.run(p, BATCH_TIMEOUT_US)?;
    if (0..N as usize).all(|i| f32::from_bits(y.r32(i * 4)) == 2.0 * i as f32 + 1.0) {
        pass |= proto::T_KSAXPY;
    }

    let d = g.dev.as_mut().ok_or(-ENODEV)?;
    let _ = d.vm.unmap(s.va, img.len() as u64);
    let _ = d.vm.unmap(xv, x.len() as u64);
    let _ = d.vm.unmap(yv, y.len() as u64);
    let _ = mmu::invalidate(&regs, &d.vm);
    let all = proto::T_FILL2D | proto::T_COPY2D | proto::T_KFILL | proto::T_KCOPY | proto::T_KSAXPY;
    Ok((pass, all, ns))
}

/// FFMA on every SM for about `ms`: floating-point operations and time.
fn bench(g: &mut Gpu, ms: u64) -> core::result::Result<(u64, u64), i64> {
    let d = g.dev(0)?;
    // Two resident blocks' worth of threads per SM keeps each busy.
    let threads = 2048 * d.topo.tpc_total as u32;
    let flops = |loops: u32| threads as u64 * loops as u64 * kernels::BURN_FFMA * 2;
    let launch = |g: &mut Gpu, loops: u32, timeout: u64| {
        let d = g.dev.as_mut().ok_or(-ENODEV)?;
        let mut p = Push::default();
        d.compute.dispatch(
            &mut p,
            Kernel::Burn,
            &engine::args(0, 0, threads, 0, loops),
            threads,
        );
        g.run(p, timeout)
    };
    // Calibrate on a short run, then aim at `ms`.
    let probe = 256;
    let t = launch(g, probe, BATCH_TIMEOUT_US)?.max(1);
    let loops = (probe as u64 * ms * 1_000_000 / t).clamp(1, u32::MAX as u64) as u32;
    let t = launch(g, loops, ms * 4_000 + BATCH_TIMEOUT_US)?;
    Ok((flops(loops), t))
}

fn reply(r: core::result::Result<u64, i64>) -> Msg {
    match r {
        Ok(v) => Msg::from_words(0, &[v]),
        Err(e) => Msg::error(e),
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let regs = Regs(Mmio::map(drv::dev_slot(0), BAR0, 0x100_0000).expect("gpu: BAR0"));
    let bar1 = Mmio::map(drv::dev_slot(1), BAR1, BAR1_MAPPED).expect("gpu: BAR1");
    let fuse = Mmio::map(drv::dev_slot(2), FUSE_PAGE, 0x1000).ok();
    let misc = Mmio::map(drv::dev_slot(3), APB_MISC, 0x1000).ok();
    let i2c = I2c::connect().expect("gpu: i2c");
    let mut g = Gpu {
        regs,
        bar1,
        i2c,
        fuse,
        misc,
        state: proto::OFF,
        stage: 0,
        err: 0,
        rail: 0,
        chip: power::Chip::default(),
        clock: None,
        boot0: 0,
        dev: None,
    };
    let ep = drv::publish("gpu").expect("gpu: publish");
    println!("[gpu] GPU service ready (off)");
    drv::serve(ep, |rx| {
        let m = &rx.msg;
        Some(match m.label() {
            proto::STATUS => g.status(),
            proto::POWER => {
                match m.mr[0] {
                    0 => {
                        if g.state != proto::OFF {
                            g.power_down();
                        }
                        g.err = 0;
                    }
                    _ => {
                        let _ = g.power_on();
                    }
                }
                g.status()
            }
            proto::ATTACH if m.handles() == 1 => reply(attach(&mut g, m)),
            proto::DETACH => {
                let regs = g.regs;
                reply(match g.dev.as_mut() {
                    Some(d) => match d.surfaces.get_mut(m.mr[0] as usize).and_then(|s| s.take()) {
                        Some((s, h)) => {
                            let _ = d.vm.unmap(s.va, s.bytes());
                            let _ = mmu::invalidate(&regs, &d.vm);
                            let _ = sys::handle_close(h);
                            Ok(0)
                        }
                        None => Err(-EINVAL),
                    },
                    None => Err(-EINVAL),
                })
            }
            proto::FILL => reply(fill(&mut g, m)),
            proto::COPY => reply(copy(&mut g, m)),
            proto::COMPUTE => reply(compute(&mut g, m)),
            proto::TEST => match test(&mut g) {
                Ok((pass, run, ns)) => Msg::from_words(0, &[pass, run, ns]),
                Err(e) => Msg::error(e),
            },
            proto::BENCH => match bench(&mut g, m.mr[0].clamp(1, 10_000)) {
                Ok((flops, ns)) => Msg::from_words(0, &[flops, ns]),
                Err(e) => Msg::error(e),
            },
            _ => Msg::error(-EINVAL),
        })
    })
}

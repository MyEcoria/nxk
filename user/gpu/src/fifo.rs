//! Host FIFO: one channel on the graphics runlist (nouveau
//! engine/fifo/gk104.c, gm107.c, gm200.c). The CPU writes the pushbuffer
//! and a GPFIFO entry, then GP_PUT in the channel's USERD through BAR1;
//! a report semaphore written by the graphics engine closes each batch.

use libnx::abi::err::EIO;
use libnx::drv::Mmio;
use libnx::{println, time};

use crate::hw::{wmb, Dma, Regs, Result};
use crate::mmu::Vm;
use crate::push::{self, Push};

/// USERD page in BAR1 (the address space's first 16 MiB are BAR1).
pub const USERD_VA: u64 = 0x10000;
pub const BAR1_SIZE: u64 = 16 << 20;
const USERD_SIZE: u32 = 0x200;
/// USERD words: GP_GET, GP_PUT.
const USERD_GP_GET: u32 = 0x88;
const USERD_GP_PUT: u32 = 0x8C;
const GPFIFO_ENTRIES: u32 = 512;
pub const PUSH_SIZE: usize = 256 << 10;

/// PBDMAs up and the FIFO's interrupts on (`gk104_fifo_init_pbdmas`,
/// `gk208_runq_init`, `gk104_fifo_init`).
pub fn init(r: &Regs) -> u32 {
    let n = r.rd(0x2004) & 0xFF;
    let mask = (1u32 << n.min(31)) - 1;
    r.wr(0x204, mask);
    r.mask(0x2A04, 0xBFFF_FFFF, 0xBFFF_FFFF);
    for i in 0..n {
        let p = i * 0x2000;
        r.mask(0x4013C + p, 0x1000_0100, 0);
        r.wr(0x40108 + p, 0xFFFF_FFFF);
        r.wr(0x4010C + p, 0xFFFF_FEFF);
        r.wr(0x40148 + p, 0xFFFF_FFFF);
        r.wr(0x4014C + p, 0xFFFF_FFFF);
        r.wr(0x4012C + p, 0x000F_4240);
    }
    r.wr(0x2254, 0x1000_0000 | (USERD_VA >> 12) as u32);
    r.wr(0x2100, 0xFFFF_FFFF);
    r.wr(0x2140, 0x7FFF_FFFF);
    n
}

/// Graphics runlist from the device table (`gk104_top_parse`); 0, as on
/// every Kepler and Maxwell, when the table is empty.
pub fn gr_runlist(r: &Regs) -> u32 {
    let mut runlist = None;
    let (mut ty, mut rl) = (u32::MAX, None);
    for i in 0..64 {
        let d = r.rd(0x22700 + i * 4);
        match d & 3 {
            0 => continue,
            2 if d & 0x10 != 0 => rl = Some((d >> 21) & 0xF),
            3 => ty = (d >> 2) & 0x1FFF_FFFF,
            _ => {}
        }
        if d & 0x8000_0000 == 0 {
            if ty == 0 && runlist.is_none() {
                runlist = rl;
            }
            (ty, rl) = (u32::MAX, None);
        }
    }
    runlist.unwrap_or(0)
}

pub struct Chan {
    pub id: u32,
    pub inst: Dma,
    userd: Dma,
    gp: Dma,
    put: u32,
    pb: Dma,
    pb_va: u64,
    runlist: Dma,
    runl: u32,
    /// Fence: the engine writes `seq` there when a batch is done.
    sem: Dma,
    sem_va: u64,
    seq: u32,
}

impl Chan {
    /// Instance block, RAMFC, USERD, GPFIFO ring and pushbuffer of
    /// channel `id` (`gk104_chan_ramfc_write`); not yet scheduled.
    pub fn new(vm: &mut Vm, id: u32, runl: u32) -> Result<Self> {
        let inst = Dma::new(4096)?;
        vm.join(&inst, crate::mmu::LIMIT);
        let userd = Dma::new(4096)?;
        vm.map_at(USERD_VA, &[userd.pa], false)?;
        let gp = Dma::new(GPFIFO_ENTRIES as usize * 8)?;
        let gp_va = vm.map(&gp, false)?;
        let pb = Dma::new(PUSH_SIZE)?;
        let pb_va = vm.map(&pb, false)?;
        let sem = Dma::new(4096)?;
        let sem_va = vm.map(&sem, false)?;
        let userd_pa = userd.pa + (id * USERD_SIZE) as u64;
        let limit2 = GPFIFO_ENTRIES.trailing_zeros();
        for (o, v) in [
            (0x08, userd_pa as u32),
            (0x0C, (userd_pa >> 32) as u32),
            (0x10, 0x0000_FACE),
            (0x30, 0xFFFF_F902),
            (0x48, gp_va as u32),
            (0x4C, (gp_va >> 32) as u32 | limit2 << 16),
            (0x84, 0x2040_0000),
            (0x94, 0x3000_0FFF),
            (0x9C, 0x0000_0100),
            (0xAC, 0x0000_001F),
            (0xE4, 0x0000_0020),
            (0xE8, id),
            (0xB8, 0xF800_0000),
            (0xF8, 0x1000_3080),
            (0xFC, 0x1000_0010),
        ] {
            inst.w32(o, v);
        }
        Ok(Self {
            id,
            inst,
            userd,
            gp,
            put: 0,
            pb,
            pb_va,
            runlist: Dma::new(4096)?,
            runl,
            sem,
            sem_va,
            seq: 0,
        })
    }

    /// Bound, enabled and put on its runlist (`gk104_chan_bind`,
    /// `_start`, `gm107_runl_insert_chan`, `gk104_runl_commit`).
    pub fn schedule(&self, r: &Regs) -> Result<()> {
        r.l2_invalidate()?;
        let c = 0x800000 + self.id * 8;
        r.mask(c + 4, 0x000F_0000, self.runl << 16);
        r.wr(c, 0x8000_0000 | (self.inst.pa >> 12) as u32);
        r.mask(c + 4, 0, 0x400);
        self.runlist.w32(0, self.id);
        self.runlist.w32(4, (self.inst.pa >> 12) as u32);
        r.l2_invalidate()?;
        r.wr(0x2270, (self.runlist.pa >> 12) as u32);
        r.wr(0x2274, self.runl << 20 | 1);
        r.wait(
            0x2284 + self.runl * 8,
            0x0010_0000,
            0,
            2_000_000,
            "runlist update",
        )
    }

    /// Off the runlist and unbound (`gk104_chan_stop`, `_unbind`).
    pub fn unschedule(&self, r: &Regs) {
        let c = 0x800000 + self.id * 8;
        r.mask(c + 4, 0, 0x800);
        r.wr(0x2270, (self.runlist.pa >> 12) as u32);
        r.wr(0x2274, self.runl << 20);
        let _ = r.wait(
            0x2284 + self.runl * 8,
            0x0010_0000,
            0,
            100_000,
            "runlist update",
        );
        r.wr(c, 0);
    }

    /// Run `p` and a fence behind it; wait up to `timeout_us`.
    pub fn run(&mut self, r: &Regs, bar1: &Mmio, mut p: Push, timeout_us: u64) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        // Idle channel first, then the graphics engine's semaphore
        // (SET_REPORT_SEMAPHORE_A-D, release, one word, flushed).
        p.immd(0, push::WFI, push::WFI_SCOPE_ALL);
        p.inc(
            push::SUBC_COMPUTE,
            0x1B00,
            &[
                (self.sem_va >> 32) as u32,
                self.sem_va as u32,
                self.seq,
                1 << 28,
            ],
        );
        let bytes = p.words.len() * 4;
        if bytes > self.pb.len() {
            return Err((EIO, "pushbuffer too large"));
        }
        for (i, w) in p.words.iter().enumerate() {
            self.pb.w32(4 * i, *w);
        }
        let e = push::gp_entry(self.pb_va, p.words.len() as u32);
        self.gp.w32(self.put as usize * 8, e[0]);
        self.gp.w32(self.put as usize * 8 + 4, e[1]);
        self.put = (self.put + 1) % (self.gp.len() as u32 / 8);
        r.l2_invalidate()?;
        wmb();
        bar1.w32(
            (USERD_VA as u32 + self.id * USERD_SIZE + USERD_GP_PUT) as usize,
            self.put,
        );
        let t0 = time::now_ns();
        let seq = self.seq;
        let sem = &self.sem;
        if !time::poll(timeout_us, || sem.r32(0) == seq) {
            // Engine status, then MC, FIFO, PBDMA 0 and GR interrupts.
            println!(
                "[gpu] batch {} not done after {} ms: GP_GET {} PUT {}, semaphore {}, engine {:#x}, interrupts MC {:#x} FIFO {:#x} PBDMA {:#x} GR {:#x}",
                seq,
                (time::now_ns() - t0) / 1_000_000,
                self.userd
                    .r32((self.id * USERD_SIZE + USERD_GP_GET) as usize),
                self.put,
                sem.r32(0),
                r.rd(0x2640),
                r.rd(0x100),
                r.rd(0x2100),
                r.rd(0x40108),
                r.rd(0x400100)
            );
            return Err((libnx::abi::err::ETIMEDOUT, "GPU batch timed out"));
        }
        r.l2_flush()
    }
}

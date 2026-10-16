//! GPU virtual memory (GMMU, 128 KiB big-page layout, small 4 KiB pages
//! only; nouveau vmmgf100.c / vmmgm200.c). One address space serves the
//! channel, the PMU's DMA and BAR1: page directory of 4 KiB (512 PDEs of
//! 128 MiB, so VAs below 64 GiB), a 256 KiB small-page table per PDE used.
//! Everything goes through the video-memory aperture, which on Tegra is
//! DRAM at its physical address, and is volatile (not cached in L2), as
//! Horizon sets it up.

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ENOMEM};

use crate::hw::{Dma, Regs, Result};

pub const PAGE: u64 = 4096;
const PDE_SPAN: u64 = 1 << 27;
const PT_SIZE: usize = 8 << 15;
/// Address space limit (512 PDEs).
pub const LIMIT: u64 = 1 << 36;
/// Compute's shared and local memory windows sit just below 4 GiB.
const HOLE: core::ops::Range<u64> = 0xFE00_0000..0x1_0000_0000;

const PTE_VALID: u64 = 1;
const PTE_PRIV: u64 = 1 << 1;
const PTE_VOL: u64 = 1 << 32;
/// Storage kind: 0 is pitch-linear memory, 0xFE the generic 16Bx2
/// block-linear layout of render targets and depth buffers.
const PTE_KIND_SHIFT: u32 = 36;
pub const KIND_PITCH: u8 = 0;
pub const KIND_GENERIC_16BX2: u8 = 0xFE;
/// 32-bit float depth, not compressed.
pub const KIND_ZF32: u8 = 0x7B;
/// PDE small-table half: video memory aperture, volatile.
const PDE_SPT_VIDMEM: u64 = 1 << 32;
const PDE_SPT_VOL: u64 = 1 << 34;

pub struct Vm {
    pub pd: Dma,
    pts: Vec<Option<Dma>>,
    next: u64,
}

impl Vm {
    /// Addresses handed out from `first`.
    pub fn new(first: u64) -> Result<Self> {
        let mut pts = Vec::new();
        pts.resize_with((LIMIT / PDE_SPAN) as usize, || None);
        Ok(Self {
            pd: Dma::new(PAGE as usize)?,
            pts,
            next: first,
        })
    }

    /// Page table for `va`, created (and its PDE written) on first use.
    fn pt(&mut self, va: u64) -> Result<&Dma> {
        let i = (va / PDE_SPAN) as usize;
        if i >= self.pts.len() {
            return Err((EINVAL, "GPU address past the limit"));
        }
        if self.pts[i].is_none() {
            let pt = Dma::new(PT_SIZE)?;
            self.pd
                .w64(i * 8, (pt.pa >> 8) << 32 | PDE_SPT_VIDMEM | PDE_SPT_VOL);
            self.pts[i] = Some(pt);
        }
        Ok(self.pts[i].as_ref().unwrap())
    }

    /// Map pages (physical addresses) at `va`; `privileged`: only for
    /// the GPU's own units (context buffers), not shaders.
    pub fn map_at(&mut self, va: u64, pages: &[u64], privileged: bool) -> Result<()> {
        self.map_at_kind(va, pages, privileged, 0)
    }

    /// Like `map_at`, with the PTE's storage kind (`KIND_*`).
    pub fn map_at_kind(
        &mut self,
        va: u64,
        pages: &[u64],
        privileged: bool,
        kind: u8,
    ) -> Result<()> {
        for (k, &pa) in pages.iter().enumerate() {
            let v = va + k as u64 * PAGE;
            let pte = (pa >> 8)
                | PTE_VALID
                | PTE_VOL
                | (kind as u64) << PTE_KIND_SHIFT
                | if privileged { PTE_PRIV } else { 0 };
            let pt = self.pt(v)?;
            pt.w64(((v % PDE_SPAN) / PAGE) as usize * 8, pte);
        }
        Ok(())
    }

    /// A free range of `len` bytes aligned to `align`, outside the hole.
    pub fn alloc(&mut self, len: u64, align: u64) -> Result<u64> {
        let mut va = self.next.next_multiple_of(align.max(PAGE));
        if va < HOLE.end && va + len > HOLE.start {
            va = HOLE.end;
        }
        if va + len > LIMIT {
            return Err((ENOMEM, "GPU address space full"));
        }
        self.next = va + len.next_multiple_of(PAGE);
        Ok(va)
    }

    /// Map contiguous DMA memory somewhere; its GPU address.
    pub fn map(&mut self, m: &Dma, privileged: bool) -> Result<u64> {
        self.map_kind(m, privileged, KIND_PITCH, PAGE)
    }

    /// Like `map`, as storage of `kind`, `align`ed (a block-linear surface
    /// starts on a block boundary).
    pub fn map_kind(&mut self, m: &Dma, privileged: bool, kind: u8, align: u64) -> Result<u64> {
        let va = self.alloc(m.len() as u64, align)?;
        let pages: Vec<u64> = (0..m.len() as u64 / PAGE)
            .map(|k| m.pa + k * PAGE)
            .collect();
        self.map_at_kind(va, &pages, privileged, kind)?;
        Ok(va)
    }

    pub fn unmap(&mut self, va: u64, len: u64) -> Result<()> {
        for k in 0..len.div_ceil(PAGE) {
            let v = va + k * PAGE;
            let pt = self.pt(v)?;
            pt.w64(((v % PDE_SPAN) / PAGE) as usize * 8, 0);
        }
        Ok(())
    }

    /// Instance block fields for this address space (`gf100_vmm_join`):
    /// directory base (video memory, volatile, 128 KiB big pages), limit.
    pub fn join(&self, inst: &Dma, limit: u64) {
        inst.w64(0x200, self.pd.pa | 1 << 2);
        inst.w64(0x208, limit - 1);
    }
}

/// Drop the GMMU's cached translations of this directory
/// (`gf100_vmm_invalidate`, PAGE_ALL).
pub fn invalidate(r: &Regs, vm: &Vm) -> Result<()> {
    r.l2_invalidate()?;
    r.until(2_000_000, "MMU flush slot", || {
        r.rd(0x100C80) & 0x00FF_0000 != 0
    })?;
    r.wr(0x100CB8, ((vm.pd.pa >> 12) << 4) as u32);
    r.wr(0x100CBC, 0x8000_0001);
    r.wait(0x100C80, 0x8000, 0x8000, 2_000_000, "MMU invalidate")
}

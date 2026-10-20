//! NVIDIA's secure boot on the GM20B (nouveau acr/gm20b.c, gm200.c,
//! falcon/gm200.c, pmu/gm200.c). The "load" HS ucode runs on the PMU: it
//! copies the WPR image into the GPU firmware carveout hekate's L4T entry
//! locked (MC_SECURITY_CARVEOUT2), then loads FECS from it right away (not
//! lazily through the PMU's own firmware: NVIDIA's gm20b flow), leaving
//! FECS light-secure, halted and ready to start.

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, EIO, ENOSPC};
use libnx::{println, time};

use crate::falcon::{self, Falcon};
use crate::fw::{self, BlDmem};
use crate::hw::{Dma, Regs, Result};
use crate::mmu::{self, Vm};

/// The firmware files it needs (`/firmware/nvidia/gm20b`).
pub struct Files {
    pub load: Vec<u8>,
    pub bl: Vec<u8>,
    pub fecs_bl: Vec<u8>,
    pub fecs_inst: Vec<u8>,
    pub fecs_data: Vec<u8>,
    pub fecs_sig: Vec<u8>,
}

/// Write-protected region as the GPU's MMU reports it
/// (`gm200_acr_wpr_check`): what hekate's carveout gave.
pub struct Wpr {
    pub start: u64,
    pub end: u64,
}

pub fn wpr(r: &Regs) -> Wpr {
    r.wr(0x100CD4, 2);
    let start = ((r.rd(0x100CD4) & 0xFFFF_FF00) as u64) << 8;
    r.wr(0x100CD4, 3);
    let end = (((r.rd(0x100CD4) & 0xFFFF_FF00) as u64) << 8) + 0x20000;
    Wpr { start, end }
}

/// PMU engine reset through PMC_ENABLE bit 13, then its memories
/// scrubbed (`gm200_flcn_disable` / `_enable`).
fn reset_pmu(r: &Regs, pmu: &Falcon) -> Result<()> {
    pmu.mask(0x048, 3, 0);
    pmu.wr(0x014, 0xFFFF_FFFF);
    r.mask(0x200, 0x2000, 0);
    r.rd(0x200);
    r.mask(0x200, 0, 0x2000);
    r.rd(0x200);
    pmu.wait_scrub("PMU memory scrub")?;
    pmu.wr(0x084, r.rd(0));
    Ok(())
}

/// Instance-block DMA for the PMU (`gm200_pmu_flcn_bind_inst`): DMA
/// indices UCODE, VIRT, then physical video, coherent and non-coherent
/// system memory.
fn bind(pmu: &Falcon, inst: &Dma) {
    pmu.mask(0x048, 0, 1);
    for (i, v) in [4, 0, 4, 5, 6].iter().enumerate() {
        pmu.wr(0xE00 + 4 * i as u32, *v);
    }
    pmu.mask(0x090, 0, 0x10000);
    pmu.wr(0x480, 1 << 30 | (inst.pa >> 12) as u32);
}

/// Bind state machine (`gm200_pmu_flcn_bind_stat`).
fn bind_state(pmu: &Falcon) -> u32 {
    pmu.wr(0x200, 0x30E);
    (pmu.rd(0x20C) >> 12) & 7
}

fn bad(e: &'static str) -> (i64, &'static str) {
    (EINVAL, e)
}

/// Run the loader. Afterwards FECS holds its firmware from the WPR.
pub fn boot(r: &Regs, vm: &mut Vm, f: &Files, slot: &mut mmu::Slot) -> Result<Wpr> {
    let w = wpr(r);
    let ls = fw::ls(&f.fecs_bl, &f.fecs_inst, &f.fecs_data, &f.fecs_sig).map_err(bad)?;
    let image = fw::wpr(&ls, fw::FALCON_FECS, w.start);
    println!(
        "[gpu] WPR {:#x}-{:#x}, FECS image {} bytes",
        w.start,
        w.end,
        image.len()
    );
    if image.len() as u64 > w.end.saturating_sub(w.start) {
        return Err((ENOSPC, "WPR image larger than the carveout"));
    }
    let blob = Dma::new(image.len())?;
    blob.write(0, &image);
    let mut hs = fw::hs(&f.load).map_err(bad)?;
    let bl = fw::bl(&f.bl).map_err(bad)?;

    let pmu = Falcon::new(*r, falcon::PMU);
    reset_pmu(r, &pmu)?;
    // Production or debug keys (`gm200_flcn_fw_signature`).
    let debug = pmu.rd(0xC08) & 1 << 20 != 0;
    hs.sign(debug);
    hs.set_blob(blob.pa, image.len() as u32);
    let img = Dma::new(hs.img.len())?;
    img.write(0, &hs.img);
    let va = vm.map_in(slot, &img)?;
    let inst = Dma::new(4096)?;
    vm.join(&inst, mmu::LIMIT);
    mmu::invalidate(r, vm)?;

    // DMA context (`gm200_flcn_fw_load`). NVIDIA's own gm20b driver does
    // not wait for the bind; nouveau does, so wait but only report.
    bind(&pmu, &inst);
    let bound = time::poll(10_000, || bind_state(&pmu) == 5);
    pmu.mask(0x004, 0, 8);
    pmu.mask(0x058, 0, 2);
    let idle = time::poll(10_000, || bind_state(&pmu) == 0);
    if !bound || !idle {
        println!("[gpu] PMU instance bind state {}", bind_state(&pmu));
    }

    // Bootloader at the top of IMEM, running at its tag address; its
    // arguments at DMEM 0 say where to DMA the HS code and data from.
    let (imem, _) = pmu.sizes();
    let at = imem.wrapping_sub(bl.code.len() as u32);
    pmu.imem(at, &bl.code, bl.boot_addr >> 8, false);
    let args = BlDmem {
        ctx_dma: fw::DMAIDX_VIRT,
        code_dma_base: va >> 8,
        non_sec_code_off: hs.nmem_base,
        non_sec_code_size: hs.nmem_size,
        sec_code_off: hs.imem_base,
        sec_code_size: hs.imem_size,
        code_entry_point: 0,
        data_dma_base: (va + hs.dmem_base_img as u64) >> 8,
        data_size: hs.dmem_size,
    };
    pmu.dmem(0, &args.bytes());
    r.l2_invalidate()?;

    // Boot (`gm200_flcn_fw_boot`): halted with mailbox 0 cleared on success.
    pmu.set_mailbox0(0xCAFE_BEEF);
    pmu.set_bootvec(bl.boot_addr);
    let t0 = time::now_ns();
    pmu.start();
    let halted = pmu.wait_halt(2_000_000, "ACR loader did not halt");
    let (m0, m1) = pmu.mailbox();
    pmu.wr(0x004, 0x10);
    println!(
        "[gpu] ACR loader: {} keys, IMEM {} KiB, mailbox {:#x} {:#x}, {} us",
        if debug { "debug" } else { "production" },
        imem / 1024,
        m0,
        m1,
        crate::hw::since_us(t0)
    );
    vm.unmap(va, img.len() as u64)?;
    mmu::invalidate(r, vm)?;
    halted?;
    if m0 != 0 {
        return Err((EIO, "ACR loader failed"));
    }
    let after = wpr(r);
    if after.start != w.start || after.end != w.end {
        println!("[gpu] WPR now {:#x}-{:#x}", after.start, after.end);
    }
    Ok(w)
}

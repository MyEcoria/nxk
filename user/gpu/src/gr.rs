//! Graphics engine: unit counts, init from the netlists, FECS and GPCCS
//! started, golden context image and a channel's context (nouveau
//! engine/gr/gf100.c, gk20a.c, gm20b.c, ctxgf100.c, ctxgm20b.c and the
//! gk104 / gm107 / gm200 helpers they use).

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, EIO, ENODEV};
use libnx::{println, time};

use crate::falcon::{self, Falcon};
use crate::fw;
use crate::hw::{Dma, Regs, Result};
use crate::mmu::{self, Vm};

const GPC_MAX: usize = 4;
const TPC_MAX: usize = 8;

fn gpc(g: usize, r: u32) -> u32 {
    0x500000 + g as u32 * 0x8000 + r
}

fn tpc(g: usize, t: usize, r: u32) -> u32 {
    0x504000 + g as u32 * 0x8000 + t as u32 * 0x800 + r
}

fn ppc(g: usize, p: usize, r: u32) -> u32 {
    0x503000 + g as u32 * 0x8000 + p as u32 * 0x200 + r
}

/// Units present (`gf100_gr_oneinit`, GM20B: one PPC per GPC).
pub struct Topo {
    pub gpc_nr: usize,
    pub tpc_nr: [usize; GPC_MAX],
    pub tpc_total: usize,
    tpc_max: usize,
    ppc_tpc_nr: [usize; GPC_MAX],
    ppc_mask: [u32; GPC_MAX],
    /// SM order: (GPC, TPC).
    sms: Vec<(usize, usize)>,
    tile: [u8; 32],
    tile_row_offset: u32,
}

pub fn topology(r: &Regs) -> Result<Topo> {
    let gpc_nr = (r.rd(0x409604) & 0x1F) as usize;
    if gpc_nr == 0 || gpc_nr > GPC_MAX {
        return Err((ENODEV, "no graphics cluster"));
    }
    let mut t = Topo {
        gpc_nr,
        tpc_nr: [0; GPC_MAX],
        tpc_total: 0,
        tpc_max: 0,
        ppc_tpc_nr: [0; GPC_MAX],
        ppc_mask: [0; GPC_MAX],
        sms: Vec::new(),
        tile: [0xFF; 32],
        tile_row_offset: 0,
    };
    for g in 0..gpc_nr {
        // FS_GPC: TPC count in bits 4:0, ZCULL count in 20:16 (nouveau's
        // u8 drops it); PES TPC mask in bits 7:0.
        let fs = r.rd(gpc(g, 0x2608));
        let n = (fs & 0x1F) as usize;
        if n == 0 || n > TPC_MAX {
            println!("[gpu] GPC {} FS_GPC {:#x}", g, fs);
            return Err((ENODEV, "no texture processing cluster"));
        }
        t.tpc_nr[g] = n;
        t.tpc_max = t.tpc_max.max(n);
        t.tpc_total += n;
        let m = r.rd(gpc(g, 0x0C30)) & 0xFF;
        if m != 0 {
            t.ppc_mask[g] = 1;
            t.ppc_tpc_nr[g] = m.count_ones() as usize;
        }
    }
    // `gf100_gr_oneinit_sm_id`.
    for tp in 0..t.tpc_max {
        for g in 0..gpc_nr {
            if tp < t.tpc_nr[g] {
                t.sms.push((g, tp));
            }
        }
    }
    tiles(&mut t);
    Ok(t)
}

/// Screen tile to GPC map (`gf100_gr_oneinit_tiles`).
fn tiles(t: &mut Topo) {
    const PRIMES: [u32; 17] = [
        3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61,
    ];
    let total = t.tpc_total as u32;
    t.tile_row_offset = match total {
        15 => 6,
        14 => 5,
        13 => 2,
        11 => 7,
        10 => 6,
        7 | 5 => 1,
        3 => 2,
        2 | 1 => 1,
        _ => PRIMES
            .iter()
            .copied()
            .find(|p| !total.is_multiple_of(*p))
            .unwrap_or(3),
    };
    let n = t.gpc_nr;
    let mut map: Vec<usize> = (0..n).collect();
    map.sort_by(|a, b| t.tpc_nr[*b].cmp(&t.tpc_nr[*a]));
    let mul: i32 = if (n * t.tpc_max) % 2 == 1 { 2 } else { 1 };
    let denom = (n * t.tpc_max) as i32 * mul;
    let mut frac = [0i32; GPC_MAX];
    let mut run = [0i32; GPC_MAX];
    for i in 0..n {
        frac[i] = t.tpc_nr[map[i]] as i32 * n as i32 * mul;
        run[i] = frac[i] + i as i32 * t.tpc_max as i32 * mul - denom / 2;
    }
    let mut i = 0;
    while i < t.tpc_total {
        for j in 0..n {
            if run[j] * 2 >= denom {
                if i < t.tpc_total {
                    t.tile[i] = map[j] as u8;
                }
                i += 1;
                run[j] += frac[j] - denom;
            } else {
                run[j] += frac[j];
            }
        }
    }
}

/// Netlists: registers and methods NVIDIA's firmware release lists.
pub struct Nets {
    pub nonctx: Vec<u8>,
    pub ctx: Vec<u8>,
    pub bundle: Vec<u8>,
    pub method: Vec<u8>,
}

fn mmio(r: &Regs, list: impl Iterator<Item = (u32, u32)>) {
    for (a, v) in list {
        r.wr(a, v);
    }
}

/// GR status, activity, interrupts and trapped-method registers
/// (nvgpu's GR dump starts with the same ones).
pub fn report(r: &Regs, what: &str) {
    println!(
        "[gpu] {}: status {:#x} {:#x} {:#x}, engine {:#x}, activity {:#x} {:#x} {:#x} {:#x}, GPC {:#x}, BE {:#x}, FE power {:#x}, FECS mailbox {:#x}, interrupts GR {:#x} exception {:#x} FECS {:#x} MC {:#x}, FIFO engine {:#x}, FE ESR {:#x} info {:#x}, trapped {:#x} {:#x} {:#x}, data error {:#x}",
        what,
        r.rd(0x400700),
        r.rd(0x400604),
        r.rd(0x400608),
        r.rd(0x40060C),
        r.rd(0x400380),
        r.rd(0x400384),
        r.rd(0x400388),
        r.rd(0x400390),
        r.rd(0x502C80),
        r.rd(0x410200),
        r.rd(0x404170),
        r.rd(0x409800),
        r.rd(0x400100),
        r.rd(0x400108),
        r.rd(0x409C18),
        r.rd(0x100),
        r.rd(0x2640),
        r.rd(0x404000),
        r.rd(0x4041B0),
        r.rd(0x400704),
        r.rd(0x400708),
        r.rd(0x40070C),
        r.rd(0x400110)
    );
    // The GPC and TPC traps behind a GR exception (nouveau
    // gf100_gr_trap_gpc / _tpc / _mp): GPC exception, then per TPC the
    // exception, TEX, and MP warp and global errors.
    // The GR trap sources (nouveau gf100_gr_trap_intr): dispatch, M2MF,
    // shader (its program header error bits and stage), ROP/CCACHE.
    println!(
        "[gpu] {}: traps dispatch {:#x} m2mf {:#x} shader {:#x} ccache {:#x}",
        what,
        r.rd(0x400804),
        r.rd(0x404600),
        r.rd(0x405840),
        r.rd(0x408030)
    );
    println!("[gpu] {}: GPC trap {:#x}", what, r.rd(0x502C90));
    for tpc in 0..2u32 {
        let t = 0x504000 + tpc * 0x800;
        println!(
            "[gpu] {}: TPC {} exception {:#x} TEX {:#x} MP warp {:#x} global {:#x}",
            what,
            tpc,
            r.rd(t + 0x508),
            r.rd(t + 0x224),
            r.rd(t + 0x648),
            r.rd(t + 0x650)
        );
    }
}

/// Idle: disabled, or not busy (`gf100_gr_wait_idle`; the FIFO's context
/// switch flag is not tracked here: one channel). A timeout reports the
/// status and activity registers nvgpu's GR dump starts with.
fn wait_idle(r: &Regs, what: &'static str) -> Result<()> {
    let res = r.until(2_000_000, what, || {
        r.rd(0x400700);
        r.rd(0x200) & 0x1000 == 0 || r.rd(0x40060C) & 1 == 0
    });
    if res.is_err() {
        report(r, what);
    }
    res
}

/// Bundles through the ICMD interface (`gf100_gr_icmd`).
fn icmd(r: &Regs, list: impl Iterator<Item = (u32, u32)>) -> Result<()> {
    r.wr(0x400208, 0x8000_0000);
    let mut last = None;
    for (a, v) in list {
        if last != Some(v) {
            r.wr(0x400204, v);
            last = Some(v);
        }
        r.wr(0x400200, a);
        if a & 0xFFFF == 0xE100 {
            wait_idle(r, "GR busy in the bundles")?;
        }
        r.wait(0x400700, 4, 0, 2_000_000, "GR bundle")?;
    }
    r.wr(0x400208, 0);
    Ok(())
}

/// Class methods through the MME shadow (`gf100_gr_mthd`): the netlist
/// word is method << 14 | class already.
fn mthd(r: &Regs, list: impl Iterator<Item = (u32, u32)>) {
    let mut last = None;
    for (a, v) in list {
        if last != Some(v) {
            r.wr(0x40448C, v);
            last = Some(v);
        }
        r.wr(0x404488, 0x8000_0000 | a);
    }
}

/// FECS method with a mailbox answer (`gf100_gr_fecs_*`).
fn fecs(r: &Regs, mthd: u32, data: u32, what: &'static str) -> Result<u32> {
    r.wr(0x409800, 0);
    r.wr(0x409500, data);
    r.wr(0x409504, mthd);
    let mut v = 0;
    r.until(2_000_000, what, || {
        v = r.rd(0x409800);
        v != 0
    })?;
    Ok(v)
}

/// Sizes FECS reports once running.
pub struct Sizes {
    pub image: u32,
}

/// GR bring-up (`gk20a_gr_init`, `gf100_gr_init_ctxctl_ext`). FECS was
/// loaded by the ACR; GPCCS is loaded here, unsecured.
pub fn init(r: &Regs, t: &Topo, gpccs_inst: &[u8], gpccs_data: &[u8], n: &Nets) -> Result<Sizes> {
    r.wr(0x40802C, 1);
    mmio(r, fw::av(&n.nonctx));
    r.wait(0x40910C, 6, 0, 2_000_000, "FECS scrub")?;
    r.wait(0x41A10C, 6, 0, 2_000_000, "GPCCS scrub")?;
    wait_idle(r, "GR busy after the registers")?;

    // MMU debug buffers and mode for the GPCs (`gm20b_gr_init_gpc_mmu`,
    // secure boot: no bypass).
    r.wr(0x418880, r.rd(0x100C80) & 0xF000_187F);
    r.wr(0x418890, 0);
    r.wr(0x418894, 0);
    r.wr(0x4188B0, r.rd(0x100CC4));
    r.wr(0x4188B4, r.rd(0x100CC8));
    r.wr(0x4188B8, r.rd(0x100CCC));
    r.wr(0x4188AC, r.rd(0x100800));
    r.mask(0x503018, 0, 1);
    zcull(r, t);
    let fbps = r.rd(0x120074);
    r.mask(0x408850, 0xF, fbps & 0xF);
    r.mask(0x408958, 0xF, fbps & 0xF);
    r.wr(0x400500, 0x0001_0001);
    r.wr(0x400100, 0xFFFF_FFFF);
    r.wr(0x40013C, 0xFFFF_FFFF);
    r.wr(0x409C24, 0x000F_0000);
    r.wr(0x404000, 0xC000_0000);
    r.wr(0x404600, 0xC000_0000);
    r.wr(0x419E44, 0x00DF_FFFE);
    r.wr(0x419E4C, 0x5);
    r.wr(0x419D0C, 0x2);
    r.wr(0x41AC94, (((1u32 << t.tpc_total) - 1) & 0xFF) << 16);
    for reg in [0x400108, 0x400138, 0x400118, 0x400130, 0x40011C, 0x400134] {
        r.wr(reg, 0xFFFF_FFFF);
    }

    // GPCCS code and data, then both falcons started (FECS through its
    // alias: the secure loader made it light-secure).
    let gpccs = Falcon::new(*r, falcon::GPCCS);
    let fecs_f = Falcon::new(*r, falcon::FECS);
    gpccs.dmem(0, gpccs_data);
    gpccs.imem(0, gpccs_inst, 0, false);
    r.wr(0x409800, 0);
    r.wr(0x41A10C, 0);
    r.wr(0x40910C, 0);
    gpccs.start();
    fecs_f.start();
    if let Err(e) = r.wait(0x409800, 1, 1, 2_000_000, "FECS did not start") {
        let (m0, m1) = fecs_f.mailbox();
        println!(
            "[gpu] FECS cpuctl {:#x} mailbox {:#x} {:#x}, GPCCS cpuctl {:#x}",
            fecs_f.cpuctl(),
            m0,
            m1,
            gpccs.cpuctl()
        );
        return Err(e);
    }
    // Watchdog off, then the context image size.
    r.wr(0x409800, 0);
    r.wr(0x409500, 0x7FFF_FFFF);
    r.wr(0x409504, 0x21);
    let image = fecs(r, 0x10, 0, "FECS image size")?;
    let zcull = fecs(r, 0x16, 0, "FECS zcull size")?;
    let pm = fecs(r, 0x25, 0, "FECS perfmon size")?;
    println!(
        "[gpu] FECS up: context {} bytes, zcull {}, perfmon {}",
        image, zcull, pm
    );
    Ok(Sizes { image })
}

/// Z-cull banks per screen tile (`gf117_gr_init_zcull`).
fn zcull(r: &Regs, t: &Topo) {
    let magic = 0x0080_0000u32.div_ceil(t.tpc_total as u32);
    let mut bank = [0u32; GPC_MAX];
    for i in (0..32).step_by(8) {
        let mut data = 0;
        for j in 0..8 {
            if i + j >= t.tpc_total {
                break;
            }
            let g = t.tile[i + j] as usize;
            data |= bank[g] << (j * 4);
            bank[g] += 1;
        }
        r.wr(0x418980 + (i / 8) as u32 * 4, data);
    }
    for g in 0..t.gpc_nr {
        r.wr(gpc(g, 0x0914), t.tile_row_offset << 8 | t.tpc_nr[g] as u32);
        r.wr(gpc(g, 0x0910), 0x0004_0000 | t.tpc_total as u32);
        r.wr(gpc(g, 0x0918), magic);
    }
    r.wr(0x41BFD4, magic);
}

/// GM20B context constants (`gm20b_grctx`).
const BUNDLE_SIZE: u32 = 0x1800;
const BUNDLE_MIN_GPM_FIFO_DEPTH: u32 = 0x182;
const BUNDLE_TOKEN_LIMIT: u32 = 0x1C0;
const PAGEPOOL_SIZE: u32 = 0x8000;
const ATTRIB_NR_MAX: u32 = 0x600;
const ATTRIB_NR: u32 = 0x400;
const ALPHA_NR_MAX: u32 = 0xC00;
const ALPHA_NR: u32 = 0x800;

/// Buffers every context shares: page pool, bundle and attribute
/// circular buffers, mapped once.
pub struct Global {
    _mem: [Dma; 3],
    pagepool: u64,
    bundle: u64,
    attrib: u64,
}

pub fn global(vm: &mut Vm, t: &Topo) -> Result<Global> {
    let attrib_size = 0x20 * (ATTRIB_NR_MAX + ALPHA_NR_MAX) * t.tpc_total as u32;
    let pp = Dma::new(PAGEPOOL_SIZE as usize)?;
    let bu = Dma::new(BUNDLE_SIZE as usize)?;
    let at = Dma::new(attrib_size as usize)?;
    let pagepool = vm.map(&pp, true)?;
    let bundle = vm.map(&bu, true)?;
    let attrib = vm.map(&at, false)?;
    Ok(Global {
        _mem: [pp, bu, at],
        pagepool,
        bundle,
        attrib,
    })
}

/// Context registers pointing at the global buffers, written directly
/// (golden image) or listed for FECS to apply (a channel's context).
fn ctx_regs(g: &Global, t: &Topo, p: &mut dyn FnMut(u32, u32)) {
    pagepool(g, p);
    bundle(g, p);
    attrib_cb(g, p);
    attrib(t, p);
}

/// gm107_grctx_generate_pagepool.
fn pagepool(g: &Global, p: &mut dyn FnMut(u32, u32)) {
    p(0x40800C, (g.pagepool >> 8) as u32);
    p(0x408010, 0x8000_0000);
    p(0x419004, (g.pagepool >> 8) as u32);
    p(0x419008, 0);
    p(0x4064CC, 0x8000_0000);
    p(0x418E30, 0x8000_0000);
}

/// gm107_grctx_generate_bundle.
fn bundle(g: &Global, p: &mut dyn FnMut(u32, u32)) {
    let state = BUNDLE_MIN_GPM_FIFO_DEPTH.min(BUNDLE_SIZE / 0x20);
    p(0x408004, (g.bundle >> 8) as u32);
    p(0x408008, 0x8000_0000 | BUNDLE_SIZE >> 8);
    p(0x418E24, (g.bundle >> 8) as u32);
    p(0x418E28, 0x8000_0000 | BUNDLE_SIZE >> 8);
    p(0x4064C8, state << 16 | BUNDLE_TOKEN_LIMIT);
}

/// gm107_grctx_generate_attrib_cb.
fn attrib_cb(g: &Global, p: &mut dyn FnMut(u32, u32)) {
    let a = (g.attrib >> 12) as u32;
    p(0x418810, 0x8000_0000 | a);
    p(0x419848, 0x1000_0000 | a);
    p(0x419C2C, 0x1000_0000 | a);
}

/// gm107_grctx_generate_attrib.
fn attrib(t: &Topo, p: &mut dyn FnMut(u32, u32)) {
    p(0x405830, ATTRIB_NR << 16 | ALPHA_NR);
    p(0x4064C4, (ALPHA_NR / 4) << 16 | 0xFFFF);
    let mut bo = 0;
    let mut ao = ATTRIB_NR_MAX * t.tpc_total as u32;
    for g in 0..t.gpc_nr {
        let n = g as u32;
        if t.ppc_mask[g] & 1 == 0 {
            continue;
        }
        let nr = t.ppc_tpc_nr[g] as u32;
        let (asz, bsz) = (ALPHA_NR * nr, ATTRIB_NR * nr);
        let o = ppc(g, 0, 0);
        p(o + 0xC0, bsz);
        p(o + 0xF4, bo);
        bo += ATTRIB_NR_MAX * nr;
        p(o + 0xE4, asz);
        p(o + 0xF8, ao);
        ao += ALPHA_NR_MAX * nr;
        p(0x418EA0 + n * 4, (bsz / 3) << 16 | bsz);
    }
}

/// SM IDs, TPC counts and screen tile mapping
/// (`gf100_grctx_generate_floorsweep` with GM20B's hooks).
fn floorsweep(r: &Regs, t: &Topo) {
    for (sm, &(g, tp)) in t.sms.iter().enumerate() {
        let sm = sm as u32;
        r.wr(tpc(g, tp, 0x698), sm);
        r.wr(gpc(g, 0x0C10 + tp as u32 * 4), sm);
        r.wr(tpc(g, tp, 0x088), sm);
    }
    for i in 0..4 {
        let mut data = 0;
        for j in 0..8 {
            let g = i * 8 + j;
            if g < t.gpc_nr {
                data |= (t.tpc_nr[g] as u32) << (j * 4);
            }
        }
        r.wr(0x405870 + i as u32 * 4, data);
        r.wr(0x406028 + i as u32 * 4, data);
    }
    // gf117_grctx_generate_rop_mapping.
    let total = t.tpc_total as u32;
    let mut data = [0u32; 6];
    for i in 0..32 {
        data[i / 6] |= (t.tile[i] as u32 & 7) << ((i % 6) * 5);
    }
    let (mut shift, mut ntpcv) = (0u32, total);
    while ntpcv & (1 << 4) == 0 {
        ntpcv <<= 1;
        shift += 1;
    }
    let d0 = ntpcv << 16 | shift << 21 | ((1 << 5) % ntpcv) << 24;
    let mut d1 = 0;
    for i in 1..7 {
        d1 |= ((1 << (i + 5)) % ntpcv) << ((i - 1) * 5);
    }
    r.wr(0x418BB8, total << 8 | t.tile_row_offset);
    for (i, v) in data.iter().enumerate() {
        r.wr(0x418B08 + i as u32 * 4, *v);
    }
    r.wr(0x41BFD0, total << 8 | t.tile_row_offset | d0);
    r.wr(0x41BFE4, d1);
    for (i, v) in data.iter().enumerate() {
        r.wr(0x41BF00 + i as u32 * 4, *v);
    }
    r.wr(0x4078BC, total << 8 | t.tile_row_offset);
    for (i, v) in data.iter().enumerate() {
        r.wr(0x40780C + i as u32 * 4, *v);
    }
}

/// gm200_grctx_generate_smid_config.
fn smid_config(r: &Regs, t: &Topo) {
    let mut dist = [0u32; TPC_MAX * GPC_MAX / 4];
    let mut gpcs = [0u32; GPC_MAX];
    for (sm, &(g, tp)) in t.sms.iter().enumerate() {
        dist[sm / 4] |= ((g as u32) << 4 | tp as u32) << ((sm % 4) * 8);
        gpcs[g] |= (sm as u32) << (tp * 8);
    }
    for (i, d) in dist.iter().take(t.tpc_total.div_ceil(4)).enumerate() {
        r.wr(0x405B60 + i as u32 * 4, *d);
    }
    for (g, v) in gpcs.iter().take(t.gpc_nr).enumerate() {
        r.wr(0x405BA0 + g as u32 * 4, *v);
    }
}

/// Room before the image in the golden generation buffer (`CB_RESERVED`).
const CB_RESERVED: usize = 0x80000;

/// Instance-block words pointing at a context (`gk104_ectx_bind`).
fn bind_ctx(inst: &Dma, va: u64) {
    inst.w32(0x210, va as u32 | 4);
    inst.w32(0x214, (va >> 32) as u32);
}

/// Golden context image (`gf100_grctx_generate` + `gm20b_grctx_generate_main`):
/// FECS builds it on `inst`'s channel from the netlists, then saves it.
pub fn golden(
    r: &Regs,
    vm: &mut Vm,
    inst: &Dma,
    t: &Topo,
    g: &Global,
    n: &Nets,
    size: u32,
) -> Result<Vec<u8>> {
    // FE power forced on while the context-switch units reset.
    r.wr(0x404170, 0x12);
    r.wait(0x404170, 0x10, 0, 2_000_000, "FE power")?;
    r.wr(0x409614, 0x70);
    time::sleep_us(10);
    r.mask(0x409614, 0, 0x700);
    time::sleep_us(10);
    r.rd(0x409614);
    r.wr(0x404170, 0x10);
    r.wait(0x404170, 0x10, 0, 2_000_000, "FE power")?;
    r.wr(0x40802C, 1);

    let data = Dma::new(CB_RESERVED + size as usize)?;
    let va = vm.map(&data, false)?;
    bind_ctx(inst, va + CB_RESERVED as u64);
    mmu::invalidate(r, vm)?;
    let ptr = 0x8000_0000 | (inst.pa >> 12) as u32;
    let bound = fecs(r, 3, ptr, "FECS bind")?;
    if bound & 0x20 != 0 || bound & 0x10 == 0 {
        return Err((EIO, "FECS refused the context"));
    }
    for (o, v) in [(0x1C, 1), (0x20, 0), (0x28, 0), (0x2C, 0)] {
        data.w32(o, v);
    }

    // gm20b_grctx_generate_main, registers written directly.
    mmio(r, fw::aiv(&n.ctx));
    wait_idle(r, "GR busy after the context registers")?;
    let idle_timeout = r.mask(0x404154, 0xFFFF_FFFF, 0);
    let mut direct = |a: u32, v: u32| r.wr(a, v);
    attrib_cb(g, &mut direct);
    attrib(t, &mut direct);
    // gk104_grctx_generate_unkn.
    r.mask(0x418C6C, 0, 1);
    r.mask(0x41980C, 0, 0x10);
    r.mask(0x41BE08, 0, 4);
    r.mask(0x4064C0, 0, 0x8000_0000);
    r.mask(0x405800, 0, 0x0800_0000);
    r.mask(0x419C00, 0, 8);
    floorsweep(r, t);
    for i in 0..8 {
        r.wr(0x4064D0 + i * 4, 0);
    }
    r.wr(0x405B00, (t.tpc_total as u32) << 8 | t.gpc_nr as u32);
    r.wr(0x408908, r.rd(0x410108) | 0x8000_0000);
    let mut mask = 0;
    for gi in 0..t.gpc_nr {
        mask |= ((1 << t.tpc_nr[gi]) - 1) << (gi * 4);
    }
    r.wr(0x4041C4, mask);
    smid_config(r, t);
    wait_idle(r, "GR busy after the SM IDs")?;
    // Bundles before methods, FE's go-idle timeout off until they are all
    // in (nvgpu `gk20a_init_sw_bundle`): with it on, an injection landing
    // as FE goes idle fails with DISPATCH INJECTED_BUNDLE_ERROR.
    icmd(r, fw::av(&n.bundle))?;
    r.wr(0x404154, idle_timeout);
    wait_idle(r, "GR busy after the idle timeout")?;
    mthd(r, fw::av(&n.method));
    wait_idle(r, "GR busy after the methods")?;
    let mut direct = |a: u32, v: u32| r.wr(a, v);
    pagepool(g, &mut direct);
    bundle(g, &mut direct);

    // Saved once the pipeline drained (`gf100_gr_fecs_wfi_golden_save`).
    r.mask(0x409800, 3, 0);
    r.wr(0x409500, ptr);
    r.wr(0x409504, 9);
    let mut st = 0;
    r.until(2_000_000, "FECS golden save", || {
        st = r.rd(0x409800);
        st & 3 != 0
    })?;
    if st & 2 != 0 {
        return Err((EIO, "FECS golden save failed"));
    }
    r.mask(0x409B00, 0x8000_0000, 0);
    r.l2_flush()?;
    let mut image = alloc::vec![0u8; size as usize];
    data.read(CB_RESERVED, &mut image);
    bind_ctx(inst, 0);
    vm.unmap(va, data.len() as u64)?;
    mmu::invalidate(r, vm)?;
    Ok(image)
}

/// A channel's context: the golden image, plus the list of registers
/// FECS applies at its first load (`gf100_gr_chan_new` / `_bind`).
pub struct ChanCtx {
    _ctx: Dma,
    _list: Dma,
}

pub fn channel(vm: &mut Vm, inst: &Dma, t: &Topo, g: &Global, golden: &[u8]) -> Result<ChanCtx> {
    if golden.is_empty() {
        return Err((EINVAL, "no golden context"));
    }
    let ctx = Dma::new(golden.len())?;
    ctx.write(0, golden);
    let list = Dma::new(4096)?;
    let mut words = 0usize;
    ctx_regs(g, t, &mut |a, v| {
        list.w32(words * 4, a);
        list.w32(words * 4 + 4, v);
        words += 2;
    });
    let list_va = vm.map(&list, true)?;
    for (o, v) in [
        (0xF4, 0),
        (0xF8, 0),
        (0x10, (words / 2) as u32),
        (0x14, list_va as u32),
        (0x18, (list_va >> 32) as u32),
        (0x1C, 1),
        (0x20, 0),
        (0x28, 0),
        (0x2C, 0),
    ] {
        ctx.w32(o, v);
    }
    let ctx_va = vm.map(&ctx, true)?;
    bind_ctx(inst, ctx_va);
    Ok(ChanCtx {
        _ctx: ctx,
        _list: list,
    })
}

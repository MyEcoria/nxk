//! GPU rail, chip fuses and the GPC clock.
//!
//! Rail: on the Erista a MAX77621 at 0x1C on I2C5, its EN pin driven by
//! the MAX77620's GPIO6 (hekate bdk/power/max7762x.c: the same part and
//! sequence as the CPU rail). Clock: the GPCPLL in noise-aware mode,
//! 76.8 MHz at power-up then sys-clk's steps, as nouveau's gm20b clock
//! code does for speedo ID 1 and up.

use libnx::abi::err::{EIO, ENODEV};
use libnx::drv::Mmio;
use libnx::i2c::I2c;
use libnx::{println, time};

use crate::hw::{Regs, Result};
use crate::pll::{self, Calib, Mnp};

const PMIC: u8 = 0x3C;
const PMIC_GPIO0: u8 = 0x36;
const PMIC_AME_GPIO: u8 = 0x40;
/// Output, high, push-pull (MAX77620_CNFG_GPIO_*).
const GPIO_OUT_HIGH: u8 = 1 << 3 | 1;
const GPU_EN_GPIO: u8 = 6;
const MAX77621_GPU: u8 = 0x1C;
const VOUT: u8 = 0x00;
const VOUT_DVS: u8 = 0x01;
const CONTROL1: u8 = 0x02;
const CONTROL2: u8 = 0x03;
const VOUT_ENABLE: u8 = 0x80;
/// hekate MAX77621_CPU_CTRL{1,2}_HOS_DEFAULT (used for both rails):
/// active discharge, NFSR, sense, 12.5 mV/us; 120 C, watchdog,
/// 75 mV/us trip, nominal inductor.
const CTRL1_HOS: u8 = 1 << 4 | 1 << 5 | 1 << 7;
const CTRL2_HOS: u8 = 1 << 7 | 1 << 6 | 1;

/// Fuses at +0x800 (hekate bdk/soc/fuse.h).
const FUSE_SKU_INFO: usize = 0x800 + 0x110;
const FUSE_CPU_SPEEDO_2: usize = 0x800 + 0x130;
const FUSE_RESERVED_CALIB0: usize = 0x800 + 0x304;
const FUSE_SPARE_BIT: usize = 0x800 + 0x380;

/// What the fuses say about this GPU.
#[derive(Clone, Copy, Default)]
pub struct Chip {
    pub sku: u32,
    pub speedo: i32,
    /// Only the Switch's Erista: the PLL is programmed there alone.
    pub speedo_id: Option<usize>,
    pub calib: Option<Calib>,
}

pub fn chip(fuse: Option<&Mmio>) -> Chip {
    let Some(f) = fuse else {
        return Chip::default();
    };
    let spare = |b: usize| f.r32(FUSE_SPARE_BIT + 4 * b) & 1;
    let rev = spare(4) << 2 | spare(3) << 1 | spare(2);
    let sku = f.r32(FUSE_SKU_INFO) & 0xFF;
    Chip {
        sku,
        speedo: pll::gpu_speedo(rev, f.r32(FUSE_CPU_SPEEDO_2)),
        speedo_id: pll::gpu_speedo_id(sku),
        calib: pll::fused_calib(f.r32(FUSE_RESERVED_CALIB0)),
    }
}

fn io(r: core::result::Result<(), i64>) -> Result<()> {
    r.map_err(|e| (-e, "GPU regulator"))
}

/// Rail on at `uv` (hekate `_ccplex_enable_power_t210` for the GPU's
/// regulator): EN pin high, HOS control values, voltage, then enable.
pub fn rail_on(i2c: &I2c, uv: u32) -> Result<()> {
    let ame = i2c
        .read_reg(5, PMIC, PMIC_AME_GPIO)
        .map_err(|e| (-e, "PMIC GPIO"))?;
    io(i2c.write_reg(5, PMIC, PMIC_AME_GPIO, ame & !(1 << GPU_EN_GPIO)))?;
    io(i2c.write_reg(5, PMIC, PMIC_GPIO0 + GPU_EN_GPIO, GPIO_OUT_HIGH))?;
    io(i2c.write_reg(5, MAX77621_GPU, CONTROL1, CTRL1_HOS))?;
    io(i2c.write_reg(5, MAX77621_GPU, CONTROL2, CTRL2_HOS))?;
    let code = pll::rail_code(uv);
    let v = i2c
        .read_reg(5, MAX77621_GPU, VOUT)
        .map_err(|e| (-e, "GPU regulator"))?;
    let v = (v & VOUT_ENABLE) | code;
    io(i2c.write_reg(5, MAX77621_GPU, VOUT, v))?;
    io(i2c.write_reg(5, MAX77621_GPU, VOUT_DVS, VOUT_ENABLE | v))?;
    time::sleep_ms(1);
    io(i2c.write_reg(5, MAX77621_GPU, VOUT, VOUT_ENABLE | code))?;
    time::sleep_ms(1);
    Ok(())
}

/// Rail moved to `uv` while on (hekate `max7762x_regulator_set_voltage`):
/// VOUT, VOUT_DVS, 1 ms for the 12.5 mV/µs ramp, checked by reading back.
pub fn rail_set(i2c: &I2c, uv: u32) -> Result<()> {
    if uv > pll::MAX_UV {
        return Err((EIO, "GPU rail above its maximum"));
    }
    let code = pll::rail_code(uv);
    let v = i2c
        .read_reg(5, MAX77621_GPU, VOUT)
        .map_err(|e| (-e, "GPU regulator"))?;
    let v = (v & VOUT_ENABLE) | code;
    io(i2c.write_reg(5, MAX77621_GPU, VOUT, v))?;
    io(i2c.write_reg(5, MAX77621_GPU, VOUT_DVS, VOUT_ENABLE | v))?;
    time::sleep_ms(1);
    match rail_uv(i2c) {
        u if u == pll::rail_step(code) => Ok(()),
        _ => Err((EIO, "GPU rail did not take the voltage")),
    }
}

/// Rail off, EN pin low (hekate `_ccplex_disable_power`).
pub fn rail_off(i2c: &I2c) {
    if let Ok(v) = i2c.read_reg(5, MAX77621_GPU, VOUT) {
        let _ = i2c.write_reg(5, MAX77621_GPU, VOUT, v & !VOUT_ENABLE);
        time::sleep_ms(1);
    }
    let _ = i2c.write_reg(5, PMIC, PMIC_GPIO0 + GPU_EN_GPIO, 0);
}

/// Rail voltage read back (µV), 0 if off or unreadable.
pub fn rail_uv(i2c: &I2c) -> u32 {
    match i2c.read_reg(5, MAX77621_GPU, VOUT) {
        Ok(v) if v & VOUT_ENABLE != 0 => pll::rail_step(v & 0x7F),
        _ => 0,
    }
}

// GPCPLL registers (nouveau clk/gk20a.h, gm20b.c).
const CFG: u32 = 0x137000;
const COEFF: u32 = 0x137004;
const CFG2: u32 = 0x13700C;
const DVFS0: u32 = 0x137010;
const DVFS1: u32 = 0x137014;
const CFG3: u32 = 0x137018;
const NDIV_SLOWDOWN: u32 = 0x13701C;
const SEL_VCO: u32 = 0x137100;
const GPC2CLK_OUT: u32 = 0x137250;
const BYPASSCTRL_SYS: u32 = 0x137340;
const BCAST_DVFS2: u32 = 0x132820;
const BCAST_NDIV_SLOWDOWN_DEBUG: u32 = 0x1328A0;

const CFG_ENABLE: u32 = 1;
const CFG_IDDQ: u32 = 1 << 1;
const CFG_SYNC_MODE: u32 = 1 << 2;
const SDM_DIN_MASK: u32 = 0xFF;
const SDM_DIN_NEW_MASK: u32 = 0x7FFF << 8;
const EN_DFS: u32 = 1 << 29;
const EN_DFS_CAL: u32 = 1 << 30;
const DFS_CAL_DONE: u32 = 1 << 31;
const DFS_EXT_STROBE: u32 = 1 << 16;
const SLOWDOWN_USING_PLL: u32 = 1 << 22;
const EN_DYNRAMP: u32 = 1 << 31;
const DYNRAMP_DONE: u32 = 1 << 24;
/// VCO divider 1:2 while the PLL changes (halves the step to bypass).
const VCODIV_MASK: u32 = 0x3F << 8;
const VCODIV2: u32 = 2 << 8;

/// Programmed PLL, what it runs at and the detection it was set for.
pub struct Clock {
    pub khz: u32,
    pub mnp: Mnp,
    uv: i32,
    cal: Calib,
    d: pll::Dvfs,
}

/// GPCPLL out of IDDQ, calibrated for `uv` and set to `pll::GPC_KHZ`
/// (`gm20b_clk_init`, `_init_dvfs`, `_prog`). `None` leaves the GPU on
/// the bypass clock (not the Switch's Erista, or fuses unreadable).
pub fn clock(r: &Regs, c: &Chip, uv: u32) -> Result<Option<Clock>> {
    if c.speedo_id.is_none() {
        println!(
            "[gpu] SKU {:#04x} or fuses unreadable: GPCPLL left alone, GPU on its bypass clock",
            c.sku
        );
        return Ok(None);
    }
    r.mask(CFG, CFG_IDDQ, 0);
    r.rd(CFG);
    time::sleep_us(5);
    // gpc2clk out: VCO divider 1, bypass divider 31, SDIV14 on.
    r.mask(GPC2CLK_OUT, 1 << 31 | 0x3F << 8 | 0x3F, 1 << 31 | 0x3C);
    r.mask(BYPASSCTRL_SYS, 1, 0);
    // Slide steps for the 38.4 MHz reference (`gk20a_clk_setup_slide`).
    r.mask(CFG2, 0xFF << 24, 0x04 << 24);
    r.mask(CFG3, 0xFF << 16, 0x05 << 16);
    // RAM SVOP PDP fuse override when not fused; idle slowdown off.
    if r.rd(0x21944) & 3 == 0 {
        r.mask(0x21944, 0, 2);
        r.mask(0x21948, 0, 1);
    }
    r.mask(0x20160, 0x003F_0000, 0);

    // Noise-aware DVFS: detection pinned at the rail voltage.
    r.mask(DVFS1, 0, EN_DFS);
    r.mask(CFG3, 0x1FF, 0x7 << 3);
    r.mask(DVFS1, 0, EN_DFS_CAL);
    let uv = uv as i32;
    let cal = match c.calib {
        Some(k) => k,
        None => {
            r.wait(DVFS1, DFS_CAL_DONE, DFS_CAL_DONE, 10, "GPCPLL calibration")?;
            pll::measured_calib(uv, (r.rd(CFG3) >> 24) & 0x7F)
        }
    };
    let d = pll::det_coeff(uv, cal);
    program_coeff(r, 0);
    program_ext_cal(r, d.ext_cal);
    program_coeff(r, d.coeff);
    program_coeff(r, d.coeff);
    program_ext_cal(r, d.ext_cal);

    // Start at the bottom of the VCO range, then slide NDIV up
    // (`gm20b_pllg_program_mnp_slide`).
    let target = pll::calc(pll::GPC_KHZ);
    let lo = Mnp {
        n: target.n_lo(),
        ..target
    };
    program(r, lo, uv, cal, d)?;
    slide(r, target.n, uv, cal, d)?;
    let khz = target.khz();
    println!(
        "[gpu] GPCPLL M {} N {} PL {}: {} kHz (DFS coeff {}, ext cal {})",
        target.m, target.n, target.pl, khz, d.coeff, d.ext_cal
    );
    Ok(Some(Clock {
        khz,
        mnp: target,
        uv,
        cal,
        d,
    }))
}

impl Clock {
    /// `khz` with the rail at `uv` (`gm20b_clk_prog` inside the voltage
    /// steps `nvkm_cstate_prog` takes around it): rail up first, a PLL
    /// below both clocks while the detector moves to the new voltage,
    /// the new PLL, rail down last. The GPU must be idle.
    pub fn set(&mut self, r: &Regs, i2c: &I2c, khz: u32, uv: u32, safe_fmax: u32) -> Result<()> {
        let new = pll::calc(khz);
        let (old_uv, uv) = (self.uv, uv as i32);
        if uv > old_uv {
            rail_set(i2c, uv as u32)?;
        }
        if uv != old_uv {
            if self.khz > safe_fmax {
                let base = if uv > old_uv { self.mnp } else { new };
                let safe = pll::safe_pll(base, safe_fmax);
                program_mnp_slide(r, safe, self.uv, self.cal, self.d)?;
                self.mnp = safe;
                self.khz = safe.khz();
            }
            let d = pll::det_coeff(uv, self.cal);
            program_coeff(r, 0);
            program_ext_cal(r, d.ext_cal);
            program_coeff(r, d.coeff);
            program_detection(r, d);
            self.uv = uv;
            self.d = d;
        }
        program_mnp_slide(r, new, self.uv, self.cal, self.d)?;
        self.mnp = new;
        self.khz = new.khz();
        if uv < old_uv {
            rail_set(i2c, uv as u32)?;
        }
        Ok(())
    }
}

/// A slide when M and PL stay; otherwise down to NDIV_LO, reprogrammed
/// there and up (`gm20b_pllg_program_mnp_slide`).
fn program_mnp_slide(r: &Regs, p: Mnp, uv: i32, cal: Calib, d: pll::Dvfs) -> Result<()> {
    if r.rd(CFG) & CFG_ENABLE != 0 {
        let cur = Mnp::from_coeff(r.rd(COEFF));
        if p.m == cur.m && p.pl == cur.pl {
            return slide(r, p.n, uv, cal, d);
        }
        slide(r, cur.n_lo(), uv, cal, d)?;
    }
    program(r, Mnp { n: p.n_lo(), ..p }, uv, cal, d)?;
    slide(r, p.n, uv, cal, d)
}

/// `gm20b_dvfs_program_dfs_detection`: coefficient and maximum
/// detection, then the calibration.
fn program_detection(r: &Regs, d: pll::Dvfs) {
    r.mask(BCAST_DVFS2, 0, DFS_EXT_STROBE);
    r.mask(
        DVFS0,
        0x7F | 0x7F << 8,
        d.coeff | (d.det_max as u32 & 0x7F) << 8,
    );
    time::sleep_us(1);
    r.mask(BCAST_DVFS2, DFS_EXT_STROBE, 0);
    program_ext_cal(r, d.ext_cal);
}

/// `gm20b_dvfs_program_coeff`.
fn program_coeff(r: &Regs, coeff: u32) {
    r.mask(BCAST_DVFS2, 0, DFS_EXT_STROBE);
    r.mask(DVFS0, 0x7F | 0x7F << 8, coeff);
    time::sleep_us(1);
    r.mask(BCAST_DVFS2, DFS_EXT_STROBE, 0);
}

/// `gm20b_dvfs_program_ext_cal`: the detector told the rail's voltage.
fn program_ext_cal(r: &Regs, cal: i32) {
    r.mask(BCAST_DVFS2, 0x7F, cal as u32 & 0x7F);
    time::sleep_us(1);
    let v = r.rd(DVFS1);
    if v & 1 << 25 == 0 {
        r.wr(DVFS1, v | 1 << 25 | 1 << 16);
    }
}

fn write_mnp(r: &Regs, p: Mnp, sdm_din: u32) {
    r.mask(CFG2, SDM_DIN_MASK, sdm_din);
    r.mask(COEFF, 0x3F_FFFF, p.coeff());
}

fn vcodiv(r: &Regs, v: u32) {
    // Written twice for the divider's linear operation.
    r.mask(GPC2CLK_OUT, VCODIV_MASK, v);
    r.mask(GPC2CLK_OUT, VCODIV_MASK, v);
    r.rd(GPC2CLK_OUT);
}

/// Full reprogram from bypass (`gm20b_pllg_program_mnp`, PLL disabled).
fn program(r: &Regs, p: Mnp, uv: i32, cal: Calib, d: pll::Dvfs) -> Result<()> {
    let (n, sdm) = pll::ndiv(p.n, uv, cal, d);
    vcodiv(r, VCODIV2);
    time::sleep_us(2);
    r.mask(SEL_VCO, 1, 0);
    r.mask(CFG, CFG_SYNC_MODE, 0);
    r.mask(CFG, CFG_ENABLE, 0);
    r.rd(CFG);
    write_mnp(r, Mnp { n, ..p }, sdm);
    r.mask(CFG, 0, CFG_ENABLE);
    r.rd(CFG);
    // In DVFS mode lock detection cannot be used: a fixed delay.
    time::sleep_us(40);
    r.mask(CFG, 0, CFG_SYNC_MODE);
    r.rd(CFG);
    r.mask(SEL_VCO, 0, 1);
    time::sleep_us(2);
    vcodiv(r, 0);
    Ok(())
}

/// NDIV ramp without leaving VCO mode (`gm20b_pllg_slide`).
fn slide(r: &Regs, n_eff: u32, uv: i32, cal: Calib, d: pll::Dvfs) -> Result<()> {
    let (n, sdm) = pll::ndiv(n_eff, uv, cal, d);
    let cur = Mnp::from_coeff(r.rd(COEFF));
    if cur.n == n && r.rd(CFG2) & SDM_DIN_MASK == sdm {
        return Ok(());
    }
    r.mask(NDIV_SLOWDOWN, 0, SLOWDOWN_USING_PLL);
    r.mask(CFG2, SDM_DIN_NEW_MASK, sdm << 8);
    time::sleep_us(1);
    r.mask(COEFF, 0x3F_FFFF, Mnp { n, ..cur }.coeff());
    time::sleep_us(1);
    r.mask(NDIV_SLOWDOWN, 0, EN_DYNRAMP);
    let ramp = r.wait(
        BCAST_NDIV_SLOWDOWN_DEBUG,
        DYNRAMP_DONE,
        DYNRAMP_DONE,
        500,
        "GPCPLL ramp",
    );
    r.mask(CFG2, SDM_DIN_MASK, sdm);
    r.mask(NDIV_SLOWDOWN, SLOWDOWN_USING_PLL | EN_DYNRAMP, 0);
    r.rd(NDIV_SLOWDOWN);
    ramp
}

/// Back to bypass before power-down: slide to the VCO minimum, bypass,
/// disable, IDDQ (`gm20b_clk_fini`).
pub fn clock_off(r: &Regs, c: &Clock) {
    let _ = slide(r, c.mnp.n_lo(), c.uv, c.cal, c.d);
    r.mask(SEL_VCO, 1, 0);
    r.mask(CFG, CFG_SYNC_MODE, 0);
    r.mask(CFG, CFG_ENABLE, 0);
    r.rd(CFG);
    r.mask(CFG, 0, CFG_IDDQ);
}

/// GPU clocks up and unclamped through `clk`, then the chip ID read:
/// 0x12B000A1 on GM20B (T210), 0x12E000A1 on the T210B01.
pub fn identify(r: &Regs) -> Result<u32> {
    let id = r.rd(0);
    match id >> 20 & 0x1FF {
        0x12B => Ok(id),
        0 | 0x1FF => Err((EIO, "GPU registers do not answer")),
        _ => Err((ENODEV, "not a GM20B")),
    }
}

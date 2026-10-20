//! The switch sequence, step for step as Minerva's `_minerva_change_clock`
//! with `needs_training == 0` (comments keep its step numbers).

use crate::layout as L;
use crate::regs::*;
use crate::{Bank, Error, Hw, Table};

const CH1: u32 = 0x4000;

/// EMC_2X_CLK_SRC values (CLK_SOURCE_EMC bits 31:29).
const PLLM_OUT0: u32 = 0;
const PLLP_OUT0: u32 = 2;
const CLK_M: u32 = 3;
const PLLM_UD: u32 = 4;
const PLLMB_UD: u32 = 5;
const PLLMB_OUT0: u32 = 6;
const PLLP_UD: u32 = 7;

const ONE_RANK: u32 = 1;
const TWO_RANK: u32 = 2;

const OSC_KHZ: u32 = 38_400;
const PLLP_KHZ: u32 = 408_000;
/// Polls of a register before giving up on a wait Minerva never bounds.
const SPIN_LIMIT: u32 = 200_000;
/// Minerva's `EMC_STATUS_UPDATE_TIMEOUT`, in 1 us polls.
const STATUS_LIMIT: u32 = 2000;

const BURST: usize = L::BURST_REGS_EMC_RC;
const BURST_PER_CH: usize = L::BURST_REG_PER_CH_EMC0_MRW10;
const TRIM: usize = L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_0;
const TRIM_PER_CH: usize = L::TRIM_PERCH_REGS_EMC_CMD_BRLSHFT_0;
const VREF: usize = L::VREF_PERCH_REGS_EMC0_TRAINING_OPT_DQS_IB_VREF_RANK0;

fn e_rd<H: Hw>(h: &mut H, off: u32) -> u32 {
    h.rd(Bank::Emc, off)
}

fn e_wr<H: Hw>(h: &mut H, off: u32, v: u32) {
    h.wr(Bank::Emc, off, v)
}

fn c_rd<H: Hw>(h: &mut H, off: u32) -> u32 {
    h.rd(Bank::Car, off)
}

fn c_wr<H: Hw>(h: &mut H, off: u32, v: u32) {
    h.wr(Bank::Car, off, v)
}

fn m_rd<H: Hw>(h: &mut H, off: u32) -> u32 {
    h.rd(Bank::Mc, off)
}

fn m_wr<H: Hw>(h: &mut H, off: u32, v: u32) {
    h.wr(Bank::Mc, off, v)
}

/// Read-modify-write: one read, one write.
fn e_rmw<H: Hw>(h: &mut H, off: u32, f: impl FnOnce(u32) -> u32) {
    let v = e_rd(h, off);
    e_wr(h, off, f(v));
}

fn c_rmw<H: Hw>(h: &mut H, off: u32, f: impl FnOnce(u32) -> u32) {
    let v = c_rd(h, off);
    c_wr(h, off, f(v));
}

/// Poll until `done` (bounded, no delay between reads).
fn spin<H: Hw>(
    h: &mut H,
    what: &'static str,
    mut done: impl FnMut(&mut H) -> bool,
) -> Result<(), Error> {
    for _ in 0..SPIN_LIMIT {
        if done(h) {
            return Ok(());
        }
    }
    Err(Error::Timeout(what))
}

fn div_o3(a: u32, b: u32) -> u32 {
    let r = a / b;
    if b.wrapping_mul(r) < a {
        r + 1
    } else {
        r
    }
}

/// Queue a register write in the clock-change FIFO (runs in the stall).
fn ccfifo<H: Hw>(h: &mut H, addr: u32, data: u32, delay: u32) {
    e_wr(h, EMC_CCFIFO_DATA, data);
    e_wr(
        h,
        EMC_CCFIFO_ADDR,
        (1 << 31) | (addr & 0xFFFF) | ((delay & 0x7FFF) << 16),
    );
}

/// `true` on timeout, as Minerva's `_wait_emc_status`.
fn wait_emc_status<H: Hw>(h: &mut H, off: u32, mask: u32, state: bool, channel: u32) -> bool {
    for _ in 0..STATUS_LIMIT {
        let r = match channel {
            0 => e_rd(h, off),
            1 => e_rd(h, CH1 + off),
            _ => return true,
        };
        if ((r & mask) != 0) == state {
            return false;
        }
        h.usleep(1);
    }
    true
}

fn timing_update<H: Hw>(h: &mut H, dual: bool) -> bool {
    e_wr(h, EMC_TIMING_CONTROL, 1);
    let mut err = wait_emc_status(h, EMC_EMC_STATUS, TIMING_UPDATE_STALLED, false, 0);
    if dual {
        err |= wait_emc_status(h, EMC_EMC_STATUS, TIMING_UPDATE_STALLED, false, 1);
    }
    err
}

/// Both channels in use (FBIO_CFG7 bits 1 and 2).
fn dual_channel<H: Hw>(h: &mut H) -> bool {
    let a = e_rd(h, EMC_FBIO_CFG7) >> 1;
    let b = (e_rd(h, EMC_FBIO_CFG7) >> 2) & 1;
    a & b & 1 != 0
}

fn dll_wait_en<H: Hw>(h: &mut H, dual: bool, en: bool) -> Result<(), Error> {
    let want = en as u32;
    spin(h, "dll", |h| e_rd(h, EMC_CFG_DIG_DLL) & 1 == want)?;
    if dual {
        spin(h, "dll ch1", |h| e_rd(h, CH1 + EMC_CFG_DIG_DLL) & 1 == want)?;
    }
    Ok(())
}

fn digital_dll_disable<H: Hw>(h: &mut H) -> Result<(), Error> {
    let dual = dual_channel(h);
    e_rmw(h, EMC_CFG_DIG_DLL, |v| v & 0xFFFF_FFFE);
    timing_update(h, dual);
    dll_wait_en(h, dual, false)
}

fn digital_dll_enable_rs<H: Hw>(h: &mut H, dual: bool) -> Result<(), Error> {
    e_rmw(h, EMC_CFG_DIG_DLL, |v| (v & 0xFFFF_FF24) | 0x89);
    timing_update(h, dual);
    dll_wait_en(h, dual, true)
}

fn change_dll_src<H: Hw>(h: &mut H, dst: &Table, clk_src_emc: u32) {
    let src2x = clk_src_emc >> 29;
    let mut dll = ((((dst.w(L::DLL_CLK_SRC) & 0x1FFF_FFFF) | (src2x << 29)) & 0xFFFF_FF00)
        | (clk_src_emc & 0xFF))
        & 0xFFFF_F3FF;
    if src2x == PLLMB_UD {
        dll |= EMC_DLL_PLLM_VCOB;
    } else if src2x != PLLM_UD {
        dll |= EMC_DLL_SWITCH_OUT;
    }
    c_wr(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC_DLL, dll);
    c_rd(h, CLK_RST_CONTROLLER_CLK_OUT_ENB_X);
    h.usleep(2);
    if dst.w(L::CLK_OUT_ENB_X_0_CLK_ENB_EMC_DLL) != 0 {
        c_wr(h, CLK_RST_CONTROLLER_CLK_ENB_X_SET, 1 << 14);
    } else {
        c_wr(h, CLK_RST_CONTROLLER_CLK_ENB_X_CLR, 1 << 14);
    }
    c_rd(h, CLK_RST_CONTROLLER_CLK_OUT_ENB_X);
    h.usleep(2);
}

fn digital_dll_prelock<H: Hw>(h: &mut H, dst: &Table, sel: u32) -> Result<(), Error> {
    let dual = dual_channel(h);
    e_rmw(h, EMC_CFG_DIG_DLL, |v| (v & 0xFFFF_F824) | 0x3C8);
    timing_update(h, dual);
    dll_wait_en(h, dual, false)?;
    e_wr(h, EMC_DLL_CFG_0, dst.w(L::BURST_REGS_EMC_DLL_CFG_0));
    e_wr(h, EMC_DLL_CFG_1, dst.w(L::BURST_REGS_EMC_DLL_CFG_1));
    change_dll_src(h, dst, sel);
    e_rmw(h, EMC_CFG_DIG_DLL, |v| v | 1);
    timing_update(h, dual);
    dll_wait_en(h, dual, true)?;
    spin(h, "dll lock", |h| {
        let a = ((e_rd(h, EMC_DIG_DLL_STATUS) >> 17) & 1) ^ 1;
        let b = ((e_rd(h, EMC_DIG_DLL_STATUS) >> 15) & 1) ^ 1;
        (a | b) == 0
    })?;
    // Minerva returns the status; the read stays.
    e_rd(h, EMC_DIG_DLL_STATUS);
    Ok(())
}

/// Step 11's pad ramp-down; returns the wait in ps.
fn ramp_down<H: Hw>(h: &mut H, src: &Table, dst: &Table, src_period: u32) -> u32 {
    let per_pc = 100_000 / src_period + 1;
    let cmd_pad = src.w(L::BURST_REGS_EMC_PMACRO_CMD_PAD_TX_CTRL);
    let mut dq_pad = (dst.w(L::BURST_REGS_EMC_PMACRO_DATA_PAD_TX_CTRL) & 0x101)
        | src.w(L::BURST_REGS_EMC_PMACRO_DATA_PAD_TX_CTRL);
    let rfu1 = src.w(L::BURST_REGS_EMC_PMACRO_BRICK_CTRL_RFU1);
    let cfg5 = src.w(L::BURST_REGS_EMC_FBIO_CFG5);
    let com_pad = src.w(L::BURST_REGS_EMC_PMACRO_COMMON_PAD_TX_CTRL);

    let mut cmd_drv = cmd_pad | 0x0400_0000;
    let mut wait = src_period.wrapping_mul(12);

    ccfifo(h, EMC_PMACRO_CMD_PAD_TX_CTRL, cmd_drv, 0);
    ccfifo(h, EMC_FBIO_CFG5, cfg5 | 0x100, 12);

    if src_period >= 1000 {
        ccfifo(
            h,
            EMC_PMACRO_BRICK_CTRL_RFU1,
            rfu1 & 0xF800_F800,
            per_pc + 19,
        );
        wait = wait
            .wrapping_add(100_000)
            .wrapping_add(src_period.wrapping_mul(20));
    } else {
        if src_period >= 416 {
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFEED_FEED, per_pc);
        } else {
            dq_pad = (dq_pad & 0xFEFE_FDFD) | 0x10200;
            cmd_drv = (cmd_pad & 0xFAFE_FDFD) | 0x0401_0200;
            ccfifo(h, EMC_PMACRO_CMD_PAD_TX_CTRL, cmd_drv, per_pc);
            ccfifo(h, EMC_PMACRO_DATA_PAD_TX_CTRL, dq_pad, 0);
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFEED_FEED, 0);
        }
        wait = wait.wrapping_add(300_000);
        ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFE40_FE40, per_pc);
        if src_period >= 416 {
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xF800_F800, per_pc);
        } else {
            ccfifo(h, EMC_PMACRO_CMD_PAD_TX_CTRL, cmd_drv & 0xFEFE_FDFD, per_pc);
            ccfifo(h, EMC_PMACRO_DATA_PAD_TX_CTRL, dq_pad & 0xFEFE_FDFD, 0);
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xF800_F800, 0);
        }
    }

    if src_period >= 1666 {
        ccfifo(
            h,
            EMC_PMACRO_COMMON_PAD_TX_CTRL,
            com_pad & 0xFFFF_FFF0,
            per_pc,
        );
    } else {
        wait = wait.wrapping_add(400_000);
        ccfifo(
            h,
            EMC_PMACRO_COMMON_PAD_TX_CTRL,
            com_pad & 0xFFFF_FFFA,
            per_pc,
        );
        ccfifo(
            h,
            EMC_PMACRO_COMMON_PAD_TX_CTRL,
            com_pad & 0xFFFF_FFF0,
            per_pc,
        );
        ccfifo(h, EMC_INTSTATUS, 0, per_pc);
    }
    wait
}

/// Step 13's pad ramp-up; returns the wait in ps.
fn ramp_up<H: Hw>(h: &mut H, dst: &Table, dst_period: u32) -> u32 {
    let per_pc = 100_000 / dst_period + 1;
    let cmd_pad = dst.w(L::BURST_REGS_EMC_PMACRO_CMD_PAD_TX_CTRL);
    let dq_pad = dst.w(L::BURST_REGS_EMC_PMACRO_DATA_PAD_TX_CTRL);
    let rfu1 = dst.w(L::BURST_REGS_EMC_PMACRO_BRICK_CTRL_RFU1);
    let cfg5 = dst.w(L::BURST_REGS_EMC_FBIO_CFG5);
    let com_pad = dst.w(L::BURST_REGS_EMC_PMACRO_COMMON_PAD_TX_CTRL);
    let mut cmd_data = (cmd_pad & 0xFAFE_FDFD) | 0x0400_0000;
    let wait;

    if dst_period >= 1666 {
        ccfifo(h, EMC_PMACRO_COMMON_PAD_TX_CTRL, com_pad | 8, 0);
        ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 | 0x600, 0);
        ccfifo(h, EMC_FBIO_CFG5, cfg5 & 0xFFFF_FEFF, 12);
        wait = dst_period.wrapping_mul(12);
    } else {
        ccfifo(h, EMC_PMACRO_COMMON_PAD_TX_CTRL, com_pad & 0xA, 0);
        ccfifo(h, EMC_PMACRO_COMMON_PAD_TX_CTRL, com_pad & 0xF, per_pc);
        if dst_period < 1000 {
            if dst_period >= 416 {
                ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFE40_FE40, per_pc);
            } else {
                ccfifo(
                    h,
                    EMC_PMACRO_CMD_PAD_TX_CTRL,
                    (cmd_pad & 0xFAFE_FDFD) | 0x0401_0200,
                    per_pc,
                );
                ccfifo(
                    h,
                    EMC_PMACRO_DATA_PAD_TX_CTRL,
                    (dq_pad & 0xFEFE_FDFD) | 0x10200,
                    0,
                );
                ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFE40_FE40, 0);
            }
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 & 0xFEED_FEED, per_pc);
            if dst_period >= 416 {
                ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1, per_pc);
            } else {
                cmd_data = cmd_pad | 0x0501_0202;
                ccfifo(h, EMC_PMACRO_CMD_PAD_TX_CTRL, cmd_data, per_pc);
                ccfifo(h, EMC_PMACRO_DATA_PAD_TX_CTRL, dq_pad | 0x0101_0202, 0);
                ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1, 0);
            }
            wait = 500_000u32.wrapping_add(dst_period.wrapping_mul(10));
        } else {
            ccfifo(h, EMC_PMACRO_BRICK_CTRL_RFU1, rfu1 | 0x0600_0600, per_pc);
            wait = 200_000u32.wrapping_add(dst_period.wrapping_mul(10));
        }
        ccfifo(h, EMC_FBIO_CFG5, cfg5 & 0xFFFF_FEFF, per_pc + 9);
    }
    ccfifo(h, EMC_PMACRO_CMD_PAD_TX_CTRL, cmd_data & 0xFBFF_FFFF, 5);
    wait
}

/// Minerva's `_minerva_change_clock` for a plain frequency switch.
fn change_clock<H: Hw>(h: &mut H, src: &Table, dst: &Table, sel_clk_src: u32) -> Result<(), Error> {
    let zcal_shared = (src.w(L::BURST_REGS_EMC_ZCAL_WAIT_CNT) >> 31) & 1 != 0;
    let enable_bg = (dst.w(L::BURST_REGS_EMC_PMACRO_BG_BIAS_CTRL_0) & 1) ^ 1 != 0;
    let ch1 = (src.w(L::BURST_REGS_EMC_FBIO_CFG7) >> 2) & 1 != 0;
    let dram_type = e_rd(h, EMC_FBIO_CFG5) & 3;
    let dev_num = (m_rd(h, MC_EMEM_ADR_CFG) & 1) + 1;
    let src_period = 1_000_000_000 / src.rate_khz();
    let dst_period = 1_000_000_000 / dst.rate_khz();
    let fsp_opwr = e_rd(h, EMC_MRW3) & 0xC0 == 0;

    if dram_type != 1 {
        return Err(Error::NotLpddr4);
    }

    let t_fc = dst.w(L::DRAM_TIMINGS_T_FC_LPDDR4).wrapping_mul(1000);
    let mut t_zqcal = 1_000_000u32;
    if dst_period <= 2000 {
        t_zqcal = t_zqcal.wrapping_sub(t_fc);
    }
    let zqcal_fc_adj = (t_zqcal / dst_period) as i32;

    e_rd(h, EMC_CFG);
    e_rd(h, EMC_AUTO_CAL_CONFIG);

    // Step 1 - Pre DVFS SW sequence.
    let dbg_o = e_rd(h, EMC_DBG);
    let pin_o = e_rd(h, EMC_PIN);
    let emc_cfg = dst.w(L::BURST_REGS_EMC_CFG) & 0x0FFF_FFFF;
    let sel_dpd = dst.w(L::EMC_SEL_DPD_CTRL) & 0xFFFF_FEC3;
    let pipe_clk_o = e_rd(h, EMC_CFG_PIPE_CLK);
    digital_dll_disable(h)?;

    // Step 1.2 - Disable AUTOCAL temporarily.
    e_wr(
        h,
        EMC_AUTO_CAL_CONFIG,
        (dst.w(L::EMC_AUTO_CAL_CONFIG) & 0x7FFF_F9FF) | 0x600,
    );
    e_rd(h, EMC_AUTO_CAL_CONFIG);

    // Step 1.3 - Disable other power features.
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(h, EMC_CFG, emc_cfg);
    e_wr(h, EMC_SEL_DPD_CTRL, sel_dpd);
    e_wr(h, EMC_DBG, dbg_o);

    e_wr(h, EMC_INTSTATUS, CLKCHANGE_COMPLETE_INT);
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(h, EMC_CFG, emc_cfg);
    e_wr(h, EMC_SEL_DPD_CTRL, sel_dpd);
    e_wr(h, EMC_CFG_PIPE_CLK, pipe_clk_o | 1);
    e_wr(
        h,
        EMC_FDPD_CTRL_CMD_NO_RAMP,
        dst.w(L::EMC_FDPD_CTRL_CMD_NO_RAMP) & 0xFFFF_FFFE,
    );

    let mut bg_change = src.w(L::BURST_REGS_EMC_PMACRO_BG_BIAS_CTRL_0)
        ^ dst.w(L::BURST_REGS_EMC_PMACRO_BG_BIAS_CTRL_0);
    bg_change = (bg_change | (bg_change >> 2)) & 1;
    let src_bg = src.w(L::BURST_REGS_EMC_PMACRO_BG_BIAS_CTRL_0);
    let dst_bg = dst.w(L::BURST_REGS_EMC_PMACRO_BG_BIAS_CTRL_0);
    if bg_change != 0 {
        e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
        if enable_bg {
            e_wr(h, EMC_PMACRO_BG_BIAS_CTRL_0, src_bg & 0xFFFF_FFFE);
        } else {
            e_wr(h, EMC_PMACRO_BG_BIAS_CTRL_0, src_bg & 0xFFFF_FFFB);
        }
    }

    // Check if we need to turn on VREF generator.
    let sp = src.w(L::BURST_REGS_EMC_PMACRO_DATA_PAD_TX_CTRL);
    let dp = dst.w(L::BURST_REGS_EMC_PMACRO_DATA_PAD_TX_CTRL);
    if (sp & 0x100 == 0 && dp & 0x100 != 0) || (sp & 1 == 0 && dp & 1 != 0) {
        e_wr(
            h,
            EMC_PMACRO_DATA_PAD_TX_CTRL,
            (((dp & 1) | (sp & 0xFFFF_FFFE)) & 0xFFFF_FEFF) | (((dp >> 8) & 1) << 8),
        );
    }
    h.usleep(1);
    e_wr(h, EMC_DBG, dbg_o);

    // Step 2 - Prelock the DLL.
    if dst.w(L::BURST_REGS_EMC_CFG_DIG_DLL) & 1 != 0 {
        digital_dll_prelock(h, dst, sel_clk_src)?;
    } else {
        change_dll_src(h, dst, sel_clk_src);
        digital_dll_disable(h)?;
    }

    // Step 3 - Prepare autocal for the clock change.
    e_wr(
        h,
        EMC_AUTO_CAL_CONFIG,
        (dst.w(L::EMC_AUTO_CAL_CONFIG) & 0x7FFF_F9FF) | 0x600,
    );
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(h, EMC_AUTO_CAL_CONFIG2, dst.w(L::EMC_AUTO_CAL_CONFIG2));
    e_wr(h, EMC_AUTO_CAL_CONFIG3, dst.w(L::EMC_AUTO_CAL_CONFIG3));
    e_wr(h, EMC_AUTO_CAL_CONFIG4, dst.w(L::EMC_AUTO_CAL_CONFIG4));
    e_wr(h, EMC_AUTO_CAL_CONFIG5, dst.w(L::EMC_AUTO_CAL_CONFIG5));
    e_wr(h, EMC_AUTO_CAL_CONFIG6, dst.w(L::EMC_AUTO_CAL_CONFIG6));
    e_wr(h, EMC_AUTO_CAL_CONFIG7, dst.w(L::EMC_AUTO_CAL_CONFIG7));
    e_wr(h, EMC_AUTO_CAL_CONFIG8, dst.w(L::EMC_AUTO_CAL_CONFIG8));
    e_wr(h, EMC_DBG, dbg_o);
    e_wr(
        h,
        EMC_AUTO_CAL_CONFIG,
        (dst.w(L::EMC_AUTO_CAL_CONFIG) & 0x7FFF_F9FE) | 0x601,
    );

    // Step 4 - Update EMC_CFG.
    if src_period <= 50_000 {
        e_wr(h, EMC_CFG_2, dst.w(L::EMC_CFG_2));
    } else {
        ccfifo(h, EMC_SELF_REF, 1, 0);
    }

    // Step 7 - Bug 200024907 - Patch RP R2P.
    let mut n_rtp = 8;
    // Minerva chains four comparisons, 266 to 1066 MHz; the last implies the others.
    if src_period < 1_000_000 / 1066 {
        n_rtp = 10;
    }
    if src_period < 1_000_000 / 1333 {
        n_rtp = 12;
    }
    if src_period < 1_000_000 / 1600 {
        n_rtp = 14;
    }
    if src_period < 1_000_000 / 1866 {
        n_rtp = 16;
    }
    let t_rpst = (src.w(L::EMC_MRW) >> 7) & 1;
    let mut delta_twatm = div_o3(7500, src_period);
    if delta_twatm < 8 {
        delta_twatm = 8;
    }
    let s_rp = src.w(L::BURST_REGS_EMC_RP);
    let s_r2p = src.w(L::BURST_REGS_EMC_R2P);
    let s_w2p = src.w(L::BURST_REGS_EMC_W2P);
    let s_trpab = src.w(L::BURST_REGS_EMC_TRPAB);
    let t_rtm = src
        .w(L::DRAM_TIMINGS_RL)
        .wrapping_add(div_o3(3600, src_period))
        .wrapping_add(delta_twatm)
        .wrapping_add(t_rpst)
        .wrapping_add(n_rtp)
        .wrapping_add(1);

    let mut r2p_war;
    let mut trpab_war;
    let mut rp_war;
    if t_rtm <= s_rp.wrapping_add(s_r2p) {
        trpab_war = s_trpab;
        r2p_war = s_r2p;
        rp_war = s_rp;
    } else {
        r2p_war = t_rtm.wrapping_sub(s_rp);
        trpab_war = s_trpab;
        rp_war = s_rp;
        if r2p_war > 63 {
            rp_war = t_rtm.wrapping_sub(63);
            r2p_war = 63;
            trpab_war = if s_trpab < t_rtm.wrapping_sub(63) {
                t_rtm.wrapping_sub(63)
            } else {
                s_trpab
            };
        }
    }
    let mut w2p_war;
    if rp_war >= delta_twatm {
        w2p_war = s_w2p;
    } else {
        let tmp = delta_twatm.wrapping_add(s_w2p);
        w2p_war = tmp.wrapping_sub(rp_war);
        if w2p_war > 63 {
            rp_war = tmp.wrapping_sub(63);
            w2p_war = 63;
            if trpab_war < rp_war {
                trpab_war = rp_war;
            }
        }
    }
    if s_w2p != w2p_war || s_rp != rp_war || s_r2p != r2p_war || s_trpab != trpab_war {
        e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
        e_wr(h, EMC_RP, rp_war);
        e_wr(h, EMC_R2P, r2p_war);
        e_wr(h, EMC_W2P, w2p_war);
        e_wr(h, EMC_TRPAB, trpab_war);
        e_wr(h, EMC_DBG, dbg_o);
        e_rd(h, EMC_TRPAB);
        h.usleep(1);
    }

    // Step 7.2 - Program FSP reference registers and send MRWs to new FSPWR.
    let dst_mrw3 = dst.w(L::EMC_MRW3);
    let (flip_fspop, flip_fspwr);
    if fsp_opwr {
        flip_fspop = dst_mrw3 | 0xC0;
        flip_fspwr = (dst_mrw3 & 0xFFFF_FF3F) | 0x40;
    } else {
        flip_fspop = dst_mrw3 & 0xFFFF_FF3F;
        flip_fspwr = flip_fspop | 0x80;
    }
    e_wr(h, EMC_MRW3, flip_fspwr);
    e_wr(h, EMC_MRW, dst.w(L::EMC_MRW));
    e_wr(h, EMC_MRW2, dst.w(L::EMC_MRW2));

    // Step 8 - Program the shadow registers.
    for (i, &addr) in BURST_REGS_EMC_ADDR_TABLE
        .iter()
        .enumerate()
        .take(dst.w(L::NUM_BURST) as usize)
    {
        let mut val = dst.w(BURST + i);
        match addr {
            EMC_CFG => val &= 0x0FFF_FFFF,
            EMC_ZCAL_INTERVAL => val = 0,
            EMC_PMACRO_BRICK_CTRL_RFU1 => val &= 0xF800_F800,
            EMC_PMACRO_CMD_PAD_TX_CTRL => val = (val & 0xFAFE_FDFD) | (1 << 26),
            EMC_PMACRO_DATA_PAD_TX_CTRL => val &= 0xFEFE_FDFD,
            EMC_PMACRO_COMMON_PAD_TX_CTRL => val &= 0xFFFF_FFF0,
            EMC_PMACRO_AUTOCAL_CFG_COMMON => val |= 1 << 16,
            EMC_TRAINING_CTRL => {}
            _ => {}
        }
        e_wr(h, addr, val);
    }
    e_wr(h, EMC_MRW, (dst.w(L::RUN_CLOCKS) & 0xFF) | 0x17_0000);

    let n = dst.w(L::NUM_BURST_PER_CH) as usize;
    for (i, &addr) in BURST_REG_PER_CH_EMC01_ADDR_TABLE.iter().enumerate().take(n) {
        if addr != 0 && (ch1 || addr.wrapping_sub(0x4000) > 0xFFF) {
            e_wr(h, addr, dst.w(BURST_PER_CH + i));
        }
    }
    for (i, &addr) in VREF_PERCH_REGS_EMC01_ADDR_TABLE
        .iter()
        .enumerate()
        .take(dst.w(L::VREF_NUM) as usize)
    {
        if addr != 0 && (ch1 || addr.wrapping_sub(0x4000) > 0xFFF) {
            e_wr(h, addr, dst.w(VREF + i));
        }
    }
    for (i, &addr) in TRIM_REGS_EMC_ADDR_TABLE
        .iter()
        .enumerate()
        .take(dst.w(L::NUM_TRIM) as usize)
    {
        if addr != 0 {
            e_wr(h, addr, dst.w(TRIM + i));
        }
    }
    let n = dst.w(L::NUM_TRIM_PER_CH) as usize;
    for (i, &addr) in TRIM_PERCH_REGS_EMC01_ADDR_TABLE.iter().enumerate().take(n) {
        if addr != 0 && (ch1 || addr.wrapping_sub(0x4000) > 0xFFF) {
            e_wr(h, addr, dst.w(TRIM_PER_CH + i));
        }
    }
    for (i, &addr) in BURST_MC_REGS_ADDR_TABLE
        .iter()
        .enumerate()
        .take(dst.w(L::NUM_MC_REGS) as usize)
    {
        m_wr(h, addr, dst.w(L::BURST_MC_REGS + i));
    }
    if dst.rate_khz() < src.rate_khz() {
        let n = dst.w(L::NUM_UP_DOWN) as usize;
        for (i, &addr) in LA_SCALE_REGS_MC_ADDR_TABLE.iter().enumerate().take(n) {
            m_wr(h, addr, dst.w(L::LA_SCALE_REGS + i));
        }
    }

    // Step 9 - LPDDR4.
    e_wr(
        h,
        EMC_ZCAL_INTERVAL,
        src.w(L::BURST_REGS_EMC_ZCAL_INTERVAL) & 0xFF00_0000,
    );
    e_wr(
        h,
        EMC_ZCAL_WAIT_CNT,
        dst.w(L::BURST_REGS_EMC_ZCAL_WAIT_CNT) & 0xFFFF_F800,
    );
    e_wr(
        h,
        EMC_DBG,
        dbg_o | EMC_DBG_WRITE_ACTIVE_ONLY | EMC_DBG_WRITE_MUX_ACTIVE,
    );
    e_wr(
        h,
        EMC_ZCAL_INTERVAL,
        src.w(L::BURST_REGS_EMC_ZCAL_INTERVAL) & 0xFF00_0000,
    );
    e_wr(h, EMC_DBG, dbg_o);

    // Step 10 - Self refresh.
    ccfifo(h, EMC_SELF_REF, 0x101, 0);
    if dst_period <= 2000 {
        ccfifo(h, EMC_MRW3, flip_fspwr ^ 0x40, 0);
        ccfifo(
            h,
            EMC_MRW6,
            (src.w(L::BURST_REGS_EMC_MRW6) & 0xC0C0)
                | (dst.w(L::BURST_REGS_EMC_MRW6) & 0xFFFF_3F3F),
            0,
        );
        ccfifo(
            h,
            EMC_MRW14,
            (src.w(L::BURST_REGS_EMC_MRW14) & 0x3838)
                | (dst.w(L::BURST_REGS_EMC_MRW14) & 0xFFFF_0707),
            0,
        );
        if dev_num == TWO_RANK {
            ccfifo(
                h,
                EMC_MRW7,
                (src.w(L::BURST_REGS_EMC_MRW7) & 0xC0C0)
                    | (dst.w(L::BURST_REGS_EMC_MRW7) & 0xFFFF_3F3F),
                0,
            );
            ccfifo(
                h,
                EMC_MRW15,
                (src.w(L::BURST_REGS_EMC_MRW15) & 0x3838)
                    | (dst.w(L::BURST_REGS_EMC_MRW15) & 0xFFFF_0707),
                0,
            );
        }
        let zq = if dev_num == ONE_RANK || zcal_shared {
            0x8000_0001
        } else {
            1
        };
        ccfifo(h, EMC_ZQ_CAL, zq, 0);
    }

    let t_rp_src = src.w(L::DRAM_TIMINGS_T_RP).wrapping_mul(1000) / src_period;
    let dbg_val = dbg_o;
    ccfifo(h, EMC_MRW3, flip_fspop | 8, t_rp_src);
    let ref_delay = t_fc / src_period;
    ccfifo(h, EMC_INTSTATUS, 0, ref_delay);
    ccfifo(h, EMC_PIN, pin_o & 0xFFFF_FFF8, 30);

    // Step 11 - Ramp down.
    ccfifo(h, EMC_CFG_SYNC, 0, 0);
    ccfifo(
        h,
        EMC_DBG,
        dbg_val | EMC_DBG_WRITE_ACTIVE_ONLY | EMC_DBG_WRITE_MUX_ACTIVE,
        0,
    );
    let ramp_down_wait = ramp_down(h, src, dst, src_period);

    // Step 12 - Trigger clock change.
    ccfifo(h, EMC_STALL_THEN_EXE_AFTER_CLKCHANGE, 1, 0);
    ccfifo(
        h,
        EMC_DBG,
        (dbg_val & !EMC_DBG_WRITE_ACTIVE_ONLY) | EMC_DBG_WRITE_MUX_ACTIVE,
        0,
    );

    // Step 13 - Ramp up.
    let ramp_up_wait = ramp_up(h, dst, dst_period);
    ccfifo(h, EMC_DBG, dbg_val, 0);

    // Step 14 - Bringup CKE pins.
    let pin_final = if dev_num == TWO_RANK {
        pin_o | 7
    } else {
        (pin_o & 0xFFFF_FFF8) | 1
    };
    ccfifo(h, EMC_PIN, pin_final, 0);

    // Step 15 - Zqlatch.
    let t_pdex = div_o3(dst.w(L::DRAM_TIMINGS_T_PDEX).wrapping_mul(1000), dst_period);
    let mut zq_latch_wait: i32 = if dst_period > 2000 {
        zqcal_fc_adj.wrapping_sub(t_pdex as i32)
    } else {
        zqcal_fc_adj.wrapping_sub((ramp_up_wait.wrapping_add(ramp_down_wait) / dst_period) as i32)
    };
    let mrw3_latch = (flip_fspop & 0xF3FF_FFF7) | 0x0C00_0000;
    let emc_zq_cal;
    if dev_num == ONE_RANK {
        if dst_period > 2000 {
            ccfifo(h, EMC_ZQ_CAL, 0x8000_0001, t_pdex);
        }
        ccfifo(h, EMC_MRW3, mrw3_latch, t_pdex);
        emc_zq_cal = 0x8000_0002;
    } else if zcal_shared {
        if dst_period > 2000 {
            ccfifo(h, EMC_ZQ_CAL, 0x8000_0001, t_pdex);
        }
        let mut t_final = zq_latch_wait.wrapping_add(t_pdex as i32);
        if t_final < 0 {
            t_final = 0;
        }
        ccfifo(h, EMC_ZQ_CAL, 0x8000_0002, t_final as u32);
        ccfifo(h, EMC_ZQ_CAL, 0x4000_0001, 0);
        ccfifo(h, EMC_MRW3, mrw3_latch, 0);
        emc_zq_cal = 0x4000_0002;
        zq_latch_wait = (1_000_000 / dst_period) as i32;
    } else {
        if dst_period > 2000 {
            ccfifo(h, EMC_ZQ_CAL, 1, t_pdex);
        }
        ccfifo(h, EMC_MRW3, mrw3_latch, t_pdex);
        emc_zq_cal = 2;
    }
    // Disable self-refresh.
    ccfifo(h, EMC_SELF_REF, 0, 0);
    ccfifo(h, EMC_REF, 0, 0);
    if zq_latch_wait < 0 {
        zq_latch_wait = 0;
    }
    ccfifo(h, EMC_ZQ_CAL, emc_zq_cal, zq_latch_wait as u32);
    ccfifo(h, EMC_INTSTATUS, 0, 10); // WAR: delay for zqlatch.

    // Step 19.2.
    if bg_change != 0 {
        ccfifo(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE, 0);
        let mut clks = 0;
        if ramp_up_wait <= 1_250_000 {
            clks = (1_250_000 - ramp_up_wait) / dst_period;
        }
        ccfifo(h, EMC_PMACRO_BG_BIAS_CTRL_0, dst_bg, clks);
        ccfifo(h, EMC_DBG, dbg_o, 0);
    }

    // Step 21 - Restore ZCAL and ZCAL interval.
    ccfifo(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE, 0);
    ccfifo(h, EMC_CFG, dst.w(L::BURST_REGS_EMC_CFG) & 0xEFFF_FFFF, 0);

    // Step 22 - Restore EMC_CFG_PIPE_CLK.
    ccfifo(h, EMC_DBG, dbg_o, 0);
    ccfifo(h, EMC_CFG_PIPE_CLK, pipe_clk_o, 0);
    if bg_change != 0 {
        if enable_bg {
            e_wr(h, EMC_PMACRO_BG_BIAS_CTRL_0, dst_bg & 0xFFFF_FFFB);
        } else {
            e_wr(h, EMC_PMACRO_BG_BIAS_CTRL_0, dst_bg & 0xFFFF_FFFE);
        }
    }

    // Step 23 - Clock change. Set CFG_DLL_MODE to RUN_PERIODIC.
    e_rmw(h, EMC_CFG_DIG_DLL, |v| (v & 0xFFFF_FF24) | 0x88);
    e_rd(h, EMC_CFG_DIG_DLL);
    e_rd(h, EMC_FBIO_CFG7);
    m_rd(h, MC_EMEM_ADR_CFG);
    e_rd(h, EMC_INTSTATUS);

    c_wr(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC, sel_clk_src);
    c_rd(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC);
    if wait_emc_status(h, EMC_INTSTATUS, CLKCHANGE_COMPLETE_INT, true, 0) {
        return Err(Error::Timeout("clkchange"));
    }

    // Step 25 - Program MC updown regs.
    if dst.rate_khz() > src.rate_khz() {
        let n = dst.w(L::NUM_UP_DOWN) as usize;
        for (i, &addr) in LA_SCALE_REGS_MC_ADDR_TABLE.iter().enumerate().take(n) {
            m_wr(h, addr, dst.w(L::LA_SCALE_REGS + i));
        }
        let dual = dual_channel(h);
        if timing_update(h, dual) {
            return Err(Error::Timeout("updown"));
        }
    }

    // Step 26 - Restore ZCAL regs.
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(h, EMC_ZCAL_WAIT_CNT, dst.w(L::BURST_REGS_EMC_ZCAL_WAIT_CNT));
    e_wr(h, EMC_ZCAL_INTERVAL, dst.w(L::BURST_REGS_EMC_ZCAL_INTERVAL));
    e_wr(h, EMC_DBG, dbg_o);

    // Step 27 - Restore EMC_CFG, FDPD regs.
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(h, EMC_CFG, dst.w(L::BURST_REGS_EMC_CFG));
    e_wr(h, EMC_DBG, dbg_o);
    e_wr(
        h,
        EMC_FDPD_CTRL_CMD_NO_RAMP,
        dst.w(L::EMC_FDPD_CTRL_CMD_NO_RAMP),
    );
    e_wr(h, EMC_SEL_DPD_CTRL, dst.w(L::EMC_SEL_DPD_CTRL));

    // Step 28.
    e_wr(h, EMC_DBG, dbg_o | EMC_DBG_WRITE_MUX_ACTIVE);
    e_wr(
        h,
        EMC_PMACRO_AUTOCAL_CFG_COMMON,
        dst.w(L::BURST_REGS_EMC_PMACRO_AUTOCAL_CFG_COMMON),
    );
    e_wr(h, EMC_DBG, dbg_o);

    // Step 29 - Power fix WAR.
    e_wr(h, EMC_PMACRO_CFG_PM_GLOBAL_0, 0xFF0000);
    e_wr(h, EMC_PMACRO_TRAINING_CTRL_0, CH0_TRAINING_E_WRPTR);
    e_wr(h, EMC_PMACRO_TRAINING_CTRL_1, CH0_TRAINING_E_WRPTR);
    e_wr(h, EMC_PMACRO_CFG_PM_GLOBAL_0, 0);

    // Step 30 - Re-enable autocal.
    if dst.w(L::BURST_REGS_EMC_CFG_DIG_DLL) & 1 != 0 {
        digital_dll_enable_rs(h, ch1)?;
    }
    e_wr(h, EMC_AUTO_CAL_CONFIG, dst.w(L::EMC_AUTO_CAL_CONFIG));
    Ok(())
}

/// Real divisor of PLLM/PLLMB's encoded post divider (field bits 24:20).
fn post_div(p: u32) -> Option<u32> {
    const D: [u32; 15] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 16, 12, 16, 20, 24, 32];
    D.get(p as usize).copied()
}

/// EMC rate (kHz, Minerva's table unit) from CLK_SOURCE_EMC and the PLLs;
/// `None` if the source is not one this code can compute.
pub fn current_rate_khz<H: Hw>(h: &mut H) -> Option<u32> {
    let clk = c_rd(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC);
    let div = clk & 0xFF;
    let pll = |base: u32, h: &mut H| -> Option<u32> {
        let v = c_rd(h, base);
        let (m, n, p) = (v & 0xFF, (v >> 8) & 0xFF, (v >> 20) & 0x1F);
        if m == 0 {
            return None;
        }
        Some(OSC_KHZ * n / m / post_div(p)?)
    };
    let (src, divided) = match clk >> 29 {
        PLLM_OUT0 | PLLM_UD => (pll(CLK_RST_CONTROLLER_PLLM_BASE, h)?, false),
        PLLMB_UD | PLLMB_OUT0 => (pll(CLK_RST_CONTROLLER_PLLMB_BASE, h)?, false),
        PLLP_OUT0 | PLLP_UD => (PLLP_KHZ, true),
        CLK_M => (OSC_KHZ, true),
        _ => return None,
    };
    // PLLM sources are undivided; the 7.1 divider (half steps) applies to PLLP and CLK_M.
    Some(if divided { src * 2 / (div + 2) } else { src })
}

/// Minerva's `_check_freq_changed`: do the sources differ enough to need
/// a new PLL setting?
fn freq_changed<H: Hw>(
    h: &mut H,
    dst_rate: u32,
    dst_clk: u32,
    src_rate: u32,
    src_clk: u32,
) -> Result<bool, Error> {
    let mut post = 0;
    match c_rd(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC) >> 29 {
        PLLM_OUT0 | PLLM_UD => post = (c_rd(h, CLK_RST_CONTROLLER_PLLM_BASE) >> 20) & 0x1F,
        PLLMB_UD | PLLMB_OUT0 => post = (c_rd(h, CLK_RST_CONTROLLER_PLLMB_BASE) >> 20) & 0x1F,
        _ => {}
    }
    if post > 5 {
        return Err(Error::PostDiv);
    }
    let (s2x, d2x) = (src_clk >> 29, dst_clk >> 29);
    let mut s_div = src_clk & 0xFF;
    let mut d_div = dst_clk & 0xFF;
    if s2x <= PLLMB_UD {
        s_div = 0;
    }
    if d2x <= PLLMB_UD {
        d_div = 0;
    }
    if d2x != s2x && (d2x & 0xFFFF_FFFB != 0 || s2x & 0xFFFF_FFFB != 0) {
        return Ok(true);
    }
    // u32 arithmetic as in Minerva (7.1 divider in half steps).
    let scale = |rate: u32, div: u32| -> u64 {
        (rate
            .wrapping_mul(post + 1)
            .wrapping_mul((div >> 1) * 10 + (div & 1) * 5 + 10)
            / 10) as u64
    };
    let dst_div = scale(dst_rate, d_div);
    let src_div = scale(src_rate, s_div);
    if dst_div == 0 {
        return Ok(true);
    }
    let ratio = ((src_div * 1000) / dst_div) as u32;
    Ok(!(990..=1010).contains(&ratio))
}

/// Minerva's `_pllm_clk_base_cfg`: bring up the PLL the target needs and
/// return the CLK_SOURCE_EMC value to switch to.
fn pllm_clk_base_cfg<H: Hw>(
    h: &mut H,
    rate_khz: u32,
    mut clk_src_emc: u32,
    new_is_pllmb: bool,
) -> Result<u32, Error> {
    let mut cfg = None;
    for row in PLLM_CONFIG.iter() {
        if row[0] == OSC_KHZ && row[1].wrapping_sub(19_200) <= rate_khz {
            cfg = Some(row);
        }
    }
    let Some(row) = cfg else {
        return Ok(clk_src_emc);
    };
    let dividers = row[3] | (row[2] << 8) | ((row[4] & 0x1F) << 20);
    if new_is_pllmb {
        c_wr(h, CLK_RST_CONTROLLER_PLLMB_BASE, dividers);
        c_rmw(h, CLK_RST_CONTROLLER_PLLMB_BASE, |v| v | PLLM_ENABLE);
        let s = clk_src_emc >> 29;
        if s == PLLM_UD {
            clk_src_emc = (clk_src_emc & 0x1FFF_FFFF) | (PLLMB_UD << 29);
        } else if s == 0 {
            clk_src_emc |= PLLMB_OUT0 << 29;
        }
        spin(h, "pllmb lock", |h| {
            c_rd(h, CLK_RST_CONTROLLER_PLLMB_BASE) & PLLM_LOCK != 0
        })?;
    } else {
        c_wr(h, CLK_RST_CONTROLLER_PLLM_BASE, dividers);
        c_rmw(h, CLK_RST_CONTROLLER_PLLM_MISC2, |v| v | PLLM_EN_LCKDET);
        c_rmw(h, CLK_RST_CONTROLLER_PLLM_BASE, |v| v | PLLM_ENABLE);
        if clk_src_emc >> 29 == PLLM_UD {
            clk_src_emc = (clk_src_emc & 0x1FFF_FFFF) | (PLLM_UD << 29);
        }
        spin(h, "pllm lock", |h| {
            c_rd(h, CLK_RST_CONTROLLER_PLLM_BASE) & PLLM_LOCK != 0
        })?;
    }
    Ok(clk_src_emc)
}

/// Switch the EMC from the entry matching the current rate to the entry
/// for `to_khz`. `tables` is the whole table (entries in any order).
/// Returns the new rate.
pub fn switch<H: Hw>(h: &mut H, tables: &[Table], to_khz: u32) -> Result<u32, Error> {
    check_lpddr4(h)?;
    let from = current_rate_khz(h).ok_or(Error::NoRate)?;
    let src = tables
        .iter()
        .find(|t| t.rate_khz() == from)
        .ok_or(Error::NoRate)?;
    let dst = tables
        .iter()
        .find(|t| t.rate_khz() == to_khz)
        .ok_or(Error::NoRate)?;
    switch_entries(h, src, dst)
}

/// The DRAM is LPDDR4 (EMC_FBIO_CFG5.DRAM_TYPE): checked before any write.
pub fn check_lpddr4<H: Hw>(h: &mut H) -> Result<(), Error> {
    if e_rd(h, EMC_FBIO_CFG5) & 3 == 1 {
        Ok(())
    } else {
        Err(Error::NotLpddr4)
    }
}

/// Memory controller registers the entry would write that read differently
/// now (but for the arbiter hysteresis group, which never reads back as written): (offset, live, table). The switch writes all of them verbatim, so a
/// table that describes the running DRAM leaves (almost) none. Returns the
/// count; `out` holds the first ones.
pub fn mc_mismatches<H: Hw>(h: &mut H, t: &Table, out: &mut [(u32, u32, u32); 33]) -> usize {
    let mut n = 0;
    for (i, &addr) in BURST_MC_REGS_ADDR_TABLE.iter().enumerate() {
        // The arbiter hysteresis group reads back differently from what the
        // table (and Minerva) wrote on a Switch (measured, run 52: bit 23
        // gone in all nine, CTRL 0x2 for 0x80001a); the other 24 match.
        if (MC_EMEM_ARB_DHYST_CTRL..=MC_EMEM_ARB_DHYST_TIMEOUT_UTIL_7).contains(&addr) {
            continue;
        }
        let (have, want) = (m_rd(h, addr), t.w(L::BURST_MC_REGS + i));
        if have != want {
            out[n] = (addr, have, want);
            n += 1;
        }
    }
    n
}

/// The switch itself, between two given entries: the trace of register
/// accesses is that of Minerva's `OP_SWITCH` (see `tests/trace.rs`).
pub fn switch_entries<H: Hw>(h: &mut H, src: &Table, dst: &Table) -> Result<u32, Error> {
    if src.rate_khz() == dst.rate_khz() {
        return Err(Error::SameRate);
    }
    if dst.periodic() {
        return Err(Error::Periodic);
    }
    let changed = freq_changed(
        h,
        dst.rate_khz(),
        dst.clk_src_emc(),
        src.rate_khz(),
        src.clk_src_emc(),
    )?;
    let live = c_rd(h, CLK_RST_CONTROLLER_CLK_SOURCE_EMC) >> 29;
    let mut src_is_pllmb = live == PLLMB_UD || live == PLLMB_OUT0;
    let sel = if changed {
        if live == PLLM_UD || live == PLLM_OUT0 {
            src_is_pllmb = !src_is_pllmb;
        } else if live == PLLMB_UD || live == PLLMB_OUT0 {
            src_is_pllmb = false;
        }
        pllm_clk_base_cfg(h, dst.rate_khz(), dst.clk_src_emc(), src_is_pllmb)?
    } else {
        let mut sel = dst.clk_src_emc();
        let s = sel >> 29;
        if src_is_pllmb {
            if s == PLLM_UD || s == PLLMB_UD {
                sel = (sel & 0x1FFF_FFFF) | (PLLMB_UD << 29);
            } else if s == PLLM_OUT0 || s == PLLMB_OUT0 {
                sel = (sel & 0x1FFF_FFFF) | (PLLMB_OUT0 << 29);
            }
        }
        sel
    };
    change_clock(h, src, dst, sel)?;
    Ok(dst.rate_khz())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_contiguous() {
        assert_eq!(
            L::BURST_REG_PER_CH_EMC0_MRW10,
            BURST + BURST_REGS_EMC_ADDR_TABLE.len()
        );
        assert_eq!(
            L::TRIM_PERCH_REGS_EMC_CMD_BRLSHFT_0,
            TRIM + TRIM_REGS_EMC_ADDR_TABLE.len()
        );
        assert_eq!(
            L::VREF_PERCH_REGS_EMC0_TRAINING_OPT_DQS_IB_VREF_RANK0,
            TRIM_PER_CH + TRIM_PERCH_REGS_EMC01_ADDR_TABLE.len()
        );
        assert_eq!(L::TABLE_WORDS * 4, 4928);
    }
}

//! Minerva's training and periodic compensation: what `change_clock`
//! calls while training or for a periodic-training entry.

use crate::layout as L;
use crate::patterns::*;
use crate::regs::*;
use crate::seq::*;
use crate::{Error, Hw, Table, TableMut, Words};

/// The channel register windows from the EMC base (EMC0, EMC1).
const CH0: u32 = 0x3000;

/// Minerva's `comp_seq_t`.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // PeriodicTraining: `periodic_compensation` inlines it.
pub(crate) enum Comp {
    Dvfs = 1,
    WriteTraining = 2,
    PeriodicTraining = 3,
}

/// Minerva's `tree_update_mode_t`.
const DVFS_PT1: u32 = 10;
const DVFS_UPDATE: u32 = 11;
const TRAINING_PT1: u32 = 12;
const TRAINING_UPDATE: u32 = 13;
const PERIODIC_TRAINING_UPDATE: u32 = 14;

/// C's `a / b` as hekate's ARM build computes it (libgcc's EABI
/// `__aeabi_uidiv`): a zero divisor gives 0xFFFFFFFF, or 0 for 0 / 0.
/// The clock tree code divides by oscillator counts read back as 0 when
/// no training ran (the emulator; a dead read on a console).
fn div(a: u32, b: u32) -> u32 {
    match a.checked_div(b) {
        Some(q) => q,
        None if a != 0 => u32::MAX,
        None => 0,
    }
}

/// `_actual_osc_clocks`.
fn actual_osc_clocks(clocks: u32) -> u32 {
    if clocks <= 63 {
        16 * clocks
    } else if clocks <= 127 {
        2048
    } else if clocks < 192 {
        4096
    } else {
        8192
    }
}

/// `_request_mmr_data`.
fn request_mmr_data<H: Hw>(h: &mut H, data: u32, dual: bool) {
    e_wr(h, EMC_MRR, data);
    wait_emc_status(h, EMC_EMC_STATUS, MRR_DIVLD, true, 0);
    if dual {
        wait_emc_status(h, EMC_EMC_STATUS, MRR_DIVLD, true, 1);
    }
}

/// `_start_periodic_compensation`.
fn start_periodic_compensation<H: Hw>(h: &mut H) {
    e_wr(h, EMC_MPC, 0x4B);
    e_rd(h, EMC_MPC);
}

/// Device `dev`'s DQS oscillator counts (MRR 19 then 18): [ch0 u0, ch0
/// u1, ch1 u0, ch1 u1].
fn read_osc<H: Hw>(h: &mut H, dev: u32, ch1: bool) -> [u32; 4] {
    let sel = if dev == 0 { 0x8000_0000 } else { 0x4000_0000 };
    let mut t = [0u32; 4];
    request_mmr_data(h, sel | 0x13_0000, ch1);
    let mrr = e_rd(h, EMC_MRR);
    t[0] = (mrr & 0xFF) << 8;
    t[1] = mrr & 0xFF00;
    if ch1 {
        let mrr = e_rd(h, CH1 + EMC_MRR);
        t[2] = (mrr & 0xFF) << 8;
        t[3] = mrr & 0xFF00;
    }
    request_mmr_data(h, sel | 0x12_0000, ch1);
    let mrr = e_rd(h, EMC_MRR);
    t[0] |= mrr & 0xFF;
    t[1] |= (mrr & 0xFF00) >> 8;
    if ch1 {
        let mrr = e_rd(h, CH1 + EMC_MRR);
        t[2] |= mrr & 0xFF;
        t[3] |= (mrr & 0xFF00) >> 8;
    }
    t
}

/// `_minerva_update_clock_tree_delay`: the DQS oscillator's clock tree
/// delays into `dst`'s moving averages, and the current delays updated
/// where they drift past the margin. Returns the largest drift.
fn update_clock_tree_delay<H: Hw>(
    h: &mut H,
    src: &Table,
    dst: &mut TableMut,
    dev_num: u32,
    ch1: bool,
    update: u32,
) -> u32 {
    let bits = 1u32 << update;
    let dst_mhz = dst.rate_khz() / 1000;
    let src_mhz = src.rate_khz() / 1000;
    let tval = 1_000_000u32.wrapping_mul(actual_osc_clocks(src.w(L::RUN_CLOCKS))) / 2;
    if update > PERIODIC_TRAINING_UPDATE {
        return 0;
    }
    let samples_dvfs = dst.w(L::PTFV_LIST_PTFV_DVFS_SAMPLES);
    let samples_write = dst.w(L::PTFV_LIST_PTFV_WRITE_SAMPLES);
    let weight = dst.w(L::PTFV_LIST_PTFV_MOVAVG_WEIGHT);
    let mut adelta = 0u32;
    // Clock tree slots: c<ch>d<dev>u<unit> at index ch * 4 + dev * 2 + unit.
    for dev in 0..2u32 {
        if dev == 1 && dev_num != TWO_RANK {
            break;
        }
        let temps = if bits & 0x5400 != 0 {
            read_osc(h, dev, ch1)
        } else {
            [0; 4]
        };
        for (k, &temp) in temps.iter().enumerate() {
            let ch = (k / 2) as u32;
            if ch == 1 && !ch1 {
                break;
            }
            let slot = (ch * 4 + dev * 2 + (k as u32 % 2)) as usize;
            let avg = L::PTFV_LIST_PTFV_DQSOSC_MOVAVG_C0D0U0 + slot;
            let cur = L::CURRENT_DRAM_CLKTREE_C0D0U0 + slot;
            let cval = div(tval, src_mhz.wrapping_mul(temp));
            match update {
                DVFS_PT1 | TRAINING_PT1 => {
                    dst.set(avg, dst.w(avg).wrapping_add(100u32.wrapping_mul(cval)));
                    // PT1 only accumulates (its bit is not in 0x6800).
                    continue;
                }
                DVFS_UPDATE => dst.set(avg, div(dst.w(avg), samples_dvfs)),
                TRAINING_UPDATE => dst.set(avg, div(dst.w(avg), samples_write)),
                _ => {
                    // PERIODIC_TRAINING_UPDATE.
                    let v = 100u32
                        .wrapping_mul(cval)
                        .wrapping_add(weight.wrapping_mul(dst.w(avg)));
                    dst.set(avg, div(v, weight.wrapping_add(1)));
                }
            }
            let mut tdelta = dst.w(cur).wrapping_sub(dst.w(avg) / 100) as i32;
            if tdelta < 0 {
                tdelta = tdelta.wrapping_neg();
            }
            if tdelta as u32 > adelta {
                adelta = tdelta as u32;
            }
            let drift = dst_mhz.wrapping_mul(tdelta as u32).wrapping_mul(128) / 1_000_000;
            if update == TRAINING_UPDATE || drift > dst.w(L::TREE_MARGIN) {
                dst.set(cur, dst.w(avg) / 100);
            }
        }
    }
    if update == TRAINING_UPDATE {
        for k in 0..8 {
            dst.set(
                L::TRAINED_DRAM_CLKTREE_C0D0U0 + k,
                dst.w(L::CURRENT_DRAM_CLKTREE_C0D0U0 + k),
            );
        }
    }
    adelta
}

/// `_minerva_periodic_compensation_handler`.
pub(crate) fn periodic_compensation_handler<H: Hw>(
    h: &mut H,
    src: &Table,
    dst: &mut TableMut,
    dev_num: u32,
    ch1: bool,
    seq: Comp,
) -> u32 {
    if dst.w(L::PERIODIC_TRAINING) == 0 {
        return 0;
    }
    let delay = div(
        1000u32.wrapping_mul(actual_osc_clocks(src.w(L::RUN_CLOCKS))),
        src.rate_khz(),
    ) + 2;
    let avg = L::PTFV_LIST_PTFV_DQSOSC_MOVAVG_C0D0U0;
    match seq {
        Comp::Dvfs => {
            if src.w(L::PERIODIC_TRAINING) != 0 && dst.w(L::PTFV_LIST_PTFV_CONFIG_CTRL) & 1 != 0 {
                let samples = dst.w(L::PTFV_LIST_PTFV_DVFS_SAMPLES);
                // C0D0U0, C0D0U1, C1D0U0, C1D0U1, C0D1U0, C0D1U1, C1D1U0, C1D1U1.
                for k in [0, 1, 4, 5, 2, 3, 6, 7] {
                    dst.set(avg + k, src.w(avg + k).wrapping_mul(samples));
                }
            } else {
                for k in [0, 1, 4, 5, 2, 3, 6, 7] {
                    dst.set(avg + k, 0);
                }
                for _ in 0..dst.w(L::PTFV_LIST_PTFV_DVFS_SAMPLES) {
                    start_periodic_compensation(h);
                    h.usleep(delay);
                    update_clock_tree_delay(h, src, dst, dev_num, ch1, DVFS_PT1);
                }
            }
            update_clock_tree_delay(h, src, dst, dev_num, ch1, DVFS_UPDATE)
        }
        Comp::WriteTraining => {
            for k in 0..8 {
                dst.set(avg + k, 0);
            }
            for _ in 0..dst.w(L::PTFV_LIST_PTFV_WRITE_SAMPLES) {
                start_periodic_compensation(h);
                h.usleep(delay);
                update_clock_tree_delay(h, src, dst, dev_num, ch1, TRAINING_PT1);
            }
            update_clock_tree_delay(h, src, dst, dev_num, ch1, TRAINING_UPDATE)
        }
        Comp::PeriodicTraining => {
            start_periodic_compensation(h);
            h.usleep(delay);
            update_clock_tree_delay(h, src, dst, dev_num, ch1, PERIODIC_TRAINING_UPDATE)
        }
    }
}

/// `STORE_TRIM_VAL(chan, rank, reg, byte)`: a byte's write DDLL trim with
/// its barrel shift above (in 64-tap units).
fn store_trim(t: &TableMut, chan: usize, rank: usize, reg: usize, byte: u32) -> i32 {
    let dq = [
        [
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3,
        ],
        [
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2,
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3,
        ],
    ];
    let brl = [
        [
            L::TRIM_PERCH_REGS_EMC0_DATA_BRLSHFT_0,
            L::TRIM_PERCH_REGS_EMC0_DATA_BRLSHFT_1,
        ],
        [
            L::TRIM_PERCH_REGS_EMC1_DATA_BRLSHFT_0,
            L::TRIM_PERCH_REGS_EMC1_DATA_BRLSHFT_1,
        ],
    ];
    let ddll = (t.w(dq[rank][reg]) >> (16 * (byte & 1))) & 0x7FF;
    let shift = ((t.w(brl[chan][rank]) >> (3 * byte)) & 7) << 6;
    ddll.wrapping_add(shift) as i32
}

/// `_minerva_apply_periodic_compensation_trimmer`: the trim register
/// `addr` with the clock tree drift past the margin added in.
pub(crate) fn apply_periodic_compensation_trimmer(t: &TableMut, addr: u32) -> u32 {
    // (chan, rank, reg, byte) of new_trim[0..16].
    let mut new_trim = [0i32; 16];
    for (i, v) in new_trim.iter_mut().enumerate() {
        let (rank, j) = (i / 8, i % 8);
        let chan = j / 4;
        *v = store_trim(t, chan, rank, j / 2, j as u32);
    }
    let rate_mhz = (t.rate_khz() / 1000) as i32;
    let margin = t.w(L::TREE_MARGIN) as i32;
    let rank = match addr {
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3
        | EMC_DATA_BRLSHFT_0 => Some(0usize),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2
        | EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3
        | EMC_DATA_BRLSHFT_1 => Some(1),
        _ => None,
    };
    if let Some(r) = rank {
        // Slots c0dRu0, c0dRu1, c1dRu0, c1dRu1.
        for (i, slot) in [r * 2, r * 2 + 1, 4 + r * 2, 4 + r * 2 + 1]
            .into_iter()
            .enumerate()
        {
            let delta = t
                .w(L::CURRENT_DRAM_CLKTREE_C0D0U0 + slot)
                .wrapping_sub(t.w(L::TRAINED_DRAM_CLKTREE_C0D0U0 + slot))
                .wrapping_mul(128) as i32;
            let taps = delta.wrapping_mul(rate_mhz) / 1_000_000;
            if taps > margin || taps < -margin {
                new_trim[r * 8 + i * 2] = new_trim[r * 8 + i * 2].wrapping_add(taps);
                new_trim[r * 8 + i * 2 + 1] = new_trim[r * 8 + i * 2 + 1].wrapping_add(taps);
            }
        }
        let brlshft = addr == EMC_DATA_BRLSHFT_0 || addr == EMC_DATA_BRLSHFT_1;
        for v in &mut new_trim[r * 8..r * 8 + 8] {
            *v = if brlshft { *v / 64 } else { *v % 64 };
        }
    }
    let pair = |a: usize| (new_trim[a] as u32 & 0x7FF) | ((new_trim[a + 1] as u32 & 0x7FF) << 16);
    let shifts = |base: usize| {
        (0..8).fold(0u32, |acc, b| {
            acc | ((new_trim[base + b] as u32 & 7) << (3 * b))
        })
    };
    match addr {
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0 => pair(0),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1 => pair(2),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2 => pair(4),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3 => pair(6),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0 => pair(8),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1 => pair(10),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2 => pair(12),
        EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3 => pair(14),
        EMC_DATA_BRLSHFT_0 => shifts(0),
        EMC_DATA_BRLSHFT_1 => shifts(8),
        _ => 0,
    }
}

/// `_save_train_results`, transliterated from Minerva (the C's `char`
/// and `u8` locals as `u32` masked to 8 bits: ARM's `char` is unsigned).
#[allow(clippy::all, unused_mut)]
pub(crate) fn save_train_results<H: Hw>(
    h: &mut H,
    t: &mut TableMut,
    needs_training: u32,
    dram_dev_num: u32,
    channel1_enabled: bool,
) {
    let needs_ca_training = needs_training & NEEDS_TRAINING_CA != 0;
    let needs_ca_vref_training = needs_training & NEEDS_TRAINING_CA_VREF != 0;
    let needs_quse_training = needs_training & NEEDS_TRAINING_QUSE != 0;
    let needs_quse_vref_training = needs_training & NEEDS_TRAINING_QUSE_VREF != 0;
    let needs_wr_training = needs_training & NEEDS_TRAINING_WR != 0;
    let needs_wr_vref_training = needs_training & NEEDS_TRAINING_WR_VREF != 0;
    let needs_rd_training = needs_training & NEEDS_TRAINING_RD != 0;
    let needs_rd_vref_training = needs_training & NEEDS_TRAINING_RD_VREF != 0;
    let needs_training_in_self_refresh = needs_training & NEEDS_TRAINING_IN_SELF_REFRESH != 0;

    if needs_ca_training {
        t.set(
            L::TRIM_PERCH_REGS_EMC_CMD_BRLSHFT_0,
            e_rd(h, CH0 + EMC_CMD_BRLSHFT_0),
        );
        t.set(
            L::TRIM_PERCH_REGS_EMC_CMD_BRLSHFT_1,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_CMD_BRLSHFT_1)
            } else {
                0
            },
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_4,
            e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_4),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_5,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_5)
            } else {
                0
            },
        );

        if needs_training_in_self_refresh {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_0,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_1,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_2,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD0_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_0,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_1,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_2,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD1_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_0,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_1,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_2,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD2_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_0,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_1,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_2,
                e_rd(h, EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_CMD3_2),
            );
        }
    }

    if needs_ca_vref_training {
        t.set(
            L::BURST_REG_PER_CH_EMC0_MRW10,
            (e_rd(h, CH0 + EMC_TRAINING_OPT_CA_VREF) & 0xFFFF) | 0x880C0000,
        );
        t.set(
            L::BURST_REG_PER_CH_EMC1_MRW10,
            (if channel1_enabled {
                e_rd(h, CH1 + EMC_TRAINING_OPT_CA_VREF) & 0xFFFF
            } else {
                0
            }) | 0x880C0000,
        );

        let mrw11_dev_selectn: u32;
        if dram_dev_num == TWO_RANK {
            mrw11_dev_selectn = 0x480C0000;
        } else {
            mrw11_dev_selectn = 0xC80C0000;
        }

        t.set(
            L::BURST_REG_PER_CH_EMC0_MRW11,
            ((e_rd(h, CH0 + EMC_TRAINING_OPT_CA_VREF) >> 16) & 0xFF)
                | (e_rd(h, CH0 + EMC_TRAINING_OPT_CA_VREF) >> 24 << 8)
                | (mrw11_dev_selectn & 0xFFFFFF00),
        );

        t.set(
            L::BURST_REG_PER_CH_EMC1_MRW11,
            (((if channel1_enabled {
                e_rd(h, CH1 + EMC_TRAINING_OPT_CA_VREF)
            } else {
                0
            }) >> 16)
                & 0xFF)
                | ((if channel1_enabled {
                    e_rd(h, CH1 + EMC_TRAINING_OPT_CA_VREF)
                } else {
                    0
                }) >> 24
                    << 8)
                | (mrw11_dev_selectn & 0xFFFFFF00),
        );
    }

    if needs_quse_training || needs_rd_training {
        t.set(
            L::TRIM_PERCH_REGS_EMC_QUSE_BRLSHFT_0,
            e_rd(h, CH0 + EMC_QUSE_BRLSHFT_0),
        );
        t.set(
            L::TRIM_PERCH_REGS_EMC_QUSE_BRLSHFT_1,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_QUSE_BRLSHFT_1)
            } else {
                0
            },
        );

        t.set(
            L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK0_0,
            e_rd(h, CH0 + EMC_PMACRO_QUSE_DDLL_RANK0_0),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK0_1,
            e_rd(h, CH0 + EMC_PMACRO_QUSE_DDLL_RANK0_1),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK0_2,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_QUSE_DDLL_RANK0_2)
            } else {
                0
            },
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK0_3,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_QUSE_DDLL_RANK0_3)
            } else {
                0
            },
        );

        if dram_dev_num == TWO_RANK {
            t.set(
                L::TRIM_PERCH_REGS_EMC_QUSE_BRLSHFT_2,
                e_rd(h, CH0 + EMC_QUSE_BRLSHFT_2),
            );
            t.set(
                L::TRIM_PERCH_REGS_EMC_QUSE_BRLSHFT_3,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_QUSE_BRLSHFT_3)
                } else {
                    0
                },
            );

            t.set(
                L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK1_0,
                e_rd(h, CH0 + EMC_PMACRO_QUSE_DDLL_RANK1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK1_1,
                e_rd(h, CH0 + EMC_PMACRO_QUSE_DDLL_RANK1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK1_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_QUSE_DDLL_RANK1_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_QUSE_DDLL_RANK1_3,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_QUSE_DDLL_RANK1_3)
                } else {
                    0
                },
            );
        }
    }

    if needs_quse_vref_training {
        if dram_dev_num == TWO_RANK {
            let mut emc0_opt_dqs_array = [0u32; 4];
            let mut emc1_opt_dqs_array = [0u32; 4];
            let mut emc1_training_opt_dqs_ib_vref_rank0_val: u32 = if channel1_enabled {
                e_rd(h, CH1 + EMC_TRAINING_OPT_DQS_IB_VREF_RANK0)
            } else {
                0
            };
            let mut emc1_training_opt_dqs_ib_vref_rank1_val: u32 = if channel1_enabled {
                e_rd(h, CH1 + EMC_TRAINING_OPT_DQS_IB_VREF_RANK1)
            } else {
                0
            };

            for i in 0..4u32 {
                emc0_opt_dqs_array[i as usize] =
                    (e_rd(h, CH0 + EMC_TRAINING_OPT_DQS_IB_VREF_RANK0) >> (8 * i)) & 0xFF;
                emc1_opt_dqs_array[i as usize] =
                    (emc1_training_opt_dqs_ib_vref_rank0_val >> (8 * i)) & 0xFF;
            }

            let mut ib_vref_dqs_0: u32 = 0;
            let mut ib_vref_dqs_1: u32 = 0;
            for i in 0..4u32 {
                ib_vref_dqs_0 |= (emc0_opt_dqs_array[i as usize]
                    + ((e_rd(h, CH0 + EMC_TRAINING_OPT_DQS_IB_VREF_RANK1) >> (8 * i)) & 0xFF))
                    >> 1
                    << (8 * i);
                ib_vref_dqs_1 |= (emc1_opt_dqs_array[i as usize]
                    + ((emc1_training_opt_dqs_ib_vref_rank1_val >> (8 * i)) & 0xFF))
                    >> 1
                    << (8 * i);
            }

            t.set(L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQS_0, ib_vref_dqs_0);
            t.set(L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQS_1, ib_vref_dqs_1);
        } else {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQS_0,
                e_rd(h, EMC_PMACRO_IB_VREF_DQS_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQS_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_VREF_DQS_1)
                } else {
                    0
                },
            );
        }
    }

    if needs_rd_training {
        t.set(
            L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_0,
            e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_0),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_1,
            e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_1),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_2,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_2)
            } else {
                0
            },
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_3,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_3)
            } else {
                0
            },
        );

        if dram_dev_num == TWO_RANK {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_0,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_1,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_3,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_3)
                } else {
                    0
                },
            );
        }

        if needs_training_in_self_refresh {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_0,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_1,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_2,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE0_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_0,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_1,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_2,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE1_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_0,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_1,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_2,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE2_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_0,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_1,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_2,
                e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE3_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE4_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE5_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE6_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK0_BYTE7_2)
                } else {
                    0
                },
            );

            if dram_dev_num == TWO_RANK {
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_0,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_1,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_2,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE0_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_0,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_1,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_2,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE1_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_0,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_1,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_2,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE2_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_0,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_1,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_2,
                    e_rd(h, CH0 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE3_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE4_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE5_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE6_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_IB_DDLL_SHORT_DQ_RANK1_BYTE7_2)
                    } else {
                        0
                    },
                );
            }
        }

        if needs_rd_vref_training {
            let mut ib_vref_dq_byte0_icr: u32 = ((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 0) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 0) & 0x80000000) != 0 {
                ib_vref_dq_byte0_icr = ((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 0) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte1_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 8) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 1) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 1) & 0x80000000) != 0 {
                ib_vref_dq_byte1_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 8) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 1) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte2_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 16) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 2) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 2) & 0x80000000) != 0 {
                ib_vref_dq_byte2_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 16) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 2) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte3_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 24) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 3) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 3) & 0x80000000) != 0 {
                ib_vref_dq_byte3_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_0) >> 24) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 3) & 0x7F))
                    & 0xFF;
            }

            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQ_0,
                ((ib_vref_dq_byte0_icr & 0x7F) | (ib_vref_dq_byte1_icr & 0x7F) << 8)
                    | ((ib_vref_dq_byte2_icr & 0x7F) << 16)
                    | ((ib_vref_dq_byte3_icr & 0x7F) << 24),
            );

            let mut ib_vref_dq_byte4_icr: u32 = ((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 4) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 4) & 0x80000000) != 0 {
                ib_vref_dq_byte4_icr = ((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 4) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte5_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 8) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 5) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 5) & 0x80000000) != 0 {
                ib_vref_dq_byte5_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 8) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 5) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte6_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 16) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 6) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 6) & 0x80000000) != 0 {
                ib_vref_dq_byte6_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 16) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 6) & 0x7F))
                    & 0xFF;
            }

            let mut ib_vref_dq_byte7_icr: u32 = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 24) & 0x7F)
                + (t.w(L::SAVE_RESTORE_MOD_REGS + 7) & 0x7F))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 7) & 0x80000000) != 0 {
                ib_vref_dq_byte7_icr = (((e_rd(h, EMC_PMACRO_IB_VREF_DQ_1) >> 24) & 0x7F)
                    - (t.w(L::SAVE_RESTORE_MOD_REGS + 7) & 0x7F))
                    & 0xFF;
            }

            t.set(
                L::TRIM_REGS_EMC_PMACRO_IB_VREF_DQ_1,
                ((ib_vref_dq_byte4_icr & 0x7F) | (ib_vref_dq_byte5_icr & 0x7F) << 8)
                    | ((ib_vref_dq_byte6_icr & 0x7F) << 16)
                    | ((ib_vref_dq_byte7_icr & 0x7F) << 24),
            );
        }
    }

    if needs_wr_training {
        t.set(
            L::TRIM_PERCH_REGS_EMC0_DATA_BRLSHFT_0,
            e_rd(h, CH0 + EMC_DATA_BRLSHFT_0),
        );
        t.set(
            L::TRIM_PERCH_REGS_EMC1_DATA_BRLSHFT_0,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_DATA_BRLSHFT_0)
            } else {
                0
            },
        );

        if dram_dev_num == TWO_RANK {
            t.set(
                L::TRIM_PERCH_REGS_EMC0_DATA_BRLSHFT_1,
                e_rd(h, CH0 + EMC_DATA_BRLSHFT_1),
            );
            t.set(
                L::TRIM_PERCH_REGS_EMC1_DATA_BRLSHFT_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_DATA_BRLSHFT_1)
                } else {
                    0
                },
            );
        }

        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0,
            e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1,
            e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1),
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2)
            } else {
                0
            },
        );
        t.set(
            L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3,
            if channel1_enabled {
                e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3)
            } else {
                0
            },
        );

        if dram_dev_num == TWO_RANK {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3)
                } else {
                    0
                },
            );
        }

        if needs_training_in_self_refresh {
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_0,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_1,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_2,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE0_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_0,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_1,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_2,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE1_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_0,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_1,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_2,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE2_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_0,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_0),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_1,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_1),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_2,
                e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE3_2),
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE4_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE5_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE6_2)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_0,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_0)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_1,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_1)
                } else {
                    0
                },
            );
            t.set(
                L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_2,
                if channel1_enabled {
                    e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK0_BYTE7_2)
                } else {
                    0
                },
            );

            if dram_dev_num == TWO_RANK {
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_0,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_1,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_2,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE0_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_0,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_1,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_2,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE1_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_0,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_1,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_2,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE2_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_0,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_0),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_1,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_1),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_2,
                    e_rd(h, CH0 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE3_2),
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE4_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE5_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE6_2)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_0,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_0)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_1,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_1)
                    } else {
                        0
                    },
                );
                t.set(
                    L::TRIM_REGS_EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_2,
                    if channel1_enabled {
                        e_rd(h, CH1 + EMC_PMACRO_OB_DDLL_SHORT_DQ_RANK1_BYTE7_2)
                    } else {
                        0
                    },
                );
            }
        }

        if needs_wr_vref_training {
            let emc1_ranks_sub_partitions: u32;
            emc1_ranks_sub_partitions = if channel1_enabled {
                e_rd(h, CH1 + EMC_TRAINING_OPT_DQ_OB_VREF)
            } else {
                0
            };

            let mut emc0_ib_vref_dq_byte8_modded_plus: u32 = (t.w(L::SAVE_RESTORE_MOD_REGS + 8)
                + e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 8) & 0x80000000) != 0 {
                emc0_ib_vref_dq_byte8_modded_plus = (e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF)
                    - t.w(L::SAVE_RESTORE_MOD_REGS + 8))
                    & 0xFF;
            }

            let mut emc0_mrw12_op_sp1: u32 =
                (((e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) & 0xFFFF) >> 8)
                    + t.w(L::SAVE_RESTORE_MOD_REGS + 9))
                    & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 9) & 0x80000000) != 0 {
                emc0_mrw12_op_sp1 = (((e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) & 0xFFFF) >> 8)
                    - t.w(L::SAVE_RESTORE_MOD_REGS + 9))
                    & 0xFF;
            }

            let mut emc0_mrw13_op_sp0: u32 = (((e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) >> 16)
                & 0xFF)
                + t.w(L::SAVE_RESTORE_MOD_REGS + 8))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 8) & 0x80000000) != 0 {
                emc0_mrw13_op_sp0 = (((e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) >> 16) & 0xFF)
                    - t.w(L::SAVE_RESTORE_MOD_REGS + 8))
                    & 0xFF;
            }

            let mut emc0_ib_vref_dq_byte9_modded_a_plus: u32 = (t.w(L::SAVE_RESTORE_MOD_REGS + 9)
                + (e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) >> 24))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 9) & 0x80000000) != 0 {
                emc0_ib_vref_dq_byte9_modded_a_plus =
                    ((e_rd(h, CH0 + EMC_TRAINING_OPT_DQ_OB_VREF) >> 24)
                        - (t.w(L::SAVE_RESTORE_MOD_REGS + 9) & 0xFF))
                        & 0xFF;
            }

            let mut emc0_ib_vref_dq_byte10_modded_plus: u32 =
                (emc1_ranks_sub_partitions + t.w(L::SAVE_RESTORE_MOD_REGS + 10)) & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 10) & 0x80000000) != 0 {
                emc0_ib_vref_dq_byte10_modded_plus =
                    (emc1_ranks_sub_partitions - t.w(L::SAVE_RESTORE_MOD_REGS + 10)) & 0xFF;
            }

            let mut emc0_ib_vref_dq_byte11_modded_plus: u32 =
                (((emc1_ranks_sub_partitions & 0xFFFF) >> 8) + t.w(L::SAVE_RESTORE_MOD_REGS + 11))
                    & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 11) & 0x80000000) != 0 {
                emc0_ib_vref_dq_byte11_modded_plus = (((emc1_ranks_sub_partitions & 0xFFFF) >> 8)
                    - t.w(L::SAVE_RESTORE_MOD_REGS + 11))
                    & 0xFF;
            }

            let mut emc1_mrw13_op_sp0: u32 = (((emc1_ranks_sub_partitions >> 16) & 0xFF)
                + t.w(L::SAVE_RESTORE_MOD_REGS + 10))
                & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 10) & 0x80000000) != 0 {
                emc1_mrw13_op_sp0 = (((emc1_ranks_sub_partitions >> 16) & 0xFF)
                    - t.w(L::SAVE_RESTORE_MOD_REGS + 10))
                    & 0xFF;
            }

            let mut emc1_mrw13_op_sp1: u32 =
                ((emc1_ranks_sub_partitions >> 24) + t.w(L::SAVE_RESTORE_MOD_REGS + 11)) & 0xFF;
            if (t.w(L::SAVE_RESTORE_MOD_REGS + 11) & 0x80000000) != 0 {
                emc1_mrw13_op_sp1 =
                    ((emc1_ranks_sub_partitions >> 24) - t.w(L::SAVE_RESTORE_MOD_REGS + 11)) & 0xFF;
            }

            let mut mr13_dev_ext_cnt_sp_addr: u32 = 0xC80E0000;
            if dram_dev_num == TWO_RANK {
                mr13_dev_ext_cnt_sp_addr = 0x480E0000;
            }

            t.set(
                L::BURST_REG_PER_CH_EMC1_MRW12,
                (emc0_ib_vref_dq_byte10_modded_plus & 0xFF)
                    | 0x880E0000
                    | (emc0_ib_vref_dq_byte11_modded_plus << 8),
            );
            t.set(
                L::BURST_REG_PER_CH_EMC0_MRW12,
                emc0_ib_vref_dq_byte8_modded_plus | 0x880E0000 | (emc0_mrw12_op_sp1 << 8),
            );
            t.set(
                L::BURST_REG_PER_CH_EMC0_MRW13,
                emc0_ib_vref_dq_byte9_modded_a_plus << 8
                    | emc0_mrw13_op_sp0
                    | mr13_dev_ext_cnt_sp_addr,
            );
            t.set(
                L::BURST_REG_PER_CH_EMC1_MRW13,
                (emc1_mrw13_op_sp1 << 8) | emc1_mrw13_op_sp0 | mr13_dev_ext_cnt_sp_addr,
            );
        }
    }
}

/// `_minerva_train`: the destination's training passes (unless trained),
/// then the switch if asked. `patterns`: the RAM patterns still to load.
fn train<H: Hw>(
    h: &mut H,
    src: &Table,
    dst: &mut TableMut,
    switch_rate: bool,
    sel: u32,
    patterns: &mut bool,
) -> Result<(), Error> {
    let needs = dst.w(L::NEEDS_TRAINING);
    if *patterns {
        let off = (dst.w(L::TRAINING_PATTERN) * 256) as usize;
        for i in 0..256 {
            e_wr(h, EMC_TRAINING_PATRAM_DQ, RAM_PATTERN_DQ_TABLE[off + i]);
            e_wr(
                h,
                EMC_TRAINING_PATRAM_DMI,
                RAM_PATTERN_DMI_TABLE[off + i] & 0xF,
            );
            e_wr(h, EMC_TRAINING_PATRAM_CTRL, 0x8000_0000 + i as u32);
        }
        *patterns = false;
    }
    if dst.w(L::TRAINED) == 0 {
        let mut passes = [0u32; 8];
        let mut n = 0;
        let isr = NEEDS_TRAINING_IN_SELF_REFRESH;
        if needs & NEEDS_TRAINING_CA_COMBO != 0 {
            passes[n] = needs & (NEEDS_TRAINING_CA_COMBO | isr);
            n += 1;
            if m_rd(h, MC_EMEM_ADR_CFG) & 1 != 0 {
                passes[n] = needs & (NEEDS_TRAINING_CA_COMBO | NEEDS_TRAINING_SWAP_RANK | isr);
                n += 1;
            }
        }
        if needs & NEEDS_TRAINING_QUSE_COMBO != 0 {
            passes[n] = needs & (NEEDS_TRAINING_QUSE_COMBO | isr);
            n += 1;
            if m_rd(h, MC_EMEM_ADR_CFG) & 1 != 0 {
                passes[n] = needs & (NEEDS_TRAINING_QUSE | isr);
                n += 1;
            }
        }
        if needs & (NEEDS_TRAINING_WR_COMBO | NEEDS_TRAINING_RD_COMBO) != 0 {
            passes[n] = needs & (NEEDS_TRAINING_WR_COMBO | NEEDS_TRAINING_RD_COMBO | isr);
            n += 1;
        }
        for &pass in &passes[..n] {
            change_clock(h, src, dst, pass, sel)?;
            let dual = dual_channel(h);
            e_rmw(h, EMC_DBG, |v| {
                (v & !EMC_DBG_CFG_SWAP_MASK) | EMC_DBG_CFG_SWAP_ASSEMBLY_ONLY
            });
            e_rmw(h, EMC_CFG_UPDATE, |v| (v & 0xFFFF_FFF9) | 4);
            timing_update(h, dual);
            e_rmw(h, EMC_CFG_UPDATE, |v| v & 0xFFFF_FFF9);
            e_rmw(h, EMC_DBG, |v| v & !EMC_DBG_CFG_SWAP_MASK);
            e_rmw(h, EMC_CFG_DIG_DLL, |v| (v & 0xFFFF_FF3E) | 0x80);
            timing_update(h, dual);
            let mut dll = e_rd(h, EMC_CFG_DIG_DLL) & 0xFFFF_FFFE;
            if dst.w(L::BURST_REGS_EMC_CFG_DIG_DLL) == 1 {
                dll = e_rd(h, EMC_CFG_DIG_DLL) | 1;
            }
            e_wr(h, EMC_CFG_DIG_DLL, (dll & 0xFFFF_FF3F) | 0x80);
            timing_update(h, dual);
            spin(h, "dll lock after training", |h| {
                e_rd(h, EMC_DIG_DLL_STATUS) & 0x8000 != 0
            })?;
            // Bug 200024907.
            e_wr(h, EMC_RP, src.w(L::BURST_REGS_EMC_RP));
            e_wr(h, EMC_R2P, src.w(L::BURST_REGS_EMC_R2P));
            e_wr(h, EMC_W2P, src.w(L::BURST_REGS_EMC_W2P));
            e_wr(h, EMC_TRPAB, src.w(L::BURST_REGS_EMC_TRPAB));
            timing_update(h, dual);
        }
        dst.set(L::TRAINED, 1);
    }
    if switch_rate {
        change_clock(h, src, dst, 0, sel)?;
    }
    Ok(())
}

/// `_minerva_do_periodic_compensation`: the clock tree drift measured at
/// the running rate and the write trimmers corrected past the margin.
pub fn periodic_compensation<H: Hw>(h: &mut H, t: &mut TableMut) {
    if t.w(L::PERIODIC_TRAINING) == 0 {
        return;
    }
    let dev_num = (m_rd(h, MC_EMEM_ADR_CFG) & 1) + 1;
    let pd = if dev_num == TWO_RANK {
        IN_POWERDOWN_BOTH_MASK
    } else {
        IN_POWERDOWN_1DEV_MASK
    };
    let ch1 = (t.w(L::BURST_REGS_EMC_FBIO_CFG7) >> 2) & 1 != 0;
    e_rd(h, EMC_DBG);
    let cfg_o = e_rd(h, EMC_CFG);
    let dll_o = e_rd(h, EMC_CFG_DIG_DLL);
    let update_o = e_rd(h, EMC_CFG_UPDATE);
    e_wr(h, EMC_CFG_DIG_DLL, dll_o & 0xFFFF_FFFE);
    e_wr(h, EMC_CFG_UPDATE, (update_o & 0xFFFF_F9FF) | 0x400);
    e_wr(h, EMC_CFG, cfg_o & 0x0FFF_FFFF);
    timing_update(h, ch1);
    wait_emc_status(h, EMC_EMC_STATUS, pd, false, 0);
    if ch1 {
        wait_emc_status(h, EMC_EMC_STATUS, pd, false, 1);
    }
    wait_emc_status(h, EMC_EMC_STATUS, IN_SELF_REFRESH_MASK, false, 0);
    if ch1 {
        wait_emc_status(h, EMC_EMC_STATUS, IN_SELF_REFRESH_MASK, false, 1);
    }
    wait_emc_status(h, EMC_CFG_DIG_DLL, 1, false, 0);
    if ch1 {
        wait_emc_status(h, EMC_CFG_DIG_DLL, 1, false, 1);
    }
    start_periodic_compensation(h);
    h.usleep(
        div(
            1000u32.wrapping_mul(actual_osc_clocks(t.w(L::RUN_CLOCKS))),
            t.rate_khz(),
        ) + 2,
    );
    // The entry is both source and destination: its own copy reads.
    let mut copy = [0u32; crate::TABLE_WORDS];
    copy.copy_from_slice(t.0);
    let src = Table(&copy);
    let adelta = update_clock_tree_delay(h, &src, t, dev_num, ch1, PERIODIC_TRAINING_UPDATE);
    let rate_mhz = t.rate_khz() / 1000;
    if adelta != 0 && (rate_mhz * 128).wrapping_mul(adelta) / 1_000_000 > t.w(L::TREE_MARGIN) {
        for &addr in PERIODIC_TRAINING_ADDR.iter() {
            let v = apply_periodic_compensation_trimmer(t, addr);
            e_wr(h, addr, v);
        }
    }
    e_wr(h, EMC_CFG, cfg_o);
    e_wr(h, EMC_CFG_DIG_DLL, dll_o);
    timing_update(h, ch1);
    e_wr(h, EMC_CFG_UPDATE, update_o);
}

/// What `Mtc::run` does (Minerva's `train_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Switch,
    Train,
    TrainSwitch,
}

/// A whole table being trained and switched in place (Minerva's
/// `mtc_config_t`): `entries` revision-7 entries back to back.
pub struct Mtc<'a> {
    pub words: &'a mut [u32],
    pub entries: usize,
    /// The rate the EMC runs at now (kHz).
    pub rate_from: u32,
    /// The RAM patterns are loaded before the first training only.
    pub train_ram_patterns: bool,
}

impl<'a> Mtc<'a> {
    pub fn new(words: &'a mut [u32], entries: usize, rate_from: u32) -> Self {
        Mtc {
            words,
            entries,
            rate_from,
            train_ram_patterns: true,
        }
    }

    fn find(&self, khz: u32) -> Option<usize> {
        (0..self.entries).rfind(|&i| self.words[i * crate::TABLE_WORDS + L::RATE_KHZ] == khz)
    }

    /// `_minerva_do_training_switch` from `rate_from` to `to_khz`.
    pub fn run<H: Hw>(&mut self, h: &mut H, op: Op, to_khz: u32) -> Result<(), Error> {
        let (si, di) = (
            self.find(self.rate_from).ok_or(Error::NoRate)?,
            self.find(to_khz).ok_or(Error::NoRate)?,
        );
        let w = crate::TABLE_WORDS;
        // The source read from a copy: it may be the destination itself
        // (training at the running rate), which the C reads as it writes;
        // training never writes the fields read from the source.
        let mut src_copy = [0u32; crate::TABLE_WORDS];
        src_copy.copy_from_slice(&self.words[si * w..(si + 1) * w]);
        let src = Table::new(&src_copy)?;
        let mut dst = TableMut::new(&mut self.words[di * w..(di + 1) * w])?;
        let sel = select_clk_src(h, &src, &dst.view())?;
        match op {
            Op::Switch => change_clock(h, &src, &mut dst, 0, sel)?,
            Op::Train => train(h, &src, &mut dst, false, sel, &mut self.train_ram_patterns)?,
            Op::TrainSwitch => train(h, &src, &mut dst, true, sel, &mut self.train_ram_patterns)?,
        }
        if op != Op::Train {
            self.rate_from = dst.rate_khz();
            if dst.w(L::PERIODIC_TRAINING) != 0 {
                periodic_compensation(h, &mut dst);
            }
        }
        Ok(())
    }
}

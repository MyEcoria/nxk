//! The DRAM configuration sequences of hekate's bdk/mem/sdram.c
//! (`_sdram_config_t210` and `_sdram_config_t210b01`), transliterated
//! line for line: hekate's comments kept, `params->x` as `p.x`.
#![allow(unused_mut, unused_parens, clippy::all)]

use crate::hdr::*;
use crate::reg::{rd, wr};
use crate::sdram_tables::{ParamsT210, ParamsT210b01};
use crate::timer::{now_us, usleep};

pub fn config_t210(p: &ParamsT210) {
    // VDDP Select.
    wr(PMC_BASE + APBDEV_PMC_VDDP_SEL, p.pmc_vddp_sel);
    usleep(p.pmc_vddp_sel_wait);

    // Set DDR pad voltage.
    wr(PMC_BASE + APBDEV_PMC_DDR_PWR, rd(PMC_BASE + APBDEV_PMC_DDR_PWR)); // Normally params->pmc_ddr_pwr.

    // Turn on MEM IO Power.
    wr(PMC_BASE + APBDEV_PMC_NO_IOPOWER, rd(PMC_BASE + APBDEV_PMC_NO_IOPOWER) & (PMC_NO_IOPOWER_SDMMC1)); // Only keep SDMMC1 state. (Was params->pmc_no_io_power).
    wr(PMC_BASE + APBDEV_PMC_REG_SHORT, p.pmc_reg_short);

    wr(PMC_BASE + APBDEV_PMC_DDR_CNTRL, p.pmc_ddr_ctrl);

    // Patch 1 using BCT spare variables
    if (p.emc_bct_spare0) != 0 {
        wr(p.emc_bct_spare0, p.emc_bct_spare1);
    }

    // Program DPD3/DPD4 regs (coldboot path).
    // Enable sel_dpd on unused pins.
    let mut dpd_req: u32 = (p.emc_pmc_scratch1 & 0x3FFFFFFF) | PMC_IO_DPD_REQ_DPD_ON;
    wr(PMC_BASE + APBDEV_PMC_IO_DPD3_REQ, (dpd_req ^ 0xFFFF) & 0xC000FFFF);
    usleep(p.pmc_io_dpd3_req_wait);

    // Disable e_dpd_vttgen.
    dpd_req = (p.emc_pmc_scratch2 & 0x3FFFFFFF) | PMC_IO_DPD_REQ_DPD_ON;
    wr(PMC_BASE + APBDEV_PMC_IO_DPD4_REQ, (dpd_req & 0xFFFF0000) ^ 0x3FFF0000);
    usleep(p.pmc_io_dpd4_req_wait);

    // Disable e_dpd_bg.
    wr(PMC_BASE + APBDEV_PMC_IO_DPD4_REQ, (dpd_req ^ 0xFFFF) & 0xC000FFFF);
    usleep(p.pmc_io_dpd4_req_wait);

    wr(PMC_BASE + APBDEV_PMC_WEAK_BIAS, 0);
    usleep(1);

    // Start PLLM.
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_MISC1, p.pllm_setup_control);
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_MISC2, 0);

    let mut pllm_div: u32 = (p.pllm_feedback_divider << 8) | p.pllm_input_divider | ((p.pllm_post_divider & 0xFFFF) << 20);
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_BASE, pllm_div);
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_BASE, pllm_div | PLL_BASE_ENABLE);

    // PLLM lock, 300 us at most (then on anyway, as hekate).
    let wait_end: u32 = now_us().wrapping_add(300);
    'lock: {
        while (rd(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_BASE) & (1 << 27)) == 0 {
            if now_us() >= wait_end {
                break 'lock;
            }
        }
        usleep(10);
    }

    // Set clock sources.
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_SOURCE_EMC, ((p.mc_emem_arb_misc0 >> 11) & 0x10000) | (p.emc_clock_source & 0xFFFEFFFF));
    if (p.emc_clock_source_dll) != 0 {
        wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_SOURCE_EMC_DLL, p.emc_clock_source_dll);
    }
    if (p.clear_clock2_mc1) != 0 {
        wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_ENB_W_CLR, (1 << CLK_W_MC1)); // Clear Reset to MC1.
    }

    // Enable and clear reset for memory clocks.
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_ENB_H_SET, (1 << CLK_H_EMC) | (1 << CLK_H_MEM));
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_ENB_X_SET, (1 << CLK_X_EMC_DLL));
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_RST_DEV_H_CLR, (1 << CLK_H_EMC) | (1 << CLK_H_MEM));

    // Set pad vtt levels.
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_0, p.emc_pmacro_vttgen_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_1, p.emc_pmacro_vttgen_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_2, p.emc_pmacro_vttgen_ctrl2);

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);
    usleep(10); // Ensure the regulators settle.

    // Select EMC write mux.
    wr(EMC_BASE + EMC_DBG, (p.emc_dbg_write_mux << 1) | p.emc_dbg);

    // Patch 2 using BCT spare variables.
    if (p.emc_bct_spare2) != 0 {
        wr(p.emc_bct_spare2, p.emc_bct_spare3);
    }

    // Program CMD mapping. Required before brick mapping, else
    // we can't guarantee CK will be differential at all times.
    wr(EMC_BASE + EMC_FBIO_CFG7, p.emc_fbio_cfg7);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_0, p.emc_cmd_mapping_cmd0_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_1, p.emc_cmd_mapping_cmd0_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_2, p.emc_cmd_mapping_cmd0_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_0, p.emc_cmd_mapping_cmd1_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_1, p.emc_cmd_mapping_cmd1_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_2, p.emc_cmd_mapping_cmd1_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_0, p.emc_cmd_mapping_cmd2_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_1, p.emc_cmd_mapping_cmd2_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_2, p.emc_cmd_mapping_cmd2_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_0, p.emc_cmd_mapping_cmd3_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_1, p.emc_cmd_mapping_cmd3_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_2, p.emc_cmd_mapping_cmd3_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_BYTE, p.emc_cmd_mapping_byte);

    // Program brick mapping.
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_0, p.emc_pmacro_brick_mapping0);
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_1, p.emc_pmacro_brick_mapping1);
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_2, p.emc_pmacro_brick_mapping2);

    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU1, (p.emc_pmacro_brick_ctrl_rfu1 & 0x1120112) | 0x1EED1EED);

    // This is required to do any reads from the pad macros.
    wr(EMC_BASE + EMC_CONFIG_SAMPLE_DELAY, p.emc_config_sample_delay);

    // Set data pipes mode.
    wr(EMC_BASE + EMC_FBIO_CFG8, p.emc_fbio_cfg8);

    // Set swizzle for Rank 0.
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE0, p.emc_swizzle_rank0_byte0);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE1, p.emc_swizzle_rank0_byte1);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE2, p.emc_swizzle_rank0_byte2);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE3, p.emc_swizzle_rank0_byte3);
    // Set swizzle for Rank 1.
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE0, p.emc_swizzle_rank1_byte0);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE1, p.emc_swizzle_rank1_byte1);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE2, p.emc_swizzle_rank1_byte2);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE3, p.emc_swizzle_rank1_byte3);

    // Patch 3 using BCT spare variables.
    if (p.emc_bct_spare6) != 0 {
        wr(p.emc_bct_spare6, p.emc_bct_spare7);
    }

    // Program calibration impedance.
    wr(EMC_BASE + EMC_XM2COMPPADCTRL, p.emc_xm2_comp_pad_ctrl);
    wr(EMC_BASE + EMC_XM2COMPPADCTRL2, p.emc_xm2_comp_pad_ctrl2);
    wr(EMC_BASE + EMC_XM2COMPPADCTRL3, p.emc_xm2_comp_pad_ctrl3);

    // Program Autocal controls.
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG2, p.emc_auto_cal_config2);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG3, p.emc_auto_cal_config3);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG4, p.emc_auto_cal_config4);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG5, p.emc_auto_cal_config5);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG6, p.emc_auto_cal_config6);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG7, p.emc_auto_cal_config7);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG8, p.emc_auto_cal_config8);

    // Program termination and drive strength
    wr(EMC_BASE + EMC_PMACRO_RX_TERM, p.emc_pmacro_rx_term);
    wr(EMC_BASE + EMC_PMACRO_DQ_TX_DRV, p.emc_pmacro_dq_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_CA_TX_DRV, p.emc_pmacro_ca_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_CMD_TX_DRV, p.emc_pmacro_cmd_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_COMMON, p.emc_pmacro_auto_cal_common);
    wr(EMC_BASE + EMC_AUTO_CAL_CHANNEL, p.emc_auto_cal_channel);
    wr(EMC_BASE + EMC_PMACRO_ZCTRL, p.emc_pmacro_zcrtl);

    // Program dll config.
    wr(EMC_BASE + EMC_DLL_CFG_0, p.emc_dll_cfg0);
    wr(EMC_BASE + EMC_DLL_CFG_1, p.emc_dll_cfg1);
    wr(EMC_BASE + EMC_CFG_DIG_DLL_1, p.emc_cfg_dig_dll_1);

    // Program barrelshift.
    wr(EMC_BASE + EMC_DATA_BRLSHFT_0, p.emc_data_brlshft0);
    wr(EMC_BASE + EMC_DATA_BRLSHFT_1, p.emc_data_brlshft1);
    wr(EMC_BASE + EMC_DQS_BRLSHFT_0, p.emc_dqs_brlshft0);
    wr(EMC_BASE + EMC_DQS_BRLSHFT_1, p.emc_dqs_brlshft1);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_0, p.emc_cmd_brlshft0);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_1, p.emc_cmd_brlshft1);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_2, p.emc_cmd_brlshft2);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_3, p.emc_cmd_brlshft3);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_0, p.emc_quse_brlshft0);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_1, p.emc_quse_brlshft1);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_2, p.emc_quse_brlshft2);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_3, p.emc_quse_brlshft3);

    // Program pad macros controls and termination.
    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU1, (p.emc_pmacro_brick_ctrl_rfu1 & 0x1BF01BF) | 0x1E401E40);
    wr(EMC_BASE + EMC_PMACRO_PAD_CFG_CTRL, p.emc_pmacro_pad_cfg_ctrl);

    wr(EMC_BASE + EMC_PMACRO_CMD_BRICK_CTRL_FDPD, p.emc_pmacro_cmd_brick_ctrl_fdpd);
    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU2, p.emc_pmacro_brick_ctrl_rfu2 & 0xFF7FFF7F);
    wr(EMC_BASE + EMC_PMACRO_DATA_BRICK_CTRL_FDPD, p.emc_pmacro_data_brick_ctrl_fdpd);
    wr(EMC_BASE + EMC_PMACRO_BG_BIAS_CTRL_0, p.emc_pmacro_bg_bias_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_DATA_PAD_RX_CTRL, p.emc_pmacro_data_pad_rx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_CMD_PAD_RX_CTRL, p.emc_pmacro_cmd_pad_rx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_DATA_PAD_TX_CTRL, p.emc_pmacro_data_pad_tx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_DATA_RX_TERM_MODE, p.emc_pmacro_data_rx_term_mode);
    wr(EMC_BASE + EMC_PMACRO_CMD_RX_TERM_MODE, p.emc_pmacro_cmd_rx_term_mode);
    wr(EMC_BASE + EMC_PMACRO_CMD_PAD_TX_CTRL, p.emc_pmacro_cmd_pad_tx_ctrl);

    // Program pad macro pins/bytes.
    wr(EMC_BASE + EMC_CFG_3, p.emc_cfg3);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_0, p.emc_pmacro_tx_pwrd0);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_1, p.emc_pmacro_tx_pwrd1);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_2, p.emc_pmacro_tx_pwrd2);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_3, p.emc_pmacro_tx_pwrd3);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_4, p.emc_pmacro_tx_pwrd4);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_5, p.emc_pmacro_tx_pwrd5);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_0, p.emc_pmacro_tx_sel_clk_src0);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_1, p.emc_pmacro_tx_sel_clk_src1);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_2, p.emc_pmacro_tx_sel_clk_src2);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_3, p.emc_pmacro_tx_sel_clk_src3);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_4, p.emc_pmacro_tx_sel_clk_src4);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_5, p.emc_pmacro_tx_sel_clk_src5);
    wr(EMC_BASE + EMC_PMACRO_DDLL_BYPASS, p.emc_pmacro_ddll_bypass);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_0, p.emc_pmacro_ddll_pwrd0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_1, p.emc_pmacro_ddll_pwrd1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_2, p.emc_pmacro_ddll_pwrd2);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_0, p.emc_pmacro_cmd_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_1, p.emc_pmacro_cmd_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_2, p.emc_pmacro_cmd_ctrl2);

    // Program inbound vref setting.
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQ_0, p.emc_pmacro_ib_vref_dq_0);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQ_1, p.emc_pmacro_ib_vref_dq_1);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQS_0, p.emc_pmacro_ib_vref_dqs_0);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQS_1, p.emc_pmacro_ib_vref_dqs_1);
    wr(EMC_BASE + EMC_PMACRO_IB_RXRT, p.emc_pmacro_ib_rxrt);

    // Program quse trimmers.
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_0, p.emc_pmacro_quse_ddll_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_1, p.emc_pmacro_quse_ddll_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_2, p.emc_pmacro_quse_ddll_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_3, p.emc_pmacro_quse_ddll_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_4, p.emc_pmacro_quse_ddll_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_5, p.emc_pmacro_quse_ddll_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_0, p.emc_pmacro_quse_ddll_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_1, p.emc_pmacro_quse_ddll_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_2, p.emc_pmacro_quse_ddll_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_3, p.emc_pmacro_quse_ddll_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_4, p.emc_pmacro_quse_ddll_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_5, p.emc_pmacro_quse_ddll_rank1_5);
    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU1, p.emc_pmacro_brick_ctrl_rfu1);

    // Program outbound trimmers.
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0, p.emc_pmacro_ob_ddll_long_dq_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1, p.emc_pmacro_ob_ddll_long_dq_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2, p.emc_pmacro_ob_ddll_long_dq_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3, p.emc_pmacro_ob_ddll_long_dq_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_4, p.emc_pmacro_ob_ddll_long_dq_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_5, p.emc_pmacro_ob_ddll_long_dq_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0, p.emc_pmacro_ob_ddll_long_dq_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1, p.emc_pmacro_ob_ddll_long_dq_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2, p.emc_pmacro_ob_ddll_long_dq_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3, p.emc_pmacro_ob_ddll_long_dq_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_4, p.emc_pmacro_ob_ddll_long_dq_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_5, p.emc_pmacro_ob_ddll_long_dq_rank1_5);

    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_0, p.emc_pmacro_ob_ddll_long_dqs_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_1, p.emc_pmacro_ob_ddll_long_dqs_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_2, p.emc_pmacro_ob_ddll_long_dqs_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_3, p.emc_pmacro_ob_ddll_long_dqs_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_4, p.emc_pmacro_ob_ddll_long_dqs_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_5, p.emc_pmacro_ob_ddll_long_dqs_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_0, p.emc_pmacro_ob_ddll_long_dqs_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_1, p.emc_pmacro_ob_ddll_long_dqs_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_2, p.emc_pmacro_ob_ddll_long_dqs_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_3, p.emc_pmacro_ob_ddll_long_dqs_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_4, p.emc_pmacro_ob_ddll_long_dqs_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_5, p.emc_pmacro_ob_ddll_long_dqs_rank1_5);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_0, p.emc_pmacro_ib_ddll_long_dqs_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_1, p.emc_pmacro_ib_ddll_long_dqs_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_2, p.emc_pmacro_ib_ddll_long_dqs_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_3, p.emc_pmacro_ib_ddll_long_dqs_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_0, p.emc_pmacro_ib_ddll_long_dqs_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_1, p.emc_pmacro_ib_ddll_long_dqs_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_2, p.emc_pmacro_ib_ddll_long_dqs_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_3, p.emc_pmacro_ib_ddll_long_dqs_rank1_3);

    // Program clock trimmers.
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_0, p.emc_pmacro_ddll_long_cmd_0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_1, p.emc_pmacro_ddll_long_cmd_1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_2, p.emc_pmacro_ddll_long_cmd_2);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_3, p.emc_pmacro_ddll_long_cmd_3);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_4, p.emc_pmacro_ddll_long_cmd_4);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_0, p.emc_pmacro_ddll_short_cmd_0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_1, p.emc_pmacro_ddll_short_cmd_1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_2, p.emc_pmacro_ddll_short_cmd_2);

    // Common pad macro (cpm).
    wr(EMC_BASE + EMC_PMACRO_COMMON_PAD_TX_CTRL, (p.emc_pmacro_common_pad_tx_ctrl & 1) | 0xE);

    // Patch 4 using BCT spare variables.
    if (p.emc_bct_spare4) != 0 {
        wr(p.emc_bct_spare4, p.emc_bct_spare5);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    // Initialize MC VPR settings.
    wr(MC_BASE + MC_VIDEO_PROTECT_BOM, p.mc_video_protect_bom);
    wr(MC_BASE + MC_VIDEO_PROTECT_BOM_ADR_HI, p.mc_video_protect_bom_adr_hi);
    wr(MC_BASE + MC_VIDEO_PROTECT_SIZE_MB, p.mc_video_protect_size_mb);
    wr(MC_BASE + MC_VIDEO_PROTECT_VPR_OVERRIDE, p.mc_video_protect_vpr_override);
    wr(MC_BASE + MC_VIDEO_PROTECT_VPR_OVERRIDE1, p.mc_video_protect_vpr_override1);
    wr(MC_BASE + MC_VIDEO_PROTECT_GPU_OVERRIDE_0, p.mc_video_protect_gpu_override0);
    wr(MC_BASE + MC_VIDEO_PROTECT_GPU_OVERRIDE_1, p.mc_video_protect_gpu_override1);

    // Program SDRAM geometry parameters.
    wr(MC_BASE + MC_EMEM_ADR_CFG, p.mc_emem_adr_cfg);
    wr(MC_BASE + MC_EMEM_ADR_CFG_DEV0, p.mc_emem_adr_cfg_dev0);
    wr(MC_BASE + MC_EMEM_ADR_CFG_DEV1, p.mc_emem_adr_cfg_dev1);
    wr(MC_BASE + MC_EMEM_ADR_CFG_CHANNEL_MASK, p.mc_emem_adr_cfg_channel_mask);

    // Program bank swizzling.
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_0, p.mc_emem_adr_cfg_bank_mask0);
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_1, p.mc_emem_adr_cfg_bank_mask1);
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_2, p.mc_emem_adr_cfg_bank_mask2);

    // Program external memory aperture (base and size).
    wr(MC_BASE + MC_EMEM_CFG, p.mc_emem_cfg);

    // Program SEC carveout (base and size).
    wr(MC_BASE + MC_SEC_CARVEOUT_BOM, p.mc_sec_carveout_bom);
    wr(MC_BASE + MC_SEC_CARVEOUT_ADR_HI, p.mc_sec_carveout_adr_hi);
    wr(MC_BASE + MC_SEC_CARVEOUT_SIZE_MB, p.mc_sec_carveout_size_mb);

    // Program MTS carveout (base and size).
    wr(MC_BASE + MC_MTS_CARVEOUT_BOM, p.mc_mts_carveout_bom);
    wr(MC_BASE + MC_MTS_CARVEOUT_ADR_HI, p.mc_mts_carveout_adr_hi);
    wr(MC_BASE + MC_MTS_CARVEOUT_SIZE_MB, p.mc_mts_carveout_size_mb);

    // Program the memory arbiter.
    wr(MC_BASE + MC_EMEM_ARB_CFG, p.mc_emem_arb_cfg);
    wr(MC_BASE + MC_EMEM_ARB_OUTSTANDING_REQ, p.mc_emem_arb_outstanding_req);
    wr(MC_BASE + MC_EMEM_ARB_REFPB_HP_CTRL, p.emc_emem_arb_refpb_hp_ctrl);
    wr(MC_BASE + MC_EMEM_ARB_REFPB_BANK_CTRL, p.emc_emem_arb_refpb_bank_ctrl);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RCD, p.mc_emem_arb_timing_rcd);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RP, p.mc_emem_arb_timing_rp);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RC, p.mc_emem_arb_timing_rc);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RAS, p.mc_emem_arb_timing_ras);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_FAW, p.mc_emem_arb_timing_faw);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RRD, p.mc_emem_arb_timing_rrd);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RAP2PRE, p.mc_emem_arb_timing_rap2pre);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_WAP2PRE, p.mc_emem_arb_timing_wap2pre);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_R2R, p.mc_emem_arb_timing_r2r);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_W2W, p.mc_emem_arb_timing_w2w);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_CCDMW, p.mc_emem_arb_timing_ccdmw);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_R2W, p.mc_emem_arb_timing_r2w);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_W2R, p.mc_emem_arb_timing_w2r);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RFCPB, p.mc_emem_arb_timing_rfcpb);
    wr(MC_BASE + MC_EMEM_ARB_DA_TURNS, p.mc_emem_arb_da_turns);
    wr(MC_BASE + MC_EMEM_ARB_DA_COVERS, p.mc_emem_arb_da_covers);
    wr(MC_BASE + MC_EMEM_ARB_MISC0, p.mc_emem_arb_misc0);
    wr(MC_BASE + MC_EMEM_ARB_MISC1, p.mc_emem_arb_misc1);
    wr(MC_BASE + MC_EMEM_ARB_MISC2, p.mc_emem_arb_misc2);
    wr(MC_BASE + MC_EMEM_ARB_RING1_THROTTLE, p.mc_emem_arb_ring1_throttle);
    wr(MC_BASE + MC_EMEM_ARB_OVERRIDE, p.mc_emem_arb_override);
    wr(MC_BASE + MC_EMEM_ARB_OVERRIDE_1, p.mc_emem_arb_override1);
    wr(MC_BASE + MC_EMEM_ARB_RSV, p.mc_emem_arb_rsv);
    wr(MC_BASE + MC_DA_CONFIG0, p.mc_da_cfg0);

    // Trigger MC timing update.
    wr(MC_BASE + MC_TIMING_CONTROL, 1);

    // Program second-level clock enable overrides.
    wr(MC_BASE + MC_CLKEN_OVERRIDE, p.mc_clken_override);

    // Program statistics gathering.
    wr(MC_BASE + MC_STAT_CONTROL, p.mc_stat_control);

    // Program SDRAM geometry parameters.
    wr(EMC_BASE + EMC_ADR_CFG, p.emc_adr_cfg);

    // Program second-level clock enable overrides.
    wr(EMC_BASE + EMC_CLKEN_OVERRIDE, p.emc_clken_override);

    // Program EMC pad auto calibration.
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_0, p.emc_pmacro_auto_cal_cfg0);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_1, p.emc_pmacro_auto_cal_cfg1);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_2, p.emc_pmacro_auto_cal_cfg2);

    wr(EMC_BASE + EMC_AUTO_CAL_VREF_SEL_0, p.emc_auto_cal_vref_sel0);
    wr(EMC_BASE + EMC_AUTO_CAL_VREF_SEL_1, p.emc_auto_cal_vref_sel1);

    // Program/Start auto calibration.
    wr(EMC_BASE + EMC_AUTO_CAL_INTERVAL, p.emc_auto_cal_interval);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG, p.emc_auto_cal_config);
    usleep(p.emc_auto_cal_wait);

    // Patch 5 using BCT spare variables.
    if (p.emc_bct_spare8) != 0 {
        wr(p.emc_bct_spare8, p.emc_bct_spare9);
    }

    // Program EMC timing configuration.
    wr(EMC_BASE + EMC_CFG_2, p.emc_cfg2);
    wr(EMC_BASE + EMC_CFG_PIPE, p.emc_cfg_pipe);
    wr(EMC_BASE + EMC_CFG_PIPE_1, p.emc_cfg_pipe1);
    wr(EMC_BASE + EMC_CFG_PIPE_2, p.emc_cfg_pipe2);
    wr(EMC_BASE + EMC_CMDQ, p.emc_cmd_q);
    wr(EMC_BASE + EMC_MC2EMCQ, p.emc_mc2emc_q);
    wr(EMC_BASE + EMC_MRS_WAIT_CNT, p.emc_mrs_wait_cnt);
    wr(EMC_BASE + EMC_MRS_WAIT_CNT2, p.emc_mrs_wait_cnt2);
    wr(EMC_BASE + EMC_FBIO_CFG5, p.emc_fbio_cfg5);
    wr(EMC_BASE + EMC_RC, p.emc_rc);
    wr(EMC_BASE + EMC_RFC, p.emc_rfc);
    wr(EMC_BASE + EMC_RFCPB, p.emc_rfc_pb);
    wr(EMC_BASE + EMC_REFCTRL2, p.emc_ref_ctrl2);
    wr(EMC_BASE + EMC_RFC_SLR, p.emc_rfc_slr);
    wr(EMC_BASE + EMC_RAS, p.emc_ras);
    wr(EMC_BASE + EMC_RP, p.emc_rp);
    wr(EMC_BASE + EMC_TPPD, p.emc_tppd);
    wr(EMC_BASE + EMC_R2R, p.emc_r2r);
    wr(EMC_BASE + EMC_W2W, p.emc_w2w);
    wr(EMC_BASE + EMC_R2W, p.emc_r2w);
    wr(EMC_BASE + EMC_W2R, p.emc_w2r);
    wr(EMC_BASE + EMC_R2P, p.emc_r2p);
    wr(EMC_BASE + EMC_W2P, p.emc_w2p);
    wr(EMC_BASE + EMC_CCDMW, p.emc_ccdmw);
    wr(EMC_BASE + EMC_RD_RCD, p.emc_rd_rcd);
    wr(EMC_BASE + EMC_WR_RCD, p.emc_wr_rcd);
    wr(EMC_BASE + EMC_RRD, p.emc_rrd);
    wr(EMC_BASE + EMC_REXT, p.emc_rext);
    wr(EMC_BASE + EMC_WEXT, p.emc_wext);
    wr(EMC_BASE + EMC_WDV, p.emc_wdv);
    wr(EMC_BASE + EMC_WDV_CHK, p.emc_wdv_chk);
    wr(EMC_BASE + EMC_WSV, p.emc_wsv);
    wr(EMC_BASE + EMC_WEV, p.emc_wev);
    wr(EMC_BASE + EMC_WDV_MASK, p.emc_wdv_mask);
    wr(EMC_BASE + EMC_WS_DURATION, p.emc_ws_duration);
    wr(EMC_BASE + EMC_WE_DURATION, p.emc_we_duration);
    wr(EMC_BASE + EMC_QUSE, p.emc_quse);
    wr(EMC_BASE + EMC_QUSE_WIDTH, p.emc_quse_width);
    wr(EMC_BASE + EMC_IBDLY, p.emc_ibdly);
    wr(EMC_BASE + EMC_OBDLY, p.emc_obdly);
    wr(EMC_BASE + EMC_EINPUT, p.emc_einput);
    wr(EMC_BASE + EMC_EINPUT_DURATION, p.emc_einput_duration);
    wr(EMC_BASE + EMC_PUTERM_EXTRA, p.emc_puterm_extra);
    wr(EMC_BASE + EMC_PUTERM_WIDTH, p.emc_puterm_width);

    wr(EMC_BASE + EMC_PMACRO_COMMON_PAD_TX_CTRL, p.emc_pmacro_common_pad_tx_ctrl);
    wr(EMC_BASE + EMC_DBG, p.emc_dbg);

    // Clear read fifo.
    wr(EMC_BASE + EMC_QRST, p.emc_qrst);
    wr(EMC_BASE + EMC_ISSUE_QRST, 1);
    wr(EMC_BASE + EMC_ISSUE_QRST, 0);

    // Program the rest of EMC timing configuration.
    wr(EMC_BASE + EMC_QSAFE, p.emc_qsafe);
    wr(EMC_BASE + EMC_RDV, p.emc_rdv);
    wr(EMC_BASE + EMC_RDV_MASK, p.emc_rdv_mask);
    wr(EMC_BASE + EMC_RDV_EARLY, p.emc_rdv_early);
    wr(EMC_BASE + EMC_RDV_EARLY_MASK, p.emc_rdv_early_mask);
    wr(EMC_BASE + EMC_QPOP, p.emc_qpop);
    wr(EMC_BASE + EMC_REFRESH, p.emc_refresh);
    wr(EMC_BASE + EMC_BURST_REFRESH_NUM, p.emc_burst_refresh_num);
    wr(EMC_BASE + EMC_PRE_REFRESH_REQ_CNT, p.emc_prerefresh_req_cnt);
    wr(EMC_BASE + EMC_PDEX2WR, p.emc_pdex2wr);
    wr(EMC_BASE + EMC_PDEX2RD, p.emc_pdex2rd);
    wr(EMC_BASE + EMC_PCHG2PDEN, p.emc_pchg2pden);
    wr(EMC_BASE + EMC_ACT2PDEN, p.emc_act2pden);
    wr(EMC_BASE + EMC_AR2PDEN, p.emc_ar2pden);
    wr(EMC_BASE + EMC_RW2PDEN, p.emc_rw2pden);
    wr(EMC_BASE + EMC_CKE2PDEN, p.emc_cke2pden);
    wr(EMC_BASE + EMC_PDEX2CKE, p.emc_pdex2che);
    wr(EMC_BASE + EMC_PDEX2MRR, p.emc_pdex2mrr);
    wr(EMC_BASE + EMC_TXSR, p.emc_txsr);
    wr(EMC_BASE + EMC_TXSRDLL, p.emc_txsr_dll);
    wr(EMC_BASE + EMC_TCKE, p.emc_tcke);
    wr(EMC_BASE + EMC_TCKESR, p.emc_tckesr);
    wr(EMC_BASE + EMC_TPD, p.emc_tpd);
    wr(EMC_BASE + EMC_TFAW, p.emc_tfaw);
    wr(EMC_BASE + EMC_TRPAB, p.emc_trpab);
    wr(EMC_BASE + EMC_TCLKSTABLE, p.emc_tclkstable);
    wr(EMC_BASE + EMC_TCLKSTOP, p.emc_tclkstop);
    wr(EMC_BASE + EMC_TREFBW, p.emc_trefbw);
    wr(EMC_BASE + EMC_ODT_WRITE, p.emc_odt_write);
    wr(EMC_BASE + EMC_CFG_DIG_DLL, p.emc_cfg_dig_dll);
    wr(EMC_BASE + EMC_CFG_DIG_DLL_PERIOD, p.emc_cfg_dig_dll_period);

    // Don't write CFG_ADR_EN (bit 1) here - lock bit written later.
    wr(EMC_BASE + EMC_FBIO_SPARE, p.emc_fbio_spare & 0xFFFFFFFD);
    wr(EMC_BASE + EMC_CFG_RSV, p.emc_cfg_rsv);
    wr(EMC_BASE + EMC_PMC_SCRATCH1, p.emc_pmc_scratch1);
    wr(EMC_BASE + EMC_PMC_SCRATCH2, p.emc_pmc_scratch2);
    wr(EMC_BASE + EMC_PMC_SCRATCH3, p.emc_pmc_scratch3);
    wr(EMC_BASE + EMC_ACPD_CONTROL, p.emc_acpd_control);
    wr(EMC_BASE + EMC_TXDSRVTTGEN, p.emc_txdsrvttgen);

    // Set pipe bypass enable bits before sending any DRAM commands.
    wr(EMC_BASE + EMC_CFG, (p.emc_cfg & 0xE) | 0x3C00000);

    // Patch BootROM.
    if (p.boot_rom_patch_control & (1 << 31)) != 0 {
        wr(APB_MISC_BASE + p.boot_rom_patch_control * 4, p.boot_rom_patch_data);

        // Trigger MC timing update.
        wr(MC_BASE + MC_TIMING_CONTROL, 1);
    }

    // Release SEL_DPD_CMD.
    wr(PMC_BASE + APBDEV_PMC_IO_DPD3_REQ, (p.emc_pmc_scratch1 & 0xFFF0000) | PMC_IO_DPD_REQ_DPD_OFF);
    usleep(p.pmc_io_dpd3_req_wait);

    // Stall auto call measurements if periodic calibration is disabled.
    if p.emc_auto_cal_interval == 0 {
        wr(EMC_BASE + EMC_AUTO_CAL_CONFIG, p.emc_auto_cal_config | 0x200);
    }

    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU2, p.emc_pmacro_brick_ctrl_rfu2);

    // ZQ CAL setup (not actually issuing ZQ CAL now).
    if (p.emc_zcal_warm_cold_boot_enables & 1) != 0 {
        wr(EMC_BASE + EMC_ZCAL_WAIT_CNT, p.emc_zcal_wait_cnt);
        wr(EMC_BASE + EMC_ZCAL_MRW_CMD, p.emc_zcal_mrw_cmd);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);
    usleep(p.emc_timing_control_wait);

    // Deassert HOLD_CKE_LOW.
    wr(PMC_BASE + APBDEV_PMC_DDR_CNTRL, rd(PMC_BASE + APBDEV_PMC_DDR_CNTRL) & (0xFFF8007F));
    usleep(p.pmc_ddr_ctrl_wait);

    // Set clock enable signal.
    let mut pin_gpio_cfg: u32 = (p.emc_pin_gpio_enable << 16) | (p.emc_pin_gpio << 12);
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg);
    let _ = rd(EMC_BASE + EMC_PIN);
    usleep(p.emc_pin_extra_wait + 200);
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg | 0x100);
    let _ = rd(EMC_BASE + EMC_PIN);

    usleep(p.emc_pin_extra_wait + 2000);

    // Enable clock enable signal.
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg | 0x101);
    let _ = rd(EMC_BASE + EMC_PIN);
    usleep(p.emc_pin_program_wait);

    // Init zq calibration,
    // Patch 6 using BCT spare variables.
    if (p.emc_bct_spare10) != 0 {
        wr(p.emc_bct_spare10, p.emc_bct_spare11);
    }

    // Write mode registers.
    wr(EMC_BASE + EMC_MRW2, p.emc_mrw2);
    wr(EMC_BASE + EMC_MRW, p.emc_mrw1);
    wr(EMC_BASE + EMC_MRW3, p.emc_mrw3);
    wr(EMC_BASE + EMC_MRW4, p.emc_mrw4);
    wr(EMC_BASE + EMC_MRW6, p.emc_mrw6);
    wr(EMC_BASE + EMC_MRW14, p.emc_mrw14);

    wr(EMC_BASE + EMC_MRW8, p.emc_mrw8);
    wr(EMC_BASE + EMC_MRW12, p.emc_mrw12);
    wr(EMC_BASE + EMC_MRW9, p.emc_mrw9);
    wr(EMC_BASE + EMC_MRW13, p.emc_mrw13);

    if (p.emc_zcal_warm_cold_boot_enables & 1) != 0 {
        // Issue ZQCAL start, device 0.
        wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev0);
        usleep(p.emc_zcal_init_wait);

        // Issue ZQCAL latch.
        wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev0 ^ 3);
        // Same for device 1.
        if (p.emc_dev_select & 2) == 0 {
            wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev1);
            usleep(p.emc_zcal_init_wait);
            wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev1 ^ 3);
        }
    }

    // Set package and DPD pad control.
    wr(PMC_BASE + APBDEV_PMC_DDR_CFG, p.pmc_ddr_cfg);

    // Start periodic ZQ calibration (LPDDRx only).
    wr(EMC_BASE + EMC_ZCAL_INTERVAL, p.emc_zcal_interval);
    wr(EMC_BASE + EMC_ZCAL_WAIT_CNT, p.emc_zcal_wait_cnt);
    wr(EMC_BASE + EMC_ZCAL_MRW_CMD, p.emc_zcal_mrw_cmd);

    // Patch 7 using BCT spare variables.
    if (p.emc_bct_spare12) != 0 {
        wr(p.emc_bct_spare12, p.emc_bct_spare13);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    if (p.emc_extra_refresh_num) != 0 {
        wr(EMC_BASE + EMC_REF, (((1 << p.emc_extra_refresh_num) - 1) << 8) | (p.emc_dev_select << 30) | 3);
    }

    // Enable refresh.
    wr(EMC_BASE + EMC_REFCTRL, p.emc_dev_select | (1 << 31));

    wr(EMC_BASE + EMC_DYN_SELF_REF_CONTROL, p.emc_dyn_self_ref_control);
    wr(EMC_BASE + EMC_CFG_UPDATE, p.emc_cfg_update);
    wr(EMC_BASE + EMC_CFG, p.emc_cfg);
    wr(EMC_BASE + EMC_FDPD_CTRL_DQ, p.emc_fdpd_ctrl_dq);
    wr(EMC_BASE + EMC_FDPD_CTRL_CMD, p.emc_fdpd_ctrl_cmd);
    wr(EMC_BASE + EMC_SEL_DPD_CTRL, p.emc_sel_dpd_ctrl);

    // Write addr swizzle lock bit.
    wr(EMC_BASE + EMC_FBIO_SPARE, p.emc_fbio_spare | (1 << 1));

    // Re-trigger timing to latch power saving functions.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    // Enable EMC pipe clock gating.
    wr(EMC_BASE + EMC_CFG_PIPE_CLK, p.emc_cfg_pipe_clk);

    // Depending on freqency, enable CMD/CLK fdpd.
    wr(EMC_BASE + EMC_FDPD_CTRL_CMD_NO_RAMP, p.emc_fdpd_ctrl_cmd_no_ramp);

    // Enable arbiter.
    wr(SYSREG_BASE + AHB_ARBITRATION_XBAR_CTRL, (rd(SYSREG_BASE + AHB_ARBITRATION_XBAR_CTRL) & 0xFFFEFFFF) | (p.ahb_arbitration_xbar_ctrl_meminit_done << 16));

    // Lock carveouts per BCT cfg.
    wr(MC_BASE + MC_VIDEO_PROTECT_REG_CTRL, p.mc_video_protect_write_access);
    wr(MC_BASE + MC_SEC_CARVEOUT_REG_CTRL, p.mc_sec_carveout_protect_write_access);
    wr(MC_BASE + MC_MTS_CARVEOUT_REG_CTRL, p.mc_mts_carveout_reg_ctrl);

    // Disable write access to a bunch of EMC registers.
    wr(MC_BASE + MC_EMEM_CFG_ACCESS_CTRL, 1);
}

pub fn config_t210b01(p: &ParamsT210b01) {
    // VDDP Select.
    wr(PMC_BASE + APBDEV_PMC_VDDP_SEL, p.pmc_vddp_sel);
    usleep(p.pmc_vddp_sel_wait);

    // Turn on MEM IO Power.
    wr(PMC_BASE + APBDEV_PMC_NO_IOPOWER, rd(PMC_BASE + APBDEV_PMC_NO_IOPOWER) & (PMC_NO_IOPOWER_SDMMC1)); // Only keep SDMMC1 state. (Was params->pmc_no_io_power).
    wr(PMC_BASE + APBDEV_PMC_REG_SHORT, p.pmc_reg_short);

    wr(PMC_BASE + APBDEV_PMC_DDR_CNTRL, p.pmc_ddr_ctrl);

    // Patch 1 using BCT spare variables
    if (p.emc_bct_spare0) != 0 {
        wr(p.emc_bct_spare0, p.emc_bct_spare1);
    }

    // Override HW FSM if needed.
    if (p.clk_rst_pllm_misc20_override_enable) != 0 {
        wr(CLOCK_BASE + CLK_RST_CONTROLLER_PLLM_MISC2, p.clk_rst_pllm_misc20_override);
    }

    // Program DPD3/DPD4 regs (coldboot path).
    // Enable sel_dpd on unused pins.
    let mut pmc_scratch1: u32 = !p.emc_pmc_scratch1;
    wr(PMC_BASE + APBDEV_PMC_WEAK_BIAS, (pmc_scratch1 & 0x1000) << 19 | (pmc_scratch1 & 0xFFF) << 18 | (pmc_scratch1 & 0x8000) << 15);
    wr(PMC_BASE + APBDEV_PMC_IO_DPD3_REQ, (pmc_scratch1 & 0x9FFF) | PMC_IO_DPD_REQ_DPD_ON);
    usleep(p.pmc_io_dpd3_req_wait);

    // Disable e_dpd_vttgen.
    let mut pmc_scratch2: u32 = !p.emc_pmc_scratch2;
    wr(PMC_BASE + APBDEV_PMC_IO_DPD4_REQ, (pmc_scratch2 & 0x3FFF0000) | PMC_IO_DPD_REQ_DPD_ON);
    usleep(p.pmc_io_dpd4_req_wait);

    // Disable e_dpd_bg.
    wr(PMC_BASE + APBDEV_PMC_IO_DPD4_REQ, (pmc_scratch2 & 0x1FFF) | PMC_IO_DPD_REQ_DPD_ON);
    usleep(1);

    // Program CMD mapping. Required before brick mapping, else
    // we can't guarantee CK will be differential at all times.
    wr(EMC_BASE + EMC_FBIO_CFG7, p.emc_fbio_cfg7);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_0, p.emc_cmd_mapping_cmd0_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_1, p.emc_cmd_mapping_cmd0_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD0_2, p.emc_cmd_mapping_cmd0_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_0, p.emc_cmd_mapping_cmd1_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_1, p.emc_cmd_mapping_cmd1_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD1_2, p.emc_cmd_mapping_cmd1_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_0, p.emc_cmd_mapping_cmd2_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_1, p.emc_cmd_mapping_cmd2_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD2_2, p.emc_cmd_mapping_cmd2_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_0, p.emc_cmd_mapping_cmd3_0);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_1, p.emc_cmd_mapping_cmd3_1);
    wr(EMC_BASE + EMC_CMD_MAPPING_CMD3_2, p.emc_cmd_mapping_cmd3_2);
    wr(EMC_BASE + EMC_CMD_MAPPING_BYTE, p.emc_cmd_mapping_byte);

    // Program brick mapping.
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_0, p.emc_pmacro_brick_mapping0);
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_1, p.emc_pmacro_brick_mapping1);
    wr(EMC_BASE + EMC_PMACRO_BRICK_MAPPING_2, p.emc_pmacro_brick_mapping2);

    // Set pad macros.
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_0, p.emc_pmacro_vttgen_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_1, p.emc_pmacro_vttgen_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_VTTGEN_CTRL_2, p.emc_pmacro_vttgen_ctrl2);

    // Set pad macros bias.
    wr(EMC_BASE + EMC_PMACRO_BG_BIAS_CTRL_0, p.emc_pmacro_bg_bias_ctrl0);

    // Patch 1 to 3 using BCT spare secure variables.
    if (p.emc_bct_spare_secure0) != 0 {
        wr(p.emc_bct_spare_secure0, p.emc_bct_spare_secure1);
    }
    if (p.emc_bct_spare_secure2) != 0 {
        wr(p.emc_bct_spare_secure2, p.emc_bct_spare_secure3);
    }
    if (p.emc_bct_spare_secure4) != 0 {
        wr(p.emc_bct_spare_secure4, p.emc_bct_spare_secure5);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);
    usleep(p.pmc_vddp_sel_wait + 2); // Ensure the regulators settle.

    // Set clock sources.
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_SOURCE_EMC, p.emc_clock_source);
    wr(CLOCK_BASE + CLK_RST_CONTROLLER_CLK_SOURCE_EMC_DLL, p.emc_clock_source_dll);

    // Select EMC write mux.
    wr(EMC_BASE + EMC_DBG, (p.emc_dbg_write_mux << 1) | p.emc_dbg);

    // Patch 2 using BCT spare variables.
    if (p.emc_bct_spare2) != 0 {
        wr(p.emc_bct_spare2, p.emc_bct_spare3);
    }

    // This is required to do any reads from the pad macros.
    wr(EMC_BASE + EMC_CONFIG_SAMPLE_DELAY, p.emc_config_sample_delay);

    // Set data pipes mode.
    wr(EMC_BASE + EMC_FBIO_CFG8, p.emc_fbio_cfg8);

    // Set swizzle for Rank 0.
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE0, p.emc_swizzle_rank0_byte0);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE1, p.emc_swizzle_rank0_byte1);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE2, p.emc_swizzle_rank0_byte2);
    wr(EMC_BASE + EMC_SWIZZLE_RANK0_BYTE3, p.emc_swizzle_rank0_byte3);
    // Set swizzle for Rank 1.
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE0, p.emc_swizzle_rank1_byte0);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE1, p.emc_swizzle_rank1_byte1);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE2, p.emc_swizzle_rank1_byte2);
    wr(EMC_BASE + EMC_SWIZZLE_RANK1_BYTE3, p.emc_swizzle_rank1_byte3);

    // Patch 3 using BCT spare variables.
    if (p.emc_bct_spare6) != 0 {
        wr(p.emc_bct_spare6, p.emc_bct_spare7);
    }

    // Program calibration impedance.
    wr(EMC_BASE + EMC_XM2COMPPADCTRL, p.emc_xm2_comp_pad_ctrl);
    wr(EMC_BASE + EMC_XM2COMPPADCTRL2, p.emc_xm2_comp_pad_ctrl2);
    wr(EMC_BASE + EMC_XM2COMPPADCTRL3, p.emc_xm2_comp_pad_ctrl3);

    // Program Autocal controls.
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG2, p.emc_auto_cal_config2);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG3, p.emc_auto_cal_config3);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG4, p.emc_auto_cal_config4);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG5, p.emc_auto_cal_config5);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG6, p.emc_auto_cal_config6);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG7, p.emc_auto_cal_config7);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG8, p.emc_auto_cal_config8);

    // Program termination and drive strength
    wr(EMC_BASE + EMC_PMACRO_RX_TERM, p.emc_pmacro_rx_term);
    wr(EMC_BASE + EMC_PMACRO_DQ_TX_DRV, p.emc_pmacro_dq_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_CA_TX_DRV, p.emc_pmacro_ca_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_CMD_TX_DRV, p.emc_pmacro_cmd_tx_drive);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_COMMON, p.emc_pmacro_auto_cal_common);
    wr(EMC_BASE + EMC_AUTO_CAL_CHANNEL, p.emc_auto_cal_channel);
    wr(EMC_BASE + EMC_PMACRO_ZCTRL, p.emc_pmacro_zcrtl);

    // Program dll config.
    wr(EMC_BASE + EMC_DLL_CFG_0, p.emc_dll_cfg0);
    wr(EMC_BASE + EMC_DLL_CFG_1, p.emc_dll_cfg1);
    wr(EMC_BASE + EMC_CFG_DIG_DLL_1, p.emc_cfg_dig_dll_1);

    // Program barrelshift.
    wr(EMC_BASE + EMC_DATA_BRLSHFT_0, p.emc_data_brlshft0);
    wr(EMC_BASE + EMC_DATA_BRLSHFT_1, p.emc_data_brlshft1);
    wr(EMC_BASE + EMC_DQS_BRLSHFT_0, p.emc_dqs_brlshft0);
    wr(EMC_BASE + EMC_DQS_BRLSHFT_1, p.emc_dqs_brlshft1);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_0, p.emc_cmd_brlshft0);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_1, p.emc_cmd_brlshft1);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_2, p.emc_cmd_brlshft2);
    wr(EMC_BASE + EMC_CMD_BRLSHFT_3, p.emc_cmd_brlshft3);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_0, p.emc_quse_brlshft0);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_1, p.emc_quse_brlshft1);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_2, p.emc_quse_brlshft2);
    wr(EMC_BASE + EMC_QUSE_BRLSHFT_3, p.emc_quse_brlshft3);

    // Program pad macros controls and termination.
    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU1, p.emc_pmacro_brick_ctrl_rfu1);
    wr(EMC_BASE + EMC_PMACRO_PAD_CFG_CTRL, p.emc_pmacro_pad_cfg_ctrl);

    wr(EMC_BASE + EMC_PMACRO_CMD_BRICK_CTRL_FDPD, p.emc_pmacro_cmd_brick_ctrl_fdpd);
    wr(EMC_BASE + EMC_PMACRO_BRICK_CTRL_RFU2, p.emc_pmacro_brick_ctrl_rfu2);
    wr(EMC_BASE + EMC_PMACRO_DATA_BRICK_CTRL_FDPD, p.emc_pmacro_data_brick_ctrl_fdpd);
    wr(EMC_BASE + EMC_PMACRO_DATA_PAD_RX_CTRL, p.emc_pmacro_data_pad_rx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_CMD_PAD_RX_CTRL, p.emc_pmacro_cmd_pad_rx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_DATA_PAD_TX_CTRL, p.emc_pmacro_data_pad_tx_ctrl);
    wr(EMC_BASE + EMC_PMACRO_DATA_RX_TERM_MODE, p.emc_pmacro_data_rx_term_mode);
    wr(EMC_BASE + EMC_PMACRO_CMD_RX_TERM_MODE, p.emc_pmacro_cmd_rx_term_mode);
    wr(EMC_BASE + EMC_PMACRO_CMD_PAD_TX_CTRL, p.emc_pmacro_cmd_pad_tx_ctrl & 0xEFFFFFFF);

    // Program pad macro pins/bytes.
    wr(EMC_BASE + EMC_CFG_3, p.emc_cfg3);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_0, p.emc_pmacro_tx_pwrd0);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_1, p.emc_pmacro_tx_pwrd1);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_2, p.emc_pmacro_tx_pwrd2);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_3, p.emc_pmacro_tx_pwrd3);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_4, p.emc_pmacro_tx_pwrd4);
    wr(EMC_BASE + EMC_PMACRO_TX_PWRD_5, p.emc_pmacro_tx_pwrd5);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_0, p.emc_pmacro_tx_sel_clk_src0);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_1, p.emc_pmacro_tx_sel_clk_src1);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_2, p.emc_pmacro_tx_sel_clk_src2);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_3, p.emc_pmacro_tx_sel_clk_src3);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_4, p.emc_pmacro_tx_sel_clk_src4);
    wr(EMC_BASE + EMC_PMACRO_TX_SEL_CLK_SRC_5, p.emc_pmacro_tx_sel_clk_src5);

    // Program per bit pad macros.
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_0_B01, p.emc_pmacro_perbit_fgcg_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_1_B01, p.emc_pmacro_perbit_fgcg_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_2_B01, p.emc_pmacro_perbit_fgcg_ctrl2);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_3_B01, p.emc_pmacro_perbit_fgcg_ctrl3);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_4_B01, p.emc_pmacro_perbit_fgcg_ctrl4);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_FGCG_CTRL_5_B01, p.emc_pmacro_perbit_fgcg_ctrl5);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_0_B01, p.emc_pmacro_perbit_rfu_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_1_B01, p.emc_pmacro_perbit_rfu_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_2_B01, p.emc_pmacro_perbit_rfu_ctrl2);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_3_B01, p.emc_pmacro_perbit_rfu_ctrl3);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_4_B01, p.emc_pmacro_perbit_rfu_ctrl4);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU_CTRL_5_B01, p.emc_pmacro_perbit_rfu_ctrl5);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_0_B01, p.emc_pmacro_perbit_rfu1_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_1_B01, p.emc_pmacro_perbit_rfu1_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_2_B01, p.emc_pmacro_perbit_rfu1_ctrl2);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_3_B01, p.emc_pmacro_perbit_rfu1_ctrl3);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_4_B01, p.emc_pmacro_perbit_rfu1_ctrl4);
    wr(EMC_BASE + EMC_PMACRO_PERBIT_RFU1_CTRL_5_B01, p.emc_pmacro_perbit_rfu1_ctrl5);
    wr(EMC_BASE + EMC_PMACRO_DATA_PI_CTRL_B01, p.emc_pmacro_data_pi_ctrl);
    wr(EMC_BASE + EMC_PMACRO_CMD_PI_CTRL_B01, p.emc_pmacro_cmd_pi_ctrl);

    wr(EMC_BASE + EMC_PMACRO_DDLL_BYPASS, p.emc_pmacro_ddll_bypass);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_0, p.emc_pmacro_ddll_pwrd0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_1, p.emc_pmacro_ddll_pwrd1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_PWRD_2, p.emc_pmacro_ddll_pwrd2);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_0, p.emc_pmacro_cmd_ctrl0);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_1, p.emc_pmacro_cmd_ctrl1);
    wr(EMC_BASE + EMC_PMACRO_CMD_CTRL_2, p.emc_pmacro_cmd_ctrl2);

    // Program inbound vref setting.
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQ_0, p.emc_pmacro_ib_vref_dq_0);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQ_1, p.emc_pmacro_ib_vref_dq_1);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQS_0, p.emc_pmacro_ib_vref_dqs_0);
    wr(EMC_BASE + EMC_PMACRO_IB_VREF_DQS_1, p.emc_pmacro_ib_vref_dqs_1);
    wr(EMC_BASE + EMC_PMACRO_IB_RXRT, p.emc_pmacro_ib_rxrt);

    // Program quse trimmers.
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_0, p.emc_pmacro_quse_ddll_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_1, p.emc_pmacro_quse_ddll_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_2, p.emc_pmacro_quse_ddll_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_3, p.emc_pmacro_quse_ddll_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_4, p.emc_pmacro_quse_ddll_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK0_5, p.emc_pmacro_quse_ddll_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_0, p.emc_pmacro_quse_ddll_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_1, p.emc_pmacro_quse_ddll_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_2, p.emc_pmacro_quse_ddll_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_3, p.emc_pmacro_quse_ddll_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_4, p.emc_pmacro_quse_ddll_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_QUSE_DDLL_RANK1_5, p.emc_pmacro_quse_ddll_rank1_5);

    // Program outbound trimmers.
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_0, p.emc_pmacro_ob_ddll_long_dq_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_1, p.emc_pmacro_ob_ddll_long_dq_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_2, p.emc_pmacro_ob_ddll_long_dq_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_3, p.emc_pmacro_ob_ddll_long_dq_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_4, p.emc_pmacro_ob_ddll_long_dq_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK0_5, p.emc_pmacro_ob_ddll_long_dq_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_0, p.emc_pmacro_ob_ddll_long_dq_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_1, p.emc_pmacro_ob_ddll_long_dq_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_2, p.emc_pmacro_ob_ddll_long_dq_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_3, p.emc_pmacro_ob_ddll_long_dq_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_4, p.emc_pmacro_ob_ddll_long_dq_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQ_RANK1_5, p.emc_pmacro_ob_ddll_long_dq_rank1_5);

    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_0, p.emc_pmacro_ob_ddll_long_dqs_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_1, p.emc_pmacro_ob_ddll_long_dqs_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_2, p.emc_pmacro_ob_ddll_long_dqs_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_3, p.emc_pmacro_ob_ddll_long_dqs_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_4, p.emc_pmacro_ob_ddll_long_dqs_rank0_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK0_5, p.emc_pmacro_ob_ddll_long_dqs_rank0_5);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_0, p.emc_pmacro_ob_ddll_long_dqs_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_1, p.emc_pmacro_ob_ddll_long_dqs_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_2, p.emc_pmacro_ob_ddll_long_dqs_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_3, p.emc_pmacro_ob_ddll_long_dqs_rank1_3);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_4, p.emc_pmacro_ob_ddll_long_dqs_rank1_4);
    wr(EMC_BASE + EMC_PMACRO_OB_DDLL_LONG_DQS_RANK1_5, p.emc_pmacro_ob_ddll_long_dqs_rank1_5);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_0, p.emc_pmacro_ib_ddll_long_dqs_rank0_0);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_1, p.emc_pmacro_ib_ddll_long_dqs_rank0_1);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_2, p.emc_pmacro_ib_ddll_long_dqs_rank0_2);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK0_3, p.emc_pmacro_ib_ddll_long_dqs_rank0_3);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_0, p.emc_pmacro_ib_ddll_long_dqs_rank1_0);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_1, p.emc_pmacro_ib_ddll_long_dqs_rank1_1);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_2, p.emc_pmacro_ib_ddll_long_dqs_rank1_2);
    wr(EMC_BASE + EMC_PMACRO_IB_DDLL_LONG_DQS_RANK1_3, p.emc_pmacro_ib_ddll_long_dqs_rank1_3);

    // Program clock trimmers.
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_0, p.emc_pmacro_ddll_long_cmd_0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_1, p.emc_pmacro_ddll_long_cmd_1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_2, p.emc_pmacro_ddll_long_cmd_2);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_3, p.emc_pmacro_ddll_long_cmd_3);
    wr(EMC_BASE + EMC_PMACRO_DDLL_LONG_CMD_4, p.emc_pmacro_ddll_long_cmd_4);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_0, p.emc_pmacro_ddll_short_cmd_0);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_1, p.emc_pmacro_ddll_short_cmd_1);
    wr(EMC_BASE + EMC_PMACRO_DDLL_SHORT_CMD_2, p.emc_pmacro_ddll_short_cmd_2);

    // Set DLL periodic offset.
    wr(EMC_BASE + EMC_PMACRO_DDLL_PERIODIC_OFFSET, p.emc_pmacro_ddll_periodic_offset);

    // Patch 4 using BCT spare variables.
    if (p.emc_bct_spare4) != 0 {
        wr(p.emc_bct_spare4, p.emc_bct_spare5);
    }

    // Patch 4 to 6 using BCT spare secure variables.
    if (p.emc_bct_spare_secure6) != 0 {
        wr(p.emc_bct_spare_secure6, p.emc_bct_spare_secure7);
    }
    if (p.emc_bct_spare_secure8) != 0 {
        wr(p.emc_bct_spare_secure8, p.emc_bct_spare_secure9);
    }
    if (p.emc_bct_spare_secure10) != 0 {
        wr(p.emc_bct_spare_secure10, p.emc_bct_spare_secure11);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    // Initialize MC VPR settings.
    wr(MC_BASE + MC_VIDEO_PROTECT_BOM, p.mc_video_protect_bom);
    wr(MC_BASE + MC_VIDEO_PROTECT_BOM_ADR_HI, p.mc_video_protect_bom_adr_hi);
    wr(MC_BASE + MC_VIDEO_PROTECT_SIZE_MB, p.mc_video_protect_size_mb);
    wr(MC_BASE + MC_VIDEO_PROTECT_VPR_OVERRIDE, p.mc_video_protect_vpr_override);
    wr(MC_BASE + MC_VIDEO_PROTECT_VPR_OVERRIDE1, p.mc_video_protect_vpr_override1);
    wr(MC_BASE + MC_VIDEO_PROTECT_GPU_OVERRIDE_0, p.mc_video_protect_gpu_override0);
    wr(MC_BASE + MC_VIDEO_PROTECT_GPU_OVERRIDE_1, p.mc_video_protect_gpu_override1);

    // Program SDRAM geometry parameters.
    wr(MC_BASE + MC_EMEM_ADR_CFG, p.mc_emem_adr_cfg);
    wr(MC_BASE + MC_EMEM_ADR_CFG_DEV0, p.mc_emem_adr_cfg_dev0);
    wr(MC_BASE + MC_EMEM_ADR_CFG_DEV1, p.mc_emem_adr_cfg_dev1);
    wr(MC_BASE + MC_EMEM_ADR_CFG_CHANNEL_MASK, p.mc_emem_adr_cfg_channel_mask);

    // Program bank swizzling.
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_0, p.mc_emem_adr_cfg_bank_mask0);
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_1, p.mc_emem_adr_cfg_bank_mask1);
    wr(MC_BASE + MC_EMEM_ADR_CFG_BANK_MASK_2, p.mc_emem_adr_cfg_bank_mask2);

    // Program external memory aperture (base and size).
    wr(MC_BASE + MC_EMEM_CFG, p.mc_emem_cfg);

    // Program SEC carveout (base and size).
    wr(MC_BASE + MC_SEC_CARVEOUT_BOM, p.mc_sec_carveout_bom);
    wr(MC_BASE + MC_SEC_CARVEOUT_ADR_HI, p.mc_sec_carveout_adr_hi);
    wr(MC_BASE + MC_SEC_CARVEOUT_SIZE_MB, p.mc_sec_carveout_size_mb);

    // Program MTS carveout (base and size).
    wr(MC_BASE + MC_MTS_CARVEOUT_BOM, p.mc_mts_carveout_bom);
    wr(MC_BASE + MC_MTS_CARVEOUT_ADR_HI, p.mc_mts_carveout_adr_hi);
    wr(MC_BASE + MC_MTS_CARVEOUT_SIZE_MB, p.mc_mts_carveout_size_mb);

    // Program the memory arbiter.
    wr(MC_BASE + MC_EMEM_ARB_CFG, p.mc_emem_arb_cfg);
    wr(MC_BASE + MC_EMEM_ARB_OUTSTANDING_REQ, p.mc_emem_arb_outstanding_req);
    wr(MC_BASE + MC_EMEM_ARB_REFPB_HP_CTRL, p.emc_emem_arb_refpb_hp_ctrl);
    wr(MC_BASE + MC_EMEM_ARB_REFPB_BANK_CTRL, p.emc_emem_arb_refpb_bank_ctrl);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RCD, p.mc_emem_arb_timing_rcd);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RP, p.mc_emem_arb_timing_rp);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RC, p.mc_emem_arb_timing_rc);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RAS, p.mc_emem_arb_timing_ras);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_FAW, p.mc_emem_arb_timing_faw);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RRD, p.mc_emem_arb_timing_rrd);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RAP2PRE, p.mc_emem_arb_timing_rap2pre);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_WAP2PRE, p.mc_emem_arb_timing_wap2pre);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_R2R, p.mc_emem_arb_timing_r2r);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_W2W, p.mc_emem_arb_timing_w2w);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_CCDMW, p.mc_emem_arb_timing_ccdmw);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_R2W, p.mc_emem_arb_timing_r2w);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_W2R, p.mc_emem_arb_timing_w2r);
    wr(MC_BASE + MC_EMEM_ARB_TIMING_RFCPB, p.mc_emem_arb_timing_rfcpb);
    wr(MC_BASE + MC_EMEM_ARB_DA_TURNS, p.mc_emem_arb_da_turns);
    wr(MC_BASE + MC_EMEM_ARB_DA_COVERS, p.mc_emem_arb_da_covers);
    wr(MC_BASE + MC_EMEM_ARB_MISC0, p.mc_emem_arb_misc0);
    wr(MC_BASE + MC_EMEM_ARB_MISC1, p.mc_emem_arb_misc1);
    wr(MC_BASE + MC_EMEM_ARB_MISC2, p.mc_emem_arb_misc2);
    wr(MC_BASE + MC_EMEM_ARB_RING1_THROTTLE, p.mc_emem_arb_ring1_throttle);
    wr(MC_BASE + MC_EMEM_ARB_OVERRIDE, p.mc_emem_arb_override);
    wr(MC_BASE + MC_EMEM_ARB_OVERRIDE_1, p.mc_emem_arb_override1);
    wr(MC_BASE + MC_EMEM_ARB_RSV, p.mc_emem_arb_rsv);
    wr(MC_BASE + MC_DA_CONFIG0, p.mc_da_cfg0);

    // Trigger MC timing update.
    wr(MC_BASE + MC_TIMING_CONTROL, 1);

    // Program second-level clock enable overrides.
    wr(MC_BASE + MC_CLKEN_OVERRIDE, p.mc_clken_override);

    // Program statistics gathering.
    wr(MC_BASE + MC_STAT_CONTROL, p.mc_stat_control);

    // Program SDRAM geometry parameters.
    wr(EMC_BASE + EMC_ADR_CFG, p.emc_adr_cfg);

    // Program second-level clock enable overrides.
    wr(EMC_BASE + EMC_CLKEN_OVERRIDE, p.emc_clken_override);

    // Program EMC pad auto calibration.
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_0, p.emc_pmacro_auto_cal_cfg0);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_1, p.emc_pmacro_auto_cal_cfg1);
    wr(EMC_BASE + EMC_PMACRO_AUTOCAL_CFG_2, p.emc_pmacro_auto_cal_cfg2);

    wr(EMC_BASE + EMC_AUTO_CAL_VREF_SEL_0, p.emc_auto_cal_vref_sel0);
    wr(EMC_BASE + EMC_AUTO_CAL_VREF_SEL_1, p.emc_auto_cal_vref_sel1);

    // Program/Start auto calibration.
    wr(EMC_BASE + EMC_AUTO_CAL_INTERVAL, p.emc_auto_cal_interval);
    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG, p.emc_auto_cal_config);
    usleep(p.emc_auto_cal_wait);

    // Patch 5 using BCT spare variables.
    if (p.emc_bct_spare8) != 0 {
        wr(p.emc_bct_spare8, p.emc_bct_spare9);
    }

    wr(EMC_BASE + EMC_AUTO_CAL_CONFIG9_B01, p.emc_auto_cal_config9);

    // Program EMC timing configuration.
    wr(EMC_BASE + EMC_CFG_2, p.emc_cfg2);
    wr(EMC_BASE + EMC_CFG_PIPE, p.emc_cfg_pipe);
    wr(EMC_BASE + EMC_CFG_PIPE_1, p.emc_cfg_pipe1);
    wr(EMC_BASE + EMC_CFG_PIPE_2, p.emc_cfg_pipe2);
    wr(EMC_BASE + EMC_CMDQ, p.emc_cmd_q);
    wr(EMC_BASE + EMC_MC2EMCQ, p.emc_mc2emc_q);
    wr(EMC_BASE + EMC_MRS_WAIT_CNT, p.emc_mrs_wait_cnt);
    wr(EMC_BASE + EMC_MRS_WAIT_CNT2, p.emc_mrs_wait_cnt2);
    wr(EMC_BASE + EMC_FBIO_CFG5, p.emc_fbio_cfg5);
    wr(EMC_BASE + EMC_RC, p.emc_rc);
    wr(EMC_BASE + EMC_RFC, p.emc_rfc);
    wr(EMC_BASE + EMC_RFCPB, p.emc_rfc_pb);
    wr(EMC_BASE + EMC_REFCTRL2, p.emc_ref_ctrl2);
    wr(EMC_BASE + EMC_RFC_SLR, p.emc_rfc_slr);
    wr(EMC_BASE + EMC_RAS, p.emc_ras);
    wr(EMC_BASE + EMC_RP, p.emc_rp);
    wr(EMC_BASE + EMC_TPPD, p.emc_tppd);
    wr(EMC_BASE + EMC_TRTM_B01, p.emc_trtm);
    wr(EMC_BASE + EMC_TWTM_B01, p.emc_twtm);
    wr(EMC_BASE + EMC_TRATM_B01, p.emc_tratm);
    wr(EMC_BASE + EMC_TWATM_B01, p.emc_twatm);
    wr(EMC_BASE + EMC_TR2REF_B01, p.emc_tr2ref);
    wr(EMC_BASE + EMC_R2R, p.emc_r2r);
    wr(EMC_BASE + EMC_W2W, p.emc_w2w);
    wr(EMC_BASE + EMC_R2W, p.emc_r2w);
    wr(EMC_BASE + EMC_W2R, p.emc_w2r);
    wr(EMC_BASE + EMC_R2P, p.emc_r2p);
    wr(EMC_BASE + EMC_W2P, p.emc_w2p);
    wr(EMC_BASE + EMC_CCDMW, p.emc_ccdmw);
    wr(EMC_BASE + EMC_RD_RCD, p.emc_rd_rcd);
    wr(EMC_BASE + EMC_WR_RCD, p.emc_wr_rcd);
    wr(EMC_BASE + EMC_RRD, p.emc_rrd);
    wr(EMC_BASE + EMC_REXT, p.emc_rext);
    wr(EMC_BASE + EMC_WEXT, p.emc_wext);
    wr(EMC_BASE + EMC_WDV, p.emc_wdv);
    wr(EMC_BASE + EMC_WDV_CHK, p.emc_wdv_chk);
    wr(EMC_BASE + EMC_WSV, p.emc_wsv);
    wr(EMC_BASE + EMC_WEV, p.emc_wev);
    wr(EMC_BASE + EMC_WDV_MASK, p.emc_wdv_mask);
    wr(EMC_BASE + EMC_WS_DURATION, p.emc_ws_duration);
    wr(EMC_BASE + EMC_WE_DURATION, p.emc_we_duration);
    wr(EMC_BASE + EMC_QUSE, p.emc_quse);
    wr(EMC_BASE + EMC_QUSE_WIDTH, p.emc_quse_width);
    wr(EMC_BASE + EMC_IBDLY, p.emc_ibdly);
    wr(EMC_BASE + EMC_OBDLY, p.emc_obdly);
    wr(EMC_BASE + EMC_EINPUT, p.emc_einput);
    wr(EMC_BASE + EMC_EINPUT_DURATION, p.emc_einput_duration);
    wr(EMC_BASE + EMC_PUTERM_EXTRA, p.emc_puterm_extra);
    wr(EMC_BASE + EMC_PUTERM_WIDTH, p.emc_puterm_width);

    wr(EMC_BASE + EMC_DBG, p.emc_dbg);

    // Clear read fifo.
    wr(EMC_BASE + EMC_QRST, p.emc_qrst);
    wr(EMC_BASE + EMC_ISSUE_QRST, 1);
    wr(EMC_BASE + EMC_ISSUE_QRST, 0);

    // Program the rest of EMC timing configuration.
    wr(EMC_BASE + EMC_QSAFE, p.emc_qsafe);
    wr(EMC_BASE + EMC_RDV, p.emc_rdv);
    wr(EMC_BASE + EMC_RDV_MASK, p.emc_rdv_mask);
    wr(EMC_BASE + EMC_RDV_EARLY, p.emc_rdv_early);
    wr(EMC_BASE + EMC_RDV_EARLY_MASK, p.emc_rdv_early_mask);
    wr(EMC_BASE + EMC_QPOP, p.emc_qpop);
    wr(EMC_BASE + EMC_REFRESH, p.emc_refresh);
    wr(EMC_BASE + EMC_BURST_REFRESH_NUM, p.emc_burst_refresh_num);
    wr(EMC_BASE + EMC_PRE_REFRESH_REQ_CNT, p.emc_prerefresh_req_cnt);
    wr(EMC_BASE + EMC_PDEX2WR, p.emc_pdex2wr);
    wr(EMC_BASE + EMC_PDEX2RD, p.emc_pdex2rd);
    wr(EMC_BASE + EMC_PCHG2PDEN, p.emc_pchg2pden);
    wr(EMC_BASE + EMC_ACT2PDEN, p.emc_act2pden);
    wr(EMC_BASE + EMC_AR2PDEN, p.emc_ar2pden);
    wr(EMC_BASE + EMC_RW2PDEN, p.emc_rw2pden);
    wr(EMC_BASE + EMC_CKE2PDEN, p.emc_cke2pden);
    wr(EMC_BASE + EMC_PDEX2CKE, p.emc_pdex2che);
    wr(EMC_BASE + EMC_PDEX2MRR, p.emc_pdex2mrr);
    wr(EMC_BASE + EMC_TXSR, p.emc_txsr);
    wr(EMC_BASE + EMC_TXSRDLL, p.emc_txsr_dll);
    wr(EMC_BASE + EMC_TCKE, p.emc_tcke);
    wr(EMC_BASE + EMC_TCKESR, p.emc_tckesr);
    wr(EMC_BASE + EMC_TPD, p.emc_tpd);
    wr(EMC_BASE + EMC_TFAW, p.emc_tfaw);
    wr(EMC_BASE + EMC_TRPAB, p.emc_trpab);
    wr(EMC_BASE + EMC_TCLKSTABLE, p.emc_tclkstable);
    wr(EMC_BASE + EMC_TCLKSTOP, p.emc_tclkstop);
    wr(EMC_BASE + EMC_TREFBW, p.emc_trefbw);
    wr(EMC_BASE + EMC_ODT_WRITE, p.emc_odt_write);
    wr(EMC_BASE + EMC_CFG_DIG_DLL, p.emc_cfg_dig_dll);
    wr(EMC_BASE + EMC_CFG_DIG_DLL_PERIOD, p.emc_cfg_dig_dll_period);

    // Don't write CFG_ADR_EN (bit 1) here - lock bit written later.
    wr(EMC_BASE + EMC_FBIO_SPARE, p.emc_fbio_spare & 0xFFFFFFFD);
    wr(EMC_BASE + EMC_CFG_RSV, p.emc_cfg_rsv);
    wr(EMC_BASE + EMC_PMC_SCRATCH1, p.emc_pmc_scratch1);
    wr(EMC_BASE + EMC_PMC_SCRATCH2, p.emc_pmc_scratch2);
    wr(EMC_BASE + EMC_PMC_SCRATCH3, p.emc_pmc_scratch3);
    wr(EMC_BASE + EMC_ACPD_CONTROL, p.emc_acpd_control);
    wr(EMC_BASE + EMC_TXDSRVTTGEN, p.emc_txdsrvttgen);
    wr(EMC_BASE + EMC_PMACRO_DSR_VTTGEN_CTRL_0_B01, p.emc_pmacro_dsr_vttgen_ctrl0);

    // Set pipe bypass enable bits before sending any DRAM commands.
    wr(EMC_BASE + EMC_CFG, (p.emc_cfg & 0xE) | 0x3C00000);

    // BootROM patching is used as a generic patch here.
    if (p.boot_rom_patch_control) != 0 {
        wr(p.boot_rom_patch_control, p.boot_rom_patch_data);

        // Trigger MC timing update.
        wr(MC_BASE + MC_TIMING_CONTROL, 1);
    }

    // Patch 7 to 9 using BCT spare secure variables.
    if (p.emc_bct_spare_secure12) != 0 {
        wr(p.emc_bct_spare_secure12, p.emc_bct_spare_secure13);
    }
    if (p.emc_bct_spare_secure14) != 0 {
        wr(p.emc_bct_spare_secure14, p.emc_bct_spare_secure15);
    }
    if (p.emc_bct_spare_secure16) != 0 {
        wr(p.emc_bct_spare_secure16, p.emc_bct_spare_secure17);
    }

    // Release SEL_DPD_CMD.
    wr(PMC_BASE + APBDEV_PMC_IO_DPD3_REQ, (p.emc_pmc_scratch1 & 0xFFF0000) | PMC_IO_DPD_REQ_DPD_OFF);
    usleep(p.pmc_io_dpd3_req_wait);

    // Set transmission pad control parameters.
    wr(EMC_BASE + EMC_PMACRO_CMD_PAD_TX_CTRL, p.emc_pmacro_cmd_pad_tx_ctrl);

    // ZQ CAL setup (not actually issuing ZQ CAL now).
    if (p.emc_zcal_warm_cold_boot_enables & 1) != 0 {
        wr(EMC_BASE + EMC_ZCAL_WAIT_CNT, p.emc_zcal_wait_cnt);
        wr(EMC_BASE + EMC_ZCAL_MRW_CMD, p.emc_zcal_mrw_cmd);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);
    usleep(p.emc_timing_control_wait);

    // Deassert HOLD_CKE_LOW.
    wr(PMC_BASE + APBDEV_PMC_DDR_CNTRL, rd(PMC_BASE + APBDEV_PMC_DDR_CNTRL) & (0xFF78007F));
    usleep(p.pmc_ddr_ctrl_wait);

    // Set clock enable signal.
    let mut pin_gpio_cfg: u32 = (p.emc_pin_gpio_enable << 16) | (p.emc_pin_gpio << 12);
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg);
    let _ = rd(EMC_BASE + EMC_PIN);
    usleep(p.emc_pin_extra_wait + 200);
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg | 0x100);
    let _ = rd(EMC_BASE + EMC_PIN);

    usleep(p.emc_pin_extra_wait + 2000);

    // Enable clock enable signal.
    wr(EMC_BASE + EMC_PIN, pin_gpio_cfg | 0x101);
    let _ = rd(EMC_BASE + EMC_PIN);
    usleep(p.emc_pin_program_wait);

    // Init zq calibration,
    // Patch 6 using BCT spare variables.
    if (p.emc_bct_spare10) != 0 {
        wr(p.emc_bct_spare10, p.emc_bct_spare11);
    }

    // Write mode registers.
    wr(EMC_BASE + EMC_MRW2, p.emc_mrw2);
    wr(EMC_BASE + EMC_MRW, p.emc_mrw1);
    wr(EMC_BASE + EMC_MRW3, p.emc_mrw3);
    wr(EMC_BASE + EMC_MRW4, p.emc_mrw4);
    wr(EMC_BASE + EMC_MRW6, p.emc_mrw6);
    wr(EMC_BASE + EMC_MRW14, p.emc_mrw14);

    wr(EMC_BASE + EMC_MRW8, p.emc_mrw8);
    wr(EMC_BASE + EMC_MRW12, p.emc_mrw12);
    wr(EMC_BASE + EMC_MRW9, p.emc_mrw9);
    wr(EMC_BASE + EMC_MRW13, p.emc_mrw13);

    if (p.emc_zcal_warm_cold_boot_enables & 1) != 0 {
        // Issue ZQCAL start, device 0.
        wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev0);
        usleep(p.emc_zcal_init_wait);

        // Issue ZQCAL latch.
        wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev0 ^ 3);
        // Same for device 1.
        if (p.emc_dev_select & 2) == 0 {
            wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev1);
            usleep(p.emc_zcal_init_wait);
            wr(EMC_BASE + EMC_ZQ_CAL, p.emc_zcal_init_dev1 ^ 3);
        }
    }

    // Patch 10 to 12 using BCT spare secure variables.
    if (p.emc_bct_spare_secure18) != 0 {
        wr(p.emc_bct_spare_secure18, p.emc_bct_spare_secure19);
    }
    if (p.emc_bct_spare_secure20) != 0 {
        wr(p.emc_bct_spare_secure20, p.emc_bct_spare_secure21);
    }
    if (p.emc_bct_spare_secure22) != 0 {
        wr(p.emc_bct_spare_secure22, p.emc_bct_spare_secure23);
    }

    // Set package and DPD pad control.
    wr(PMC_BASE + APBDEV_PMC_DDR_CFG, p.pmc_ddr_cfg);

    // Start periodic ZQ calibration (LPDDRx only).
    wr(EMC_BASE + EMC_ZCAL_INTERVAL, p.emc_zcal_interval);
    wr(EMC_BASE + EMC_ZCAL_WAIT_CNT, p.emc_zcal_wait_cnt);
    wr(EMC_BASE + EMC_ZCAL_MRW_CMD, p.emc_zcal_mrw_cmd);

    // Patch 7 using BCT spare variables.
    if (p.emc_bct_spare12) != 0 {
        wr(p.emc_bct_spare12, p.emc_bct_spare13);
    }

    // Trigger timing update so above writes take place.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    if (p.emc_extra_refresh_num) != 0 {
        wr(EMC_BASE + EMC_REF, ((1 << p.emc_extra_refresh_num << 8) - 253) | (p.emc_dev_select << 30));
    }

    // Enable refresh.
    wr(EMC_BASE + EMC_REFCTRL, p.emc_dev_select | (1 << 31));

    wr(EMC_BASE + EMC_DYN_SELF_REF_CONTROL, p.emc_dyn_self_ref_control);
    wr(EMC_BASE + EMC_CFG, p.emc_cfg);
    wr(EMC_BASE + EMC_FDPD_CTRL_DQ, p.emc_fdpd_ctrl_dq);
    wr(EMC_BASE + EMC_FDPD_CTRL_CMD, p.emc_fdpd_ctrl_cmd);
    wr(EMC_BASE + EMC_SEL_DPD_CTRL, p.emc_sel_dpd_ctrl);

    // Write addr swizzle lock bit.
    wr(EMC_BASE + EMC_FBIO_SPARE, p.emc_fbio_spare | (1 << 1));

    // Re-trigger timing to latch power saving functions.
    wr(EMC_BASE + EMC_TIMING_CONTROL, 1);

    wr(EMC_BASE + EMC_CFG_UPDATE, p.emc_cfg_update);

    // Enable EMC pipe clock gating.
    wr(EMC_BASE + EMC_CFG_PIPE_CLK, p.emc_cfg_pipe_clk);

    // Depending on freqency, enable CMD/CLK fdpd.
    wr(EMC_BASE + EMC_FDPD_CTRL_CMD_NO_RAMP, p.emc_fdpd_ctrl_cmd_no_ramp);

    // Set untranslated region requirements.
    wr(MC_BASE + MC_UNTRANSLATED_REGION_CHECK_B01, p.mc_untranslated_region_check);

    // Lock carveouts per BCT cfg.
    wr(MC_BASE + MC_VIDEO_PROTECT_REG_CTRL, p.mc_video_protect_write_access);
    wr(MC_BASE + MC_SEC_CARVEOUT_REG_CTRL, p.mc_sec_carveout_protect_write_access);
    wr(MC_BASE + MC_MTS_CARVEOUT_REG_CTRL, p.mc_mts_carveout_reg_ctrl);

    // Disable write access to a bunch of EMC registers.
    wr(MC_BASE + MC_EMEM_CFG_ACCESS_CTRL, 1);

    // Enable arbiter.
    wr(SYSREG_BASE + AHB_ARBITRATION_XBAR_CTRL, (rd(SYSREG_BASE + AHB_ARBITRATION_XBAR_CTRL) & 0xFFFEFFFF) | (p.ahb_arbitration_xbar_ctrl_meminit_done << 16));
}


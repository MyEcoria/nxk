//! Hardware bring-up from RCM (hekate bdk/soc/hw_init.c `hw_init`), in
//! the same order and with the same values: oscillators and BPMP clock,
//! pins, I2C, the PMIC's rails for the SoC (Erista or Mariko), then DRAM.
//! Safe to run again without a reset, as hekate's.

use crate::clock::{self, car, set_car, ClkRst};
use crate::hdr::*;
use crate::i2c::{self, I2C_1, I2C_5};
use crate::pmic;
use crate::reg::{rd, rmw, wr};
use crate::soc::{self, *};
use crate::timer::{now_ms, usleep};

/// The reset cause the boot ROM left (`hw_rst_status`, `hw_rst_reason`).
pub static mut RST_STATUS: u32 = 0;
pub static mut RST_REASON: u32 = 0;
/// When the SD card's power was cut (`sd_power_cycle_time_start`).
pub static mut SD_POWER_CUT_MS: u32 = 0;

fn pmc(off: u32) -> u32 {
    rd(PMC_BASE + off)
}
fn set_pmc(off: u32, v: u32) {
    wr(PMC_BASE + off, v)
}
fn apb_misc(off: u32) -> u32 {
    rd(APB_MISC_BASE + off)
}
fn set_apb_misc(off: u32, v: u32) {
    wr(APB_MISC_BASE + off, v)
}

const SE_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_V_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_V_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_SE,
    index: CLK_V_SE,
    clk_src: 0,
    clk_div: clock::src_div(1),
};
const TZRAM_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_V_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_V_SET,
    source: 0,
    index: CLK_V_TZRAM,
    clk_src: 0,
    clk_div: 0,
};
pub const HOST1X_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_HOST1X,
    index: CLK_L_HOST1X,
    clk_src: 4,
    // CLK_SRC_DIV(2.5): 163.2 MHz.
    clk_div: 3,
};
const CL_DVFS_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_W_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_W_SET,
    source: 0,
    index: CLK_W_DVFS,
    clk_src: 0,
    clk_div: 0,
};
/// I2C1 at 100 kHz, I2C5 (power) at 400 kHz.
const I2C1_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_I2C1,
    index: CLK_L_I2C1,
    clk_src: 6,
    clk_div: 3,
};
const I2C5_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_H_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_H_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_I2C5,
    index: CLK_H_I2C5,
    clk_src: 6,
    clk_div: 0,
};

/// `bpmp_clk_rate_relaxed(true)`: SCLK back on PLLP_OUT while a bus device
/// is set up (the restore is a no-op until the BPMP is overclocked).
fn bpmp_clk_relaxed() {
    set_car(CLK_RST_CONTROLLER_SCLK_BURST_POLICY, 0x2000_3330);
    let _ = car(CLK_RST_CONTROLLER_SCLK_BURST_POLICY);
    usleep(100);
    set_car(CLK_RST_CONTROLLER_CLK_SYSTEM_RATE, 2);
    let _ = car(CLK_RST_CONTROLLER_CLK_SYSTEM_RATE);
}

/// `clock_enable_i2c`.
fn enable_i2c_clock(c: &ClkRst) {
    bpmp_clk_relaxed();
    clock::enable(c);
}

/// `_config_oscillators`: CLK_M 19.2 MHz from the 38.4 MHz crystal, the
/// microsecond timer, BPMP/SCLK on PLLP_OUT2 (204 MHz).
fn config_oscillators() {
    set_car(CLK_RST_CONTROLLER_SPARE_REG0, car(CLK_RST_CONTROLLER_SPARE_REG0) & 0xFFFF_FFF3 | 4);
    wr(SYSCTR0_BASE + SYSCTR0_CNTFID0, 19_200_000);
    wr(TMR_BASE + TIMERUS_USEC_CFG, 0x45F);
    set_car(CLK_RST_CONTROLLER_OSC_CTRL, 0x5000_0071);

    rmw(PMC_BASE + APBDEV_PMC_OSC_EDPD_OVER, 0xFFFF_FF81, 0xE);
    rmw(PMC_BASE + APBDEV_PMC_OSC_EDPD_OVER, 0xFFBF_FFFF, PMC_OSC_EDPD_OVER_OSC_CTRL_OVER);
    rmw(PMC_BASE + APBDEV_PMC_CNTRL2, 0xFFFF_EFFF, PMC_CNTRL2_HOLD_CKE_LOW_EN);
    rmw(APB_MISC_BASE + APB_MISC_GP_ASDBGREG, 0xFCFF_FFFF, 2 << 24);

    set_car(CLK_RST_CONTROLLER_CLK_SYSTEM_RATE, 0x10);
    set_car(CLK_RST_CONTROLLER_PLLMB_BASE, car(CLK_RST_CONTROLLER_PLLMB_BASE) & 0xBFFF_FFFF);
    rmw(PMC_BASE + APBDEV_PMC_TSC_MULT, 0xFFFF_0000, 0x249F);

    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_SYS, 0);
    set_car(CLK_RST_CONTROLLER_SCLK_BURST_POLICY, 0x2000_4444);
    set_car(CLK_RST_CONTROLLER_SUPER_SCLK_DIVIDER, 0x8000_0000);
    set_car(CLK_RST_CONTROLLER_CLK_SYSTEM_RATE, 2);
}

/// `hw_config_arbiter`.
pub fn config_arbiter(reset: bool) {
    let v = if reset {
        [0x0040090, 0x12024C2, 0x2201209, 0x320365B]
    } else {
        [0x12412D1, 0x0000000, 0x220244A, 0x320369B]
    };
    wr(ARB_PRI_BASE + ARB_PRIO_CPU_PRIORITY, v[0]);
    wr(ARB_PRI_BASE + ARB_PRIO_COP_PRIORITY, v[1]);
    wr(ARB_PRI_BASE + ARB_PRIO_VCP_PRIORITY, v[2]);
    wr(ARB_PRI_BASE + ARB_PRIO_DMA_PRIORITY, v[3]);
}

/// `_config_gpios`: Joy-Con detection, the power I2C pads, UART-A, the
/// volume and HOME buttons as inputs.
fn config_gpios(hoag: bool) {
    set_apb_misc(APB_MISC_PP_PINMUX_GLOBAL, 0);
    if !hoag {
        pinmux(PINMUX_AUX_UART2_TX, 0);
        pinmux(PINMUX_AUX_UART3_TX, 0);
        gpio_direction_input(GPIO_PORT_G, GPIO_PIN_0);
        gpio_direction_input(GPIO_PORT_D, GPIO_PIN_1);
    }
    pinmux(PINMUX_AUX_GPIO_PE6, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    pinmux(PINMUX_AUX_GPIO_PH6, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    gpio_direction_input(GPIO_PORT_E, GPIO_PIN_6);
    gpio_direction_input(GPIO_PORT_H, GPIO_PIN_6);
    pinmux_config_i2c(I2C_1);
    pinmux_config_i2c(I2C_5);
    pinmux_config_uart(0);
    gpio_direction_input(GPIO_PORT_X, GPIO_PIN_6 | GPIO_PIN_7);
    pinmux(PINMUX_AUX_BUTTON_HOME, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    gpio_direction_input(GPIO_PORT_Y, GPIO_PIN_1);
}

/// `_config_pmc_scratch`.
fn config_pmc_scratch() {
    set_pmc(APBDEV_PMC_SCRATCH20, pmc(APBDEV_PMC_SCRATCH20) & 0xFFF3_FFFF);
    set_pmc(APBDEV_PMC_SCRATCH190, pmc(APBDEV_PMC_SCRATCH190) & 0xFFFF_FFFE);
    set_pmc(
        APBDEV_PMC_SECURE_SCRATCH21,
        pmc(APBDEV_PMC_SECURE_SCRATCH21) | PMC_FUSE_PRIVATEKEYDISABLE_TZ_STICKY_BIT,
    );
}

/// `_mbist_workaround_bl`: T210 MBIST errata (RAMs corrupted by MBIST_EN
/// after power-on): toggle the affected clocks, then gate what the boot
/// ROM left on.
fn mbist_workaround() {
    let sor1 = car(CLK_RST_CONTROLLER_CLK_SOURCE_SOR1) & !(1 << 14) | 1 << 15;
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_SOR1, sor1);
    set_car(
        CLK_RST_CONTROLLER_PLLD_BASE,
        car(CLK_RST_CONTROLLER_PLLD_BASE) | PLL_BASE_ENABLE | 1 << 23,
    );
    set_car(CLK_RST_CONTROLLER_CLK_ENB_V_SET, 1 << CLK_V_AHUB);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_Y_SET, 1 << CLK_Y_APE);
    set_car(CLK_RST_CONTROLLER_RST_DEV_Y_CLR, 1 << CLK_Y_APE);
    set_car(CLK_RST_CONTROLLER_RST_DEV_X_CLR, 1 << CLK_X_VIC);
    set_car(CLK_RST_CONTROLLER_RST_DEV_L_CLR, 1 << CLK_L_HOST1X | 1 << CLK_L_DISP1);
    usleep(2);

    for i in 0..5 {
        let a = I2S_BASE + I2S_CTRL + (i << 8);
        wr(a, rd(a) | I2S_CTRL_MASTER_EN);
        wr(I2S_BASE + I2S_CG + (i << 8), I2S_CG_SLCG_DISABLE);
    }
    let dsc = DISPLAY_A_BASE + (DC_COM_DSC_TOP_CTL << 2);
    wr(dsc, rd(dsc) | 1 << 2);
    wr(VIC_BASE + VIC_THI_SLCG_OVERRIDE_LOW_A, 0xFFFF_FFFF);
    usleep(2);

    set_car(CLK_RST_CONTROLLER_RST_DEV_Y_SET, 1 << CLK_Y_APE);
    set_car(CLK_RST_CONTROLLER_RST_DEV_L_SET, 1 << CLK_L_HOST1X | 1 << CLK_L_DISP1);
    set_car(CLK_RST_CONTROLLER_RST_DEV_X_SET, 1 << CLK_X_VIC);

    set_car(CLK_RST_CONTROLLER_CLK_OUT_ENB_H, 1 << CLK_H_PMC | 1 << CLK_H_FUSE);
    set_car(
        CLK_RST_CONTROLLER_CLK_OUT_ENB_L,
        1 << CLK_L_RTC | 1 << CLK_L_TMR | 1 << CLK_L_GPIO | 1 << CLK_L_BPMP_CACHE_CTRL,
    );
    set_car(
        CLK_RST_CONTROLLER_CLK_OUT_ENB_U,
        1 << CLK_U_CSITE
            | 1 << CLK_U_IRAMA
            | 1 << CLK_U_IRAMB
            | 1 << CLK_U_IRAMC
            | 1 << CLK_U_IRAMD
            | 1 << CLK_U_BPMP_CACHE_RAM,
    );
    set_car(
        CLK_RST_CONTROLLER_CLK_OUT_ENB_V,
        1 << CLK_V_MSELECT | 1 << CLK_V_APB2APE | 1 << CLK_V_SPDIF_DOUBLER | 1 << CLK_V_SE,
    );
    set_car(
        CLK_RST_CONTROLLER_CLK_OUT_ENB_W,
        1 << CLK_W_PCIERX0
            | 1 << CLK_W_PCIERX1
            | 1 << CLK_W_PCIERX2
            | 1 << CLK_W_PCIERX3
            | 1 << CLK_W_PCIERX4
            | 1 << CLK_W_PCIERX5
            | 1 << CLK_W_ENTROPY
            | 1 << CLK_W_MC1,
    );
    set_car(
        CLK_RST_CONTROLLER_CLK_OUT_ENB_X,
        1 << CLK_X_MC_CAPA
            | 1 << CLK_X_MC_CBPA
            | 1 << CLK_X_MC_CPU
            | 1 << CLK_X_MC_BBC
            | 1 << CLK_X_GPU
            | 1 << CLK_X_DBGAPB
            | 1 << CLK_X_PLLG_REF,
    );
    set_car(CLK_RST_CONTROLLER_CLK_OUT_ENB_Y, 1 << CLK_Y_MC_CDPA | 1 << CLK_Y_MC_CCPA);

    set_car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRA, 0);
    set_car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRB, 0);
    set_car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRC, 0);
    set_car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRD, 0);
    set_car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRE, 0);

    set_car(CLK_RST_CONTROLLER_PLLD_BASE, car(CLK_RST_CONTROLLER_PLLD_BASE) & 0x1F7F_FFFF);
    set_car(
        CLK_RST_CONTROLLER_CLK_SOURCE_SOR1,
        car(CLK_RST_CONTROLLER_CLK_SOURCE_SOR1) & !(1 << 15 | 1 << 14),
    );
    for src in [
        CLK_RST_CONTROLLER_CLK_SOURCE_VI,
        CLK_RST_CONTROLLER_CLK_SOURCE_HOST1X,
        CLK_RST_CONTROLLER_CLK_SOURCE_NVENC,
    ] {
        set_car(src, car(src) & 0x1FFF_FFFF | 4 << 29);
    }
}

/// `_config_se_brom`: what the boot ROM skips on the RCM path.
fn config_se_brom() {
    soc::fuse_visible(true);
    soc::fuse_set_sbk();
    // TZRAM cleared here, else it misbehaves later (hekate).
    for a in (TZRAM_BASE..TZRAM_BASE + TZRAM_SIZE).step_by(4) {
        wr(a, 0);
    }
    set_pmc(APBDEV_PMC_CRYPTO_OP, PMC_CRYPTO_OP_SE_ENABLE);
    wr(
        SE_BASE + SE_INT_STATUS_REG,
        SE_INT_OP_DONE | SE_INT_OUT_DONE | SE_INT_OUT_LL_BUF_WR | SE_INT_IN_DONE | SE_INT_IN_LL_BUF_RD,
    );
    unsafe {
        RST_STATUS = pmc(APBDEV_PMC_SCRATCH200);
        RST_REASON = pmc(APBDEV_PMC_RST_STATUS) & PMC_RST_STATUS_MASK;
    }
    set_pmc(APBDEV_PMC_SCRATCH200, 0);
    set_pmc(APBDEV_PMC_RST_STATUS, PMC_RST_STATUS_POR);
    let strap = apb_misc(APB_MISC_PP_STRAPPING_OPT_A) & 0xF0 | 7 << 10;
    set_apb_misc(APB_MISC_PP_STRAPPING_OPT_A, strap);
}

fn pmic_byte(reg: u32, v: u32) {
    let _ = i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, reg, v as u8);
}

/// `_config_regulators`: SoC rails for an Erista (MAX77620 FPS, vdd_core
/// 1.125 V, CPU/GPU regulators to their POR state) or a Mariko (vdd_core
/// 1.05 V, SD2 on for LDO0/1/8).
fn config_regulators(t210: bool, hoag: bool) {
    if t210 {
        pmic::set_voltage(REGULATOR_LDO4, 1_000_000);
    }
    pmic::low_battery_monitor(false);

    set_pmc(
        APBDEV_PMC_NO_IOPOWER,
        pmc(APBDEV_PMC_NO_IOPOWER) & (PMC_NO_IOPOWER_MEM_COMP | PMC_NO_IOPOWER_SDMMC1 | PMC_NO_IOPOWER_MEM),
    );
    // SD card I/O and core off: its power cycle starts here.
    pmic::enable(REGULATOR_LDO2, false);
    gpio_write(GPIO_PORT_E, GPIO_PIN_4, false);
    set_pmc(APBDEV_PMC_NO_IOPOWER, pmc(APBDEV_PMC_NO_IOPOWER) | PMC_NO_IOPOWER_SDMMC1);
    let _ = pmc(APBDEV_PMC_NO_IOPOWER);
    unsafe { SD_POWER_CUT_MS = now_ms() };

    pmic_byte(MAX77620_REG_CNFGBBC, MAX77620_CNFGBBC_RESISTOR_1K);
    pmic_byte(
        MAX77620_REG_ONOFFCNFG1,
        MAX77620_ONOFFCNFG1_RSVD | 3 << MAX77620_ONOFFCNFG1_MRT_SHIFT,
    );

    if t210 {
        pmic_byte(MAX77620_REG_FPS_CFG0, 7 << MAX77620_FPS_TIME_PERIOD_SHIFT);
        pmic_byte(
            MAX77620_REG_FPS_CFG1,
            7 << MAX77620_FPS_TIME_PERIOD_SHIFT | 1 << MAX77620_FPS_EN_SRC_SHIFT,
        );
        pmic_byte(MAX77620_REG_FPS_CFG2, 7 << MAX77620_FPS_TIME_PERIOD_SHIFT);
        for id in [REGULATOR_LDO4, REGULATOR_LDO8, REGULATOR_SD0, REGULATOR_SD1, REGULATOR_SD3] {
            pmic::config_fps(id);
        }
        pmic_byte(
            MAX77620_REG_FPS_GPIO3,
            4 << MAX77620_FPS_PU_PERIOD_SHIFT | 2 << MAX77620_FPS_PD_PERIOD_SHIFT,
        );
        pmic::set_voltage(REGULATOR_SD0, 1_125_000);
        pmic::config_gpio(5, false);
        pmic::config_gpio(6, false);
        pmic::max77621_config_default(REGULATOR_CPU0, true);
        pmic::max77621_config_default(REGULATOR_GPU0, true);
    } else {
        pmic::set_voltage(REGULATOR_SD0, 1_050_000);
        pmic::set_voltage(REGULATOR_SD2, 1_325_000);
        pmic_byte(
            MAX77620_REG_SD2_CFG,
            1 << MAX77620_SD_SR_SHIFT
                | MAX77620_POWER_MODE_NORMAL << MAX77620_SD_POWER_MODE_SHIFT
                | MAX77620_SD_CFG1_FSRADE_SD_ENABLE,
        );
        if hoag {
            pmic::set_voltage(REGULATOR_LDO8, 2_800_000);
            pmic::enable(REGULATOR_LDO8, true);
        }
    }
}

/// `hw_init`.
pub fn init() {
    let t210 = soc::is_t210();
    let hoag = soc::hw_type() == HW_HOAG;

    config_se_brom();

    // Unset APB2JTAG_OVERRIDE_EN and OBS_OVERRIDE_EN.
    rmw(SYSREG_BASE + AHB_AHB_SPARE_REG, 0xFFFF_FF9F, 0);
    rmw(PMC_BASE + APBDEV_PMC_SCRATCH49, 0xFFFF_FFFC, 0);

    if t210 {
        mbist_workaround();
    }

    // PLLP_OUT3/4 at 408 MHz, enabled.
    set_car(CLK_RST_CONTROLLER_PLLP_OUTB, 0x30003);

    // clock_enable_se: the chip id read again, as hekate.
    clock::enable(&SE_CLK);
    if !soc::is_t210() {
        set_car(CLK_RST_CONTROLLER_CLK_SOURCE_SE, car(CLK_RST_CONTROLLER_CLK_SOURCE_SE) | 1 << 8);
    }
    soc::fuse_visible(true);
    soc::fuse_disable_program();
    soc::mc_enable();

    config_oscillators();
    config_gpios(hoag);

    // CL-DVFS clock on: it shares I2C5's pads.
    clock::enable(&CL_DVFS_CLK);
    enable_i2c_clock(&I2C1_CLK);
    enable_i2c_clock(&I2C5_CLK);
    clock::enable(&TZRAM_CLK);

    i2c::init(I2C_5);
    config_regulators(t210, hoag);
    i2c::init(I2C_1);
    config_pmc_scratch();

    // BPMP/SCLK on PLLP_OUT (408 MHz).
    set_car(CLK_RST_CONTROLLER_SCLK_BURST_POLICY, 0x2000_3333);

    if !t210 {
        set_pmc(
            APBDEV_PMC_TZRAM_PWR_CNTRL_B01,
            pmc(APBDEV_PMC_TZRAM_PWR_CNTRL_B01) & !PMC_TZRAM_PWR_CNTRL_SD,
        );
        set_pmc(
            APBDEV_PMC_TZRAM_NON_SEC_DISABLE_B01,
            PMC_TZRAM_DISABLE_REG_WRITE | PMC_TZRAM_DISABLE_REG_READ,
        );
        set_pmc(
            APBDEV_PMC_TZRAM_SEC_DISABLE_B01,
            PMC_TZRAM_DISABLE_REG_WRITE | PMC_TZRAM_DISABLE_REG_READ,
        );
    }

    config_arbiter(false);
    crate::sdram::init();
    soc::bpmp_mmu_enable();
    clock::enable(&HOST1X_CLK);
}

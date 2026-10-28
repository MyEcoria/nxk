//! The CPU cluster's start (hekate bdk/soc/ccplex.c `ccplex_boot_cpu0`,
//! its own path for HOS and Mariko L4T): CPU rail, PLLX at 998.4 MHz,
//! power partitions, RAM repair, then CPU0 out of reset at `entry` in
//! AArch64.

use crate::clock::{self, car, set_car, ClkRst};
use crate::hdr::*;
use crate::pmic;
use crate::reg::{rd, wr};
use crate::soc;
use crate::timer::usleep;

const CORESIGHT_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_U_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_U_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_CSITE,
    index: CLK_U_CSITE,
    clk_src: 0,
    clk_div: clock::src_div(3),
};

const POWER_RAIL_CRAIL: u32 = 0;
const POWER_RAIL_CE0: u32 = 14;
const POWER_RAIL_C0NC: u32 = 15;

/// `_ccplex_enable_power_t210`: the MAX77621 behind PMIC GPIO5, 950 mV.
fn enable_power_t210() {
    pmic::config_gpio(5, true);
    pmic::max77621_config_default(REGULATOR_CPU0, MAX77621_CTRL_HOS_CFG != 0);
    pmic::set_voltage(REGULATOR_CPU0, 950_000);
    pmic::enable(REGULATOR_CPU0, true);
}

/// `_ccplex_enable_power_t210b01`: the MAX77812 CPU buck, 800 mV.
fn enable_power_t210b01() {
    pmic::set_voltage(REGULATOR_CPU1, 800_000);
    pmic::enable(REGULATOR_CPU1, true);
}

/// `clock_enable_pllx`: 38.4 MHz / 2 x 156 / 2 = 998.4 MHz, if off.
fn enable_pllx() {
    if car(CLK_RST_CONTROLLER_PLLX_BASE) & PLL_BASE_ENABLE == 0 {
        set_car(CLK_RST_CONTROLLER_PLLX_MISC_3, car(CLK_RST_CONTROLLER_PLLX_MISC_3) & !PLLX_MISC3_IDDQ);
        usleep(2);
        let div = 2 << 20 | 156 << 8 | 2;
        set_car(CLK_RST_CONTROLLER_PLLX_BASE, PLL_BASE_BYPASS | div);
        set_car(CLK_RST_CONTROLLER_PLLX_BASE, div);
        set_car(CLK_RST_CONTROLLER_PLLX_MISC, car(CLK_RST_CONTROLLER_PLLX_MISC) | PLLX_MISC_LOCK_EN);
        set_car(CLK_RST_CONTROLLER_PLLX_BASE, PLL_BASE_ENABLE | div);
    }
    // _clock_pll_wait_lock(PLLX_BASE, 300).
    for _ in 0..300 {
        if car(CLK_RST_CONTROLLER_PLLX_BASE) & PLL_BASE_LOCK != 0 {
            break;
        }
        usleep(1);
    }
    usleep(2);
}

/// `pmc_domain_pwrgate_set(part, true)`; false if it never toggled.
fn power_partition(part: u32) -> bool {
    let pmc = |off| rd(PMC_BASE + off);
    let mask = 1 << part;
    if pmc(APBDEV_PMC_PWRGATE_STATUS) & mask == mask {
        return true;
    }
    let mut tries = 5000;
    while pmc(APBDEV_PMC_PWRGATE_TOGGLE) & PMC_PWRGATE_TOGGLE_START != 0 {
        usleep(1);
        tries -= 1;
        if tries < 1 {
            return false;
        }
    }
    wr(PMC_BASE + APBDEV_PMC_PWRGATE_TOGGLE, part | PMC_PWRGATE_TOGGLE_START);
    let mut tries = 5000;
    while pmc(APBDEV_PMC_PWRGATE_STATUS) & mask != mask {
        usleep(1);
        tries -= 1;
        if tries < 1 {
            return false;
        }
    }
    true
}

/// `ccplex_boot_cpu0(entry, lock)`.
pub fn boot_cpu0(entry: u32, lock: bool) {
    let flow = FLOW_CTLR_BASE + FLOW_CTLR_BPMP_CLUSTER_CONTROL;
    wr(flow, rd(flow) & !CLUSTER_CTRL_ACTIVE_SLOW);
    if soc::is_t210() {
        enable_power_t210();
    } else {
        enable_power_t210b01();
    }
    enable_pllx();

    // MSELECT at 102 MHz, the CPU clock from PLLX.
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_MSELECT, clock::src_div(4));
    set_car(CLK_RST_CONTROLLER_CLK_ENB_V_SET, 1 << CLK_V_MSELECT);
    set_car(CLK_RST_CONTROLLER_CCLK_BURST_POLICY, 0x2000_8888);
    set_car(CLK_RST_CONTROLLER_SUPER_CCLK_DIVIDER, 1 << 31);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_V_SET, 1 << CLK_V_CPUG);
    clock::enable(&CORESIGHT_CLK);

    // CAR2PMC_CPU_ACK_WIDTH to 0.
    set_car(CLK_RST_CONTROLLER_CPU_SOFTRST_CTRL2, car(CLK_RST_CONTROLLER_CPU_SOFTRST_CTRL2) & 0xFFFF_F000);

    power_partition(POWER_RAIL_CRAIL);
    power_partition(POWER_RAIL_C0NC);
    power_partition(POWER_RAIL_CE0);

    // RAM repair, needed by the fast cluster.
    wr(FLOW_CTLR_BASE + FLOW_CTLR_RAM_REPAIR, RAM_REPAIR_REQ);
    while rd(FLOW_CTLR_BASE + FLOW_CTLR_RAM_REPAIR) & RAM_REPAIR_STS == 0 {}

    wr(EXCP_VEC_BASE + EVP_CPU_RESET_VECTOR, 0);
    wr(SB_BASE + SB_AA64_RESET_LOW, entry | SB_AA64_RST_AARCH64_MODE_EN);
    wr(SB_BASE + SB_AA64_RESET_HIGH, 0);
    if lock {
        wr(SB_BASE + SB_CSR, SB_CSR_NS_RST_VEC_WR_DIS);
        let _ = rd(SB_BASE + SB_CSR);
    }

    set_car(CLK_RST_CONTROLLER_RST_DEV_V_CLR, 1 << CLK_V_MSELECT);
    set_car(CLK_RST_CONTROLLER_RST_CPUG_CMPLX_CLR, 1 << 29); // NONCPU reset off
    set_car(
        CLK_RST_CONTROLLER_RST_CPUG_CMPLX_CLR,
        1 << 30 | 1 << 24 | 1 << 16 | 1 << 0,
    );
}

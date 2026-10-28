//! The hand-off to nxk-bl31 as hekate's L4T launch does it
//! (bootloader/l4t/l4t.c `launch_l4t`, from `sd_end` on): bus and pin
//! state for the next stage, `hw_deinit`, `_l4t_late_hw_config`, then the
//! CPU started at bl31's cold entry and the BPMP halted.

use crate::ccplex;
use crate::clock::{self, car, set_car};
use crate::hdr::*;
use crate::hw;
use crate::reg::{rd, wr};
use crate::soc;

/// nxk-bl31's entry: the start of TZDRAM.
pub const TZDRAM_COLD_ENTRY: u32 = 0xFFF0_0000;

/// UART-B, as hekate's `uart_port=2` in `hwboot/nxk.ini` leaves it.
const UARTB_CLK: clock::ClkRst = clock::ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_UARTB,
    index: CLK_L_UARTB,
    clk_src: 0,
    clk_div: clock::src_div(2),
};

/// `_l4t_late_hw_config`.
fn late_hw_config(t210b01: bool) {
    let mut i = 0;
    while i < SYSCTR0_COUNTERS {
        wr(SYSCTR0_BASE + SYSCTR0_COUNTERS_BASE + i, 0);
        i += 4;
    }
    // PMIC config scratch: max77621/max77812 CPU regulator.
    wr(PMC_BASE + APBDEV_PMC_SCRATCH201, 1 << 1);
    let ovr = PMC_BASE + APBDEV_PMC_PLLP_WB0_OVERRIDE;
    wr(ovr, rd(ovr) & !PMC_PLLP_WB0_OVR_PLLM_OVR_ENABLE);
    let spare = SYSREG_BASE + AHB_AHB_SPARE_REG;
    if t210b01 && rd(spare) & 0xE000_0000 != 0xE000_0000 {
        wr(spare, 0xE0000 << 12);
    }
    wr(PMC_BASE + APBDEV_PMC_STICKY_BITS, PMC_STICKY_BITS_HDA_LPBK_DIS);
    wr(MC_BASE + MC_INTSTATUS, rd(MC_BASE + MC_INTSTATUS));
    let ms = MSELECT_BASE + MSELECT_CONFIG;
    wr(
        ms,
        rd(ms) & !(MSELECT_CFG_ERR_RESP_EN_GPU | MSELECT_CFG_ERR_RESP_EN_PCIE)
            | MSELECT_CFG_WRAP_TO_INCR_GPU
            | MSELECT_CFG_WRAP_TO_INCR_PCIE
            | MSELECT_CFG_WRAP_TO_INCR_BPMP,
    );
}

/// `hw_deinit(true)` without the display or Minerva parts nxk-ipl does
/// not have yet: BPMP cache off, arbiter reset, APE clocks back on (T210).
fn hw_deinit() {
    soc::bpmp_mmu_disable();
    hw::config_arbiter(true);
    if soc::is_t210() {
        set_car(CLK_RST_CONTROLLER_CLK_ENB_V_SET, 1 << CLK_V_AHUB);
        set_car(CLK_RST_CONTROLLER_CLK_ENB_Y_SET, 1 << CLK_Y_APE);
    }
}

/// Start nxk-bl31 on CPU0 and stop the BPMP.
pub fn launch() -> ! {
    let t210b01 = !soc::is_t210();
    soc::mc_disable_ahb_redirect();

    // Debug port as hekate's uart_port=2, then UART-B/C TX pins back to
    // their function (they were Joy-Con detect inputs).
    soc::pinmux_config_uart(1);
    clock::enable(&UARTB_CLK);
    soc::gpio_config(GPIO_PORT_G, GPIO_PIN_0, false);
    soc::gpio_config(GPIO_PORT_D, GPIO_PIN_1, false);

    hw_deinit();
    late_hw_config(t210b01);
    let _ = car(CLK_RST_CONTROLLER_RST_DEVICES_L);
    ccplex::boot_cpu0(TZDRAM_COLD_ENTRY, true);
    crate::halt()
}

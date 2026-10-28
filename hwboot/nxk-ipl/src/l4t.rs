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

const SZ_128K: u32 = 128 << 10;
const SZ_1M: u32 = 1 << 20;
/// Generalized carveouts go down from under TZDRAM (hekate's l4t_config:
/// SECFW on, NVDEC/TSEC off).
const GEN_CARVEOUT_TOP: u32 = TZDRAM_COLD_ENTRY - SZ_1M;
const CARVEOUT_SECFW_SIZE: u32 = SZ_1M;
const CARVEOUT_GPUFW_SIZE: u32 = 256 << 10;
const CARVEOUT_GPUWPR_SIZE: u32 = (512 + 256) << 10;
const FALCON_DMA_PAGE_SIZE: u32 = 0x100;

fn mc(off: u32, v: u32) {
    wr(MC_BASE + off, v);
}

fn aperture(id: u32) -> u32 {
    id << 11
}

/// GSC `n`'s registers (`MC_SECURITY_CARVEOUTn_*`, 0x50 apart).
fn carveout(n: u32, bom: u32, size: u32, access: [u32; 5], cfg: u32) {
    let b = MC_SECURITY_CARVEOUT1_BOM + (n - 1) * 0x50;
    mc(b, bom);
    mc(b + 4, 0); // BOM_HI
    mc(b + 8, size / SZ_128K);
    for (k, &a) in access.iter().enumerate() {
        mc(MC_SECURITY_CARVEOUT1_CLIENT_ACCESS0 + (n - 1) * 0x50 + 4 * k as u32, a);
    }
    mc(MC_SECURITY_CARVEOUT1_CFG0 + (n - 1) * 0x50, cfg);
}

/// `_l4t_mc_config_carveout` with hekate's configuration: VPR and the
/// TZDRAM carveout off (bl31 sets its own), GSC1 over the secure firmware
/// area (Minerva's table among it), GSC2 the GPU firmware's WPR page
/// (cleared, ACR's GSC3 magic and the TSEC init words at its end), GSC3
/// the GPU WPR, GSC4/5 (TSEC) empty. The LP0 copy into PMC scratch
/// registers (for SC7) is not done yet.
fn config_carveouts(t210b01: bool) {
    mc(MC_VIDEO_PROTECT_BOM, 0);
    mc(MC_VIDEO_PROTECT_SIZE_MB, 0);
    mc(MC_VIDEO_PROTECT_REG_CTRL, VPR_CTRL_TZ_SECURE | VPR_CTRL_LOCKED);
    mc(MC_SEC_CARVEOUT_BOM, 0);
    mc(MC_SEC_CARVEOUT_SIZE_MB, 0);
    mc(MC_SEC_CARVEOUT_REG_CTRL, 0);

    let mut base = GEN_CARVEOUT_TOP;
    // Minerva's table place (MTCTABLE_BASE, 256 KiB): zero until nxk-ipl
    // trains the DRAM, so `emc` finds no table (revision 0) rather than
    // whatever DRAM held at power-on.
    unsafe { core::ptr::write_bytes((GEN_CARVEOUT_TOP + (512 << 10)) as *mut u8, 0, 256 << 10) };
    soc::bpmp_mmu_maintenance(18, false); // BPMP_MMU_MAINT_INVALID_WAY
    carveout(
        1,
        base,
        CARVEOUT_SECFW_SIZE,
        [
            SEC_CARVEOUT_CA0_R_BPMP_C | SEC_CARVEOUT_CA0_R_PPCSAHBSLV,
            SEC_CARVEOUT_CA1_W_BPMP_C
                | SEC_CARVEOUT_CA1_R_CCPLEX_C
                | SEC_CARVEOUT_CA1_R_CCPLEXLP_C
                | SEC_CARVEOUT_CA1_W_CCPLEX_C
                | SEC_CARVEOUT_CA1_W_CCPLEXLP_C
                | SEC_CARVEOUT_CA1_W_PPCSAHBSLV,
            0,
            0,
            0,
        ],
        SEC_CARVEOUT_CFG_RD_NS | SEC_CARVEOUT_CFG_RD_SEC | SEC_CARVEOUT_CFG_WR_NS | SEC_CARVEOUT_CFG_WR_SEC | SEC_CARVEOUT_CFG_LOCKED,
    );

    // ALIGN(CARVEOUT_GPUFW_SIZE, SZ_1M): a whole MiB down.
    base -= SZ_1M;
    unsafe {
        core::ptr::write_bytes(base as *mut u8, 0, CARVEOUT_GPUFW_SIZE as usize);
        ((base + CARVEOUT_GPUFW_SIZE - 4) as *mut u32).write_volatile(ACR_GSC3_ENABLE_MAGIC);
        // tsec_init_t: SKU NX, HDCP 2.2, SoC id.
        let init = (base + CARVEOUT_GPUFW_SIZE - FALCON_DMA_PAGE_SIZE) as *mut u32;
        init.write_volatile(0x83);
        init.add(1).write_volatile(2);
        init.add(2).write_volatile(if t210b01 { 0x214 } else { 0x210 });
    }
    soc::bpmp_mmu_maintenance(18, false);
    let gpu_cfg = SEC_CARVEOUT_CFG_LOCKED
        | SEC_CARVEOUT_CFG_UNTRANSLATED_ONLY
        | SEC_CARVEOUT_CFG_RD_NS
        | SEC_CARVEOUT_CFG_RD_SEC
        | SEC_CARVEOUT_CFG_RD_FALCON_LS
        | SEC_CARVEOUT_CFG_RD_FALCON_HS
        | SEC_CARVEOUT_CFG_WR_FALCON_LS
        | SEC_CARVEOUT_CFG_WR_FALCON_HS
        | SEC_CARVEOUT_CFG_SEND_CFG_TO_GPU
        | SEC_CARVEOUT_CFG_FORCE_APERTURE_ID_MATCH;
    carveout(
        2,
        base,
        CARVEOUT_GPUFW_SIZE,
        [
            0,
            0,
            SEC_CARVEOUT_CA2_R_GPU | SEC_CARVEOUT_CA2_W_GPU | SEC_CARVEOUT_CA2_R_TSEC,
            0,
            SEC_CARVEOUT_CA4_R_GPU2 | SEC_CARVEOUT_CA4_W_GPU2,
        ],
        gpu_cfg | aperture(2),
    );
    carveout(3, base + CARVEOUT_GPUFW_SIZE, CARVEOUT_GPUWPR_SIZE, [0; 5], gpu_cfg | aperture(3));

    let tsec_cfg = SEC_CARVEOUT_CFG_LOCKED
        | SEC_CARVEOUT_CFG_RD_FALCON_HS
        | SEC_CARVEOUT_CFG_WR_FALCON_HS
        | SEC_CARVEOUT_CFG_FORCE_APERTURE_ID_MATCH;
    // hekate writes only CLIENT_ACCESS2 and 4 of these two.
    for (n, a4) in [(4, SEC_CARVEOUT_CA4_R_TSECB | SEC_CARVEOUT_CA4_W_TSECB), (5, 0)] {
        let b = MC_SECURITY_CARVEOUT1_BOM + (n - 1) * 0x50;
        mc(b, 0);
        mc(b + 4, 0);
        mc(b + 8, 0);
        mc(MC_SECURITY_CARVEOUT1_CLIENT_ACCESS2 + (n - 1) * 0x50, SEC_CARVEOUT_CA2_R_TSEC | SEC_CARVEOUT_CA2_W_TSEC);
        if a4 != 0 {
            mc(MC_SECURITY_CARVEOUT1_CLIENT_ACCESS4 + (n - 1) * 0x50, a4);
        }
        mc(MC_SECURITY_CARVEOUT1_CFG0 + (n - 1) * 0x50, tsec_cfg | aperture(n));
    }
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

    config_carveouts(t210b01);
    hw_deinit();
    late_hw_config(t210b01);
    let _ = car(CLK_RST_CONTROLLER_RST_DEVICES_L);
    ccplex::boot_cpu0(TZDRAM_COLD_ENTRY, true);
    crate::halt()
}

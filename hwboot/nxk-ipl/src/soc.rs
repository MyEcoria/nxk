//! Small SoC blocks hw_init needs (hekate bdk/soc/{fuse,gpio,pinmux,
//! bpmp}.c, bdk/mem/mc.c, bdk/sec/se.c): chip id, fuses, GPIO direction,
//! I2C pads, the memory controller clocks, the SBK keyslot, the BPMP cache.

use crate::clock::{car, set_car};
use crate::hdr::*;
use crate::reg::{rd, rmw, wr};

/// `hw_get_chip_id`.
pub fn chip_id() -> u32 {
    if (rd(APB_MISC_BASE + APB_MISC_GP_HIDREV) >> 4) & 0xF >= GP_HIDREV_MAJOR_T210B01 {
        GP_HIDREV_MAJOR_T210B01
    } else {
        GP_HIDREV_MAJOR_T210
    }
}

pub fn is_t210() -> bool {
    chip_id() == GP_HIDREV_MAJOR_T210
}

pub fn fuse(off: u32) -> u32 {
    rd(FUSE_BASE + off)
}

/// `fuse_read_hw_type`: Icosa (Erista) 0, Iowa 1, Hoag (Lite) 2, Aula (OLED) 3.
pub const HW_ICOSA: u32 = 0;
pub const HW_IOWA: u32 = 1;
pub const HW_HOAG: u32 = 2;
pub const HW_AULA: u32 = 3;
pub fn hw_type() -> u32 {
    if is_t210() {
        return HW_ICOSA;
    }
    match (fuse(FUSE_RESERVED_ODM4) & 0xF0000) >> 16 {
        2 => HW_HOAG,
        4 => HW_AULA,
        _ => HW_IOWA,
    }
}

/// `clock_enable_fuse`.
pub fn fuse_visible(on: bool) {
    let v = car(CLK_RST_CONTROLLER_MISC_CLK_ENB) & 0xEFFF_FFFF | (on as u32) << 28;
    set_car(CLK_RST_CONTROLLER_MISC_CLK_ENB, v);
}

/// `fuse_set_sbk`: the secure boot key from the fuses into SE keyslot 14,
/// then made unreadable (a console whose key fuses read all ones has none).
pub fn fuse_set_sbk() {
    if fuse(FUSE_PRIVATE_KEY0) == 0xFFFF_FFFF {
        return;
    }
    let sbk = [
        fuse(FUSE_PRIVATE_KEY0),
        fuse(FUSE_PRIVATE_KEY1),
        fuse(FUSE_PRIVATE_KEY2),
        fuse(FUSE_PRIVATE_KEY3),
    ];
    for (i, &k) in sbk.iter().enumerate() {
        wr(SE_BASE + SE_CRYPTO_KEYTABLE_ADDR_REG, 14 << 4 | KEYS_0_3 << 2 | i as u32);
        wr(SE_BASE + SE_CRYPTO_KEYTABLE_DATA_REG, k);
    }
    // se_key_acc_ctrl(14, SE_KEY_TBL_DIS_KEYREAD_FLAG).
    let flags = SE_KEY_TBL_DIS_KEYREAD_FLAG;
    if flags & SE_KEY_TBL_DIS_KEY_ACCESS_FLAG != 0 {
        wr(SE_BASE + SE_CRYPTO_KEYTABLE_ACCESS_REG + 4 * 14, !flags);
    }
    if flags & SE_KEY_LOCK_FLAG != 0 {
        rmw(SE_BASE + SE_CRYPTO_SECURITY_PERKEY_REG, !(1 << 14), 0);
    }
}

/// `fuse_disable_program`.
pub fn fuse_disable_program() {
    wr(FUSE_BASE + FUSE_DISABLEREGPROGRAM, 1);
}

// GPIO (port numbers and pins as hekate's GPIO_PORT_x / GPIO_PIN_x).
fn gpio_off(port: u32) -> u32 {
    ((port >> 2) << 8) + ((port % 4) << 2)
}

fn gpio_bits(reg: u32, port: u32, pins: u32, set: bool) {
    let a = GPIO_BASE + reg + gpio_off(port);
    if set {
        wr(a, rd(a) | pins);
    } else {
        wr(a, rd(a) & !pins);
    }
    let _ = rd(a);
}

/// `gpio_config`: GPIO mode (true) or the pin's function (SPIO).
pub fn gpio_config(port: u32, pins: u32, gpio: bool) {
    gpio_bits(0x00, port, pins, gpio);
}

pub fn gpio_output_enable(port: u32, pins: u32, on: bool) {
    gpio_bits(0x10, port, pins, on);
}

pub fn gpio_write(port: u32, pins: u32, high: bool) {
    gpio_bits(0x20, port, pins, high);
}

pub fn gpio_read(port: u32, pin: u32) -> bool {
    rd(GPIO_BASE + 0x30 + gpio_off(port)) & pin != 0
}

pub fn gpio_direction_input(port: u32, pins: u32) {
    gpio_config(port, pins, true);
    gpio_output_enable(port, pins, false);
}

pub fn gpio_direction_output(port: u32, pins: u32, high: bool) {
    gpio_config(port, pins, true);
    gpio_write(port, pins, high);
    gpio_output_enable(port, pins, true);
}

pub fn pinmux(off: u32, v: u32) {
    wr(PINMUX_AUX_BASE + off, v);
}

pub fn pinmux_rd(off: u32) -> u32 {
    rd(PINMUX_AUX_BASE + off)
}

/// `pinmux_config_i2c` for I2C1 or I2C5.
pub fn pinmux_config_i2c(idx: u32) {
    pinmux(0xBC + 8 * idx, PINMUX_INPUT_ENABLE);
    pinmux(0xC0 + 8 * idx, PINMUX_INPUT_ENABLE);
}

/// `pinmux_config_uart`.
pub fn pinmux_config_uart(idx: u32) {
    pinmux(0xE4 + 0x10 * idx, 0);
    pinmux(0xE8 + 0x10 * idx, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    pinmux(0xEC + 0x10 * idx, 0);
    pinmux(0xF0 + 0x10 * idx, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
}

pub fn mc(off: u32) -> u32 {
    rd(MC_BASE + off)
}

pub fn set_mc(off: u32, v: u32) {
    wr(MC_BASE + off, v);
}

/// `mc_enable_ahb_redirect`: IRAM up to `DRAM_START - offset` seen over AHB.
pub fn mc_enable_ahb_redirect(offset: u32) {
    set_car(
        CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRD,
        car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRD) | 1 << 19,
    );
    set_mc(MC_IRAM_BOM, IRAM_BASE);
    set_mc(MC_IRAM_TOM, DRAM_START - offset);
}

/// `mc_disable_ahb_redirect`.
pub fn mc_disable_ahb_redirect() {
    set_mc(MC_IRAM_BOM, 0xFFFF_F000);
    set_mc(MC_IRAM_TOM, 0);
    set_car(
        CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRD,
        car(CLK_RST_CONTROLLER_LVL2_CLK_GATE_OVRD) & !(1 << 19),
    );
}

/// `mc_enable`: EMC source back to PLLP, memory clocks on and out of reset.
pub fn mc_enable() {
    let v = car(CLK_RST_CONTROLLER_CLK_SOURCE_EMC) & 0x1FFF_FFFF | 2 << 29;
    set_car(CLK_RST_CONTROLLER_CLK_SOURCE_EMC, v);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_H_SET, 1 << CLK_H_EMC | 1 << CLK_H_MEM);
    set_car(CLK_RST_CONTROLLER_CLK_ENB_X_SET, 1 << CLK_X_EMC_DLL);
    set_car(CLK_RST_CONTROLLER_RST_DEV_H_CLR, 1 << CLK_H_EMC | 1 << CLK_H_MEM);
    crate::timer::usleep(5);
    mc_enable_ahb_redirect(1);
}

// BPMP cache and MMU (hekate bpmp.c; the register layout is NVIDIA's AVP cache).
const CACHE_CONFIG: u32 = 0x0;
const CFG_ENABLE_CACHE: u32 = 1 << 0;
const CFG_FORCE_WRITE_THROUGH: u32 = 1 << 3;
const CFG_TAG_CHK_ABRT_ON_ERR: u32 = 1 << 14;
const MAINT_REQ: u32 = 0x28;
const INT_CLEAR: u32 = 0x44;
const INT_RAW_EVENT: u32 = 0x48;
const INT_MAINT_DONE: u32 = 1 << 0;
const MMU_FALLBACK_ENTRY: u32 = 0xA0;
const MMU_SHADOW_COPY_MASK: u32 = 0xA4;
const MMU_CFG: u32 = 0xAC;
const MMU_CFG_SEQ_EN: u32 = 1 << 1;
const MMU_CFG_TLB_EN: u32 = 1 << 2;
const MMU_CFG_ABORT_STORE_LAST: u32 = 1 << 4;
const MMU_CMD: u32 = 0xB0;
const MMU_CMD_INIT: u32 = 1;
const MMU_CMD_COPY_SHADOW: u32 = 2;
const MMU_SHADOW_ENTRY: u32 = 0x400;
const MMU_EN_CACHED: u32 = 1 << 0;
const MMU_EN_EXEC: u32 = 1 << 1;
const MMU_EN_READ: u32 = 1 << 2;
const MMU_EN_WRITE: u32 = 1 << 3;
pub const MAINT_CLN_INV_WAY: u32 = 19;
const MAINT_INVALID_WAY: u32 = 18;
const LINE: u32 = 0x20;

fn cache(off: u32) -> u32 {
    rd(BPMP_CACHE_BASE + off)
}

fn set_cache(off: u32, v: u32) {
    wr(BPMP_CACHE_BASE + off, v);
}

/// `bpmp_mmu_maintenance`.
pub fn bpmp_mmu_maintenance(op: u32, force: bool) {
    if !force && cache(CACHE_CONFIG) & CFG_ENABLE_CACHE == 0 {
        return;
    }
    set_cache(INT_CLEAR, INT_MAINT_DONE);
    set_cache(MAINT_REQ, 0xF << 8 | op);
    while cache(INT_RAW_EVENT) & INT_MAINT_DONE == 0 {}
    set_cache(INT_CLEAR, cache(INT_RAW_EVENT));
}

/// `bpmp_mmu_enable`: DRAM and IRAM cached (write-through), the rest RWX uncached.
pub fn bpmp_mmu_enable() {
    if cache(CACHE_CONFIG) & CFG_ENABLE_CACHE != 0 {
        return;
    }
    set_cache(MMU_CMD, MMU_CMD_INIT);
    set_cache(MMU_FALLBACK_ENTRY, MMU_EN_READ | MMU_EN_WRITE | MMU_EN_EXEC);
    set_cache(MMU_CFG, MMU_CFG_SEQ_EN | MMU_CFG_TLB_EN | MMU_CFG_ABORT_STORE_LAST);
    set_cache(MMU_SHADOW_COPY_MASK, 0);
    let all = MMU_EN_READ | MMU_EN_WRITE | MMU_EN_EXEC | MMU_EN_CACHED;
    for (idx, (start, end)) in [(DRAM_START, 0xFFFF_FFFFu32), (IRAM_BASE, 0x4003_FFFF)]
        .into_iter()
        .enumerate()
    {
        let e = BPMP_CACHE_BASE + MMU_SHADOW_ENTRY + 16 * idx as u32;
        wr(e, (start + LINE - 1) & !(LINE - 1));
        wr(e + 4, end & !(LINE - 1));
        wr(e + 8, all);
        set_cache(MMU_SHADOW_COPY_MASK, cache(MMU_SHADOW_COPY_MASK) | 1 << idx);
    }
    set_cache(MMU_CMD, MMU_CMD_COPY_SHADOW);
    bpmp_mmu_maintenance(MAINT_INVALID_WAY, true);
    set_cache(
        CACHE_CONFIG,
        CFG_ENABLE_CACHE | CFG_FORCE_WRITE_THROUGH | CFG_TAG_CHK_ABRT_ON_ERR,
    );
    bpmp_mmu_maintenance(MAINT_INVALID_WAY, false);
}

/// `bpmp_mmu_disable`: flush, then cache off.
pub fn bpmp_mmu_disable() {
    if cache(CACHE_CONFIG) & CFG_ENABLE_CACHE == 0 {
        return;
    }
    bpmp_mmu_maintenance(MAINT_CLN_INV_WAY, false);
    set_cache(CACHE_CONFIG, 0);
}

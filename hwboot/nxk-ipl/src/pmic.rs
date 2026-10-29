//! The PMIC (MAX77620) and the CPU/GPU/RAM regulators (MAX77621 on an
//! Erista, MAX77812 on a Mariko), over I2C5 (hekate bdk/power/max7762x.c,
//! same table and the same retries and ramp delays).

use crate::hdr::*;
use crate::i2c::{self, I2C_5};
use crate::reg::{rd, FUSE};
use crate::timer::usleep;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sd,
    Ldo,
    /// MAX77621.
    Bc0,
    /// MAX77812.
    Bc1,
}

/// One regulator (hekate `max77620_regulator_t`); `cfg` is the FPS
/// address, source, power-down and power-up periods for SD/LDO, the
/// control defaults (CTRL1 POR, CTRL1 HOS, CTRL2 POR, CTRL2 HOS) for a
/// MAX77621, the enable mask and shift for a MAX77812.
struct Regulator {
    uv_step: u32,
    uv_min: u32,
    uv_default: u32,
    uv_max: u32,
    kind: Kind,
    volt_addr: u32,
    cfg_addr: u32,
    volt_mask: u32,
    cfg: [u32; 4],
}

const fn r(
    step: u32,
    min: u32,
    def: u32,
    max: u32,
    kind: Kind,
    volt: u32,
    cfg_addr: u32,
    mask: u32,
    cfg: [u32; 4],
) -> Regulator {
    Regulator {
        uv_step: step,
        uv_min: min,
        uv_default: def,
        uv_max: max,
        kind,
        volt_addr: volt,
        cfg_addr,
        volt_mask: mask,
        cfg,
    }
}

use Kind::*;

#[rustfmt::skip]
static REGULATORS: [Regulator; 17] = [
    r(12500, 600000, 625000, 1400000, Sd, MAX77620_REG_SD0, MAX77620_REG_SD0_CFG, MAX77620_SD0_VOLT_MASK, [MAX77620_REG_FPS_SD0, 1, 7, 1]),
    r(12500, 600000, 1125000, 1237500, Sd, MAX77620_REG_SD1, MAX77620_REG_SD1_CFG, MAX77620_SD1_VOLT_MASK, [MAX77620_REG_FPS_SD1, 0, 1, 5]),
    r(12500, 600000, 1325000, 1350000, Sd, MAX77620_REG_SD2, MAX77620_REG_SD2_CFG, MAX77620_SDX_VOLT_MASK, [MAX77620_REG_FPS_SD2, 1, 5, 2]),
    r(12500, 600000, 1800000, 1800000, Sd, MAX77620_REG_SD3, MAX77620_REG_SD3_CFG, MAX77620_SDX_VOLT_MASK, [MAX77620_REG_FPS_SD3, 0, 3, 3]),
    r(25000, 800000, 1200000, 1200000, Ldo, MAX77620_REG_LDO0_CFG, MAX77620_REG_LDO0_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO0, 3, 7, 0]),
    r(25000, 800000, 1050000, 1050000, Ldo, MAX77620_REG_LDO1_CFG, MAX77620_REG_LDO1_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO1, 3, 7, 0]),
    r(50000, 800000, 1800000, 3300000, Ldo, MAX77620_REG_LDO2_CFG, MAX77620_REG_LDO2_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO2, 3, 7, 0]),
    r(50000, 800000, 3100000, 3100000, Ldo, MAX77620_REG_LDO3_CFG, MAX77620_REG_LDO3_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO3, 3, 7, 0]),
    r(12500, 800000, 850000, 1000000, Ldo, MAX77620_REG_LDO4_CFG, MAX77620_REG_LDO4_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO4, 0, 7, 1]),
    r(50000, 800000, 1800000, 1800000, Ldo, MAX77620_REG_LDO5_CFG, MAX77620_REG_LDO5_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO5, 3, 7, 0]),
    r(50000, 800000, 2900000, 2900000, Ldo, MAX77620_REG_LDO6_CFG, MAX77620_REG_LDO6_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO6, 3, 7, 0]),
    r(50000, 800000, 1050000, 1050000, Ldo, MAX77620_REG_LDO7_CFG, MAX77620_REG_LDO7_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO7, 1, 4, 3]),
    r(50000, 800000, 1050000, 2800000, Ldo, MAX77620_REG_LDO8_CFG, MAX77620_REG_LDO8_CFG2, MAX77620_LDO_VOLT_MASK, [MAX77620_REG_FPS_LDO8, 3, 7, 0]),
    r(6250, 606250, 1000000, 1400000, Bc0, MAX77621_REG_VOUT, MAX77621_REG_VOUT_DVS, MAX77621_DVC_DVS_VOLT_MASK, [MAX77621_CPU_CTRL1_POR_DEFAULT, MAX77621_CPU_CTRL1_HOS_DEFAULT, MAX77621_CPU_CTRL2_POR_DEFAULT, MAX77621_CPU_CTRL2_HOS_DEFAULT]),
    r(6250, 606250, 1200000, 1400000, Bc0, MAX77621_REG_VOUT, MAX77621_REG_VOUT_DVS, MAX77621_DVC_DVS_VOLT_MASK, [MAX77621_CPU_CTRL1_POR_DEFAULT, MAX77621_CPU_CTRL1_HOS_DEFAULT, MAX77621_CPU_CTRL2_POR_DEFAULT, MAX77621_CPU_CTRL2_HOS_DEFAULT]),
    r(5000, 250000, 600000, 1525000, Bc1, MAX77812_REG_M4_VOUT, MAX77812_REG_EN_CTRL, MAX77812_BUCK_VOLT_MASK, [MAX77812_EN_CTRL_EN_M4_MASK, MAX77812_EN_CTRL_EN_M4_SHIFT, 0, 0]),
    r(5000, 250000, 600000, 650000, Bc1, MAX77812_REG_M3_VOUT, MAX77812_REG_EN_CTRL, MAX77812_BUCK_VOLT_MASK, [MAX77812_EN_CTRL_EN_M3_MASK, MAX77812_EN_CTRL_EN_M3_SHIFT, 0, 0]),
];

/// `_max77812_get_address`: the phase configuration is fused.
fn max77812_address() -> u32 {
    if rd(FUSE + FUSE_RESERVED_ODM28_B01) & 1 == 0 {
        MAX77812_PHASE31_CPU_I2C_ADDR
    } else {
        MAX77812_PHASE211_CPU_I2C_ADDR
    }
}

/// `_max7762x_get_i2c_address`; 0 if the regulator is absent.
fn address(id: u32) -> u32 {
    match REGULATORS[id as usize].kind {
        Sd | Ldo => MAX77620_I2C_ADDR,
        Bc0 if id == REGULATOR_CPU0 => MAX77621_CPU_I2C_ADDR,
        Bc0 => MAX77621_GPU_I2C_ADDR,
        Bc1 => {
            let a = max77812_address();
            if id == REGULATOR_RAM0 && a == MAX77812_PHASE31_CPU_I2C_ADDR {
                0
            } else {
                a
            }
        }
    }
}

/// `_max7762x_set_reg`: up to 100 tries, 50 us apart.
fn set_reg(addr: u32, reg: u32, val: u32) {
    for _ in 0..100 {
        if i2c::send_byte(I2C_5, addr, reg, val as u8).is_ok() {
            break;
        }
        usleep(50);
    }
}

/// `max77620_regulator_config_fps`.
pub fn config_fps(id: u32) {
    if id > REGULATOR_LDO8 {
        return;
    }
    let [fps_addr, src, pd, pu] = REGULATORS[id as usize].cfg;
    set_reg(
        MAX77620_I2C_ADDR,
        fps_addr,
        src << MAX77620_FPS_SRC_SHIFT | pu << MAX77620_FPS_PU_PERIOD_SHIFT | pd << MAX77620_FPS_PD_PERIOD_SHIFT,
    );
}

/// `max7762x_regulator_set_voltage`; false if out of range or absent.
pub fn set_voltage(id: u32, uv: u32) -> bool {
    if id > REGULATOR_MAX {
        return false;
    }
    let reg = &REGULATORS[id as usize];
    if uv < reg.uv_min || uv > reg.uv_max {
        return false;
    }
    let addr = address(id);
    if addr == 0 {
        return false;
    }
    let mult = (uv + reg.uv_step - 1 - reg.uv_min) / reg.uv_step;
    let mut val = i2c::recv_byte(I2C_5, addr, reg.volt_addr) as u32;
    val = val & !reg.volt_mask & 0xFF | mult & reg.volt_mask;
    set_reg(addr, reg.volt_addr, val);
    if reg.kind == Bc0 {
        set_reg(addr, reg.cfg_addr, MAX77621_VOUT_ENABLE_MASK | val);
    }
    usleep(1000);
    true
}

/// `max7762x_regulator_enable`.
pub fn enable(id: u32, on: bool) -> bool {
    if id > REGULATOR_MAX {
        return false;
    }
    let reg = &REGULATORS[id as usize];
    let (reg_addr, val_on, mask, shift) = match reg.kind {
        Sd => (
            reg.cfg_addr,
            MAX77620_POWER_MODE_NORMAL,
            MAX77620_SD_POWER_MODE_MASK,
            MAX77620_SD_POWER_MODE_SHIFT,
        ),
        Ldo => (
            reg.volt_addr,
            MAX77620_POWER_MODE_NORMAL,
            MAX77620_LDO_POWER_MODE_MASK,
            MAX77620_LDO_POWER_MODE_SHIFT,
        ),
        Bc0 => (
            reg.volt_addr,
            MAX77621_VOUT_ENABLE,
            MAX77621_DVC_DVS_ENABLE_MASK,
            MAX77621_DVC_DVS_ENABLE_SHIFT,
        ),
        Bc1 => (reg.cfg_addr, MAX77812_EN_CTRL_ENABLE, reg.cfg[0], reg.cfg[1]),
    };
    let addr = address(id);
    if addr == 0 {
        return false;
    }
    let mut val = i2c::recv_byte(I2C_5, addr, reg_addr) as u32 & !mask & 0xFF;
    if on {
        val |= val_on << shift;
    }
    set_reg(addr, reg_addr, val);
    usleep(1000);
    true
}

/// `max77620_config_gpio`.
pub fn config_gpio(gpio: u32, enable: bool) {
    if gpio > 7 {
        return;
    }
    let val = i2c::recv_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_AME_GPIO) as u32;
    let _ = i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_AME_GPIO, (val & !(1 << gpio)) as u8);
    let cfg = if enable {
        MAX77620_CNFG_GPIO_OUTPUT_VAL_HIGH | MAX77620_CNFG_GPIO_DIR_OUTPUT | MAX77620_CNFG_GPIO_DRV_PUSHPULL
    } else {
        MAX77620_CNFG_GPIO_DIR_INPUT | MAX77620_CNFG_GPIO_DRV_OPENDRAIN
    };
    let _ = i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_GPIO0 + gpio, cfg as u8);
}

/// `max77621_config_default`.
pub fn max77621_config_default(id: u32, por: bool) {
    let reg = &REGULATORS[id as usize];
    if reg.kind != Bc0 {
        return;
    }
    let addr = address(id);
    if addr == 0 {
        return;
    }
    let (c1, c2) = if por {
        set_voltage(id, 1_000_000);
        enable(id, false);
        (reg.cfg[0], reg.cfg[2])
    } else {
        (reg.cfg[1], reg.cfg[3])
    };
    let _ = i2c::send_byte(I2C_5, addr, MAX77621_REG_CONTROL1, c1 as u8);
    let _ = i2c::send_byte(I2C_5, addr, MAX77621_REG_CONTROL2, c2 as u8);
}

/// `max77620_rtc_prep_read`: the RTC's read buffers updated (T210B01 R2C).
pub fn rtc_prep_read() {
    let _ = i2c::send_byte(I2C_5, MAX77620_RTC_I2C_ADDR, MAX77620_RTC_UPDATE0_REG, MAX77620_RTC_READ_UPDATE as u8);
}

/// `max77620_low_battery_monitor_config`.
pub fn low_battery_monitor(enable: bool) {
    set_reg(
        MAX77620_I2C_ADDR,
        MAX77620_REG_CNFGGLBL1,
        MAX77620_CNFGGLBL1_LBDAC_EN
            | if enable { MAX77620_CNFGGLBL1_MPPLD } else { 0 }
            | MAX77620_CNFGGLBL1_LBHYST_200
            | MAX77620_CNFGGLBL1_LBDAC_2800,
    );
}

/// Regulator defaults (`uv_default`), for a regulator set back to them.
pub fn default_uv(id: u32) -> u32 {
    REGULATORS[id as usize].uv_default
}

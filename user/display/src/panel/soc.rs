//! What the display path needs outside the display controller and DSI:
//! its clocks and PLLD (CAR), the panel's rails and reset (GPIO I and V,
//! their pads, the DSI supply LDO0 on the PMIC) as hekate's `display_init`
//! and `display_end` drive them (bdk/soc/clock.c, bdk/display/di.c).
//!
//! GPIO goes through the masked registers: other services share the
//! controller and a read-modify-write would race them. The PWM block is
//! left alone (the fan owns channel 1).

use libnx::drv::Mmio;
use libnx::i2c::I2c;
use libnx::time;

// CAR (byte offsets).
const RST_DEV_L_SET: usize = 0x300;
const RST_DEV_L_CLR: usize = 0x304;
const RST_DEV_H_SET: usize = 0x308;
const RST_DEV_H_CLR: usize = 0x30C;
const CLK_ENB_L_SET: usize = 0x320;
const CLK_ENB_L_CLR: usize = 0x324;
const CLK_ENB_H_SET: usize = 0x328;
const CLK_ENB_H_CLR: usize = 0x32C;
const CLK_ENB_W_SET: usize = 0x448;
const CLK_ENB_W_CLR: usize = 0x44C;
const CLK_ENB_X_SET: usize = 0x284;
const CLK_ENB_X_CLR: usize = 0x288;
const CLK_SOURCE_DISP1: usize = 0x138;
const CLK_SOURCE_DSIA_LP: usize = 0x620;
const CLK_SOURCE_UART_FST_MIPI_CAL: usize = 0x66C;
const PLLD_BASE: usize = 0xD0;
const PLLD_MISC1: usize = 0xD8;
const PLLD_MISC: usize = 0xDC;
const CLK_L_DISP1: u32 = 1 << 27;
const CLK_H_DSI: u32 = 1 << 16;
const CLK_H_MIPI_CAL: u32 = 1 << 24;
const CLK_W_DSIA_LP: u32 = 1 << 19;
const CLK_X_UART_FST_MIPI_CAL: u32 = 1 << 17;
/// `CLK_SRC_DIV(6)` on source 0 (a PLLP output): 17 MHz for the MIPI
/// calibration, 68 MHz for the DSI low-power clock.
const SRC_DIV_6: u32 = 10;
const DISP1_FROM_PLLD_OUT0: u32 = 2 << 29;
const PLL_ENABLE: u32 = 1 << 30;
const PLL_LOCK: u32 = 1 << 27;
const PLLD_MISC1_T210: u32 = 0x20;
/// PLLD_MISC: SDM enable (16), lock enable (18), bit 19 and DSI clock
/// enable (21) as hekate sets them, plus the SDM fraction in 15:0; IDDQ
/// (20) powers the analog part down (Linux clk-tegra210.c).
const PLLD_MISC_RUN: u32 = (1 << 21) | (1 << 19) | (1 << 18) | (1 << 16);
const PLLD_MISC_IDDQ: u32 = 1 << 20;
const SDM_HS: u32 = 0xFC00;
const SDM_LP: u32 = 0x0AAA;

// Pinmux (byte offsets from the pad control page).
const PAD_NFC_EN: usize = 0x1D0;
const PAD_NFC_INT: usize = 0x1D4;
const PAD_LCD_BL_PWM: usize = 0x1FC;
const PAD_LCD_BL_EN: usize = 0x200;
const PAD_LCD_RST: usize = 0x204;
const PAD_PULL_DOWN: u32 = 1 << 2;
const PAD_TRISTATE: u32 = 1 << 4;
const PAD_FUNC_MASK: u32 = 3;
const PAD_FUNC_PWM0: u32 = 1;

// GPIO: ports I (LCD AVDD channels) and V (backlight PWM and enable, panel reset).
const PORT_I: u32 = 8;
const PORT_V: u32 = 21;
const LCD_AVDD: u32 = 0b11;
const BL_PWM: u32 = 1 << 0;
const BL_EN: u32 = 1 << 1;
const LCD_RESET: u32 = 1 << 2;
const MSK_CNF: usize = 0x80;
const MSK_OE: usize = 0x90;
const MSK_OUT: usize = 0xA0;

// MAX77620 LDO0 (DSI AVDD): voltage in 5:0 (0.8 V + 25 mV), mode in 7:6.
const I2C_BUS: u8 = 5;
const PMIC: u8 = 0x3C;
const LDO0_CFG: u8 = 0x23;
const LDO0_1V2: u8 = ((1_200_000 - 800_000) / 25_000) as u8;
const LDO_MODE_MASK: u8 = 0xC0;
const LDO_MODE_NORMAL: u8 = 0xC0;

#[derive(Clone, Copy)]
pub struct Soc {
    pub car: Mmio,
    pub gpio: Mmio,
    pub pinmux: Mmio,
    pub i2c: I2c,
}

impl Soc {
    fn gpio_write(&self, port: u32, reg: usize, pins: u32, high: bool) {
        let off = (((port >> 2) << 8) + ((port & 3) << 2)) as usize + reg;
        self.gpio.w32(off, pins << 8 | if high { pins } else { 0 });
    }

    /// GPIO mode, level, then output enable (hekate `gpio_direction_output`).
    fn output(&self, port: u32, pins: u32, high: bool) {
        self.gpio_write(port, MSK_CNF, pins, true);
        self.gpio_write(port, MSK_OUT, pins, high);
        self.gpio_write(port, MSK_OE, pins, true);
    }

    fn level(&self, port: u32, pins: u32, high: bool) {
        self.gpio_write(port, MSK_OUT, pins, high);
    }

    /// DSI AVDD (LDO0, 1.2 V): voltage first, then the normal mode.
    pub fn dsi_avdd(&self, on: bool) -> Result<(), i64> {
        let v = self.i2c.read_reg(I2C_BUS, PMIC, LDO0_CFG)?;
        if on {
            self.i2c
                .write_reg(I2C_BUS, PMIC, LDO0_CFG, (v & 0xC0) | LDO0_1V2)?;
            time::sleep_ms(1);
            self.i2c
                .write_reg(I2C_BUS, PMIC, LDO0_CFG, LDO_MODE_NORMAL | LDO0_1V2)?;
        } else {
            self.i2c
                .write_reg(I2C_BUS, PMIC, LDO0_CFG, v & !LDO_MODE_MASK)?;
        }
        time::sleep_ms(1);
        Ok(())
    }

    /// Display clocks out of reset and running (hekate `display_init`).
    pub fn clocks_on(&self) {
        self.car.w32(RST_DEV_H_CLR, CLK_H_MIPI_CAL | CLK_H_DSI);
        self.car.w32(CLK_ENB_H_SET, CLK_H_MIPI_CAL | CLK_H_DSI);
        self.car.w32(RST_DEV_L_CLR, CLK_L_DISP1);
        self.car.w32(CLK_ENB_L_SET, CLK_L_DISP1);
        self.car.w32(CLK_ENB_X_SET, CLK_X_UART_FST_MIPI_CAL);
        self.car.w32(CLK_SOURCE_UART_FST_MIPI_CAL, SRC_DIV_6);
        self.car.w32(CLK_ENB_W_SET, CLK_W_DSIA_LP);
        self.car.w32(CLK_SOURCE_DSIA_LP, SRC_DIV_6);
    }

    /// Display clocks held in reset and gated (`display_end`, plus the two
    /// that only the display uses).
    pub fn clocks_off(&self) {
        self.car.w32(RST_DEV_H_SET, CLK_H_MIPI_CAL | CLK_H_DSI);
        self.car.w32(CLK_ENB_H_CLR, CLK_H_MIPI_CAL | CLK_H_DSI);
        self.car.w32(RST_DEV_L_SET, CLK_L_DISP1);
        self.car.w32(CLK_ENB_L_CLR, CLK_L_DISP1);
        self.car.w32(CLK_ENB_X_CLR, CLK_X_UART_FST_MIPI_CAL);
        self.car.w32(CLK_ENB_W_CLR, CLK_W_DSIA_LP);
    }

    /// PLLD running (hekate `clock_enable_plld`, T210): the pixel clock's
    /// source and the DSI bit clock. Out of IDDQ first. False: no lock.
    pub fn plld(&self, divp: u32, divn: u32, low_power: bool) -> bool {
        let sdm = if low_power { SDM_LP } else { SDM_HS };
        self.car.w32(CLK_SOURCE_DISP1, DISP1_FROM_PLLD_OUT0);
        self.car.w32(PLLD_MISC1, PLLD_MISC1_T210);
        self.car.w32(PLLD_MISC, PLLD_MISC_RUN | sdm);
        time::sleep_us(10);
        self.car
            .w32(PLLD_BASE, PLL_ENABLE | divp << 20 | divn << 11 | 1);
        let lock = self.car.wait(PLLD_BASE, PLL_LOCK, PLL_LOCK, 1000);
        time::sleep_us(2);
        lock
    }

    /// PLLD stopped and its analog part powered down.
    pub fn plld_off(&self) {
        self.car.modify(PLLD_BASE, PLL_ENABLE, 0);
        self.car.modify(PLLD_MISC, PLLD_MISC_RUN, PLLD_MISC_IDDQ);
    }

    /// LCD driver and WLED pins configured, AVDD channels on, WLED enabled
    /// with its PWM low, panel reset held low.
    pub fn rails_on(&self) {
        for pad in [PAD_NFC_EN, PAD_NFC_INT, PAD_LCD_BL_PWM, PAD_LCD_BL_EN] {
            self.pinmux.w32(pad, PAD_PULL_DOWN);
        }
        self.output(PORT_I, LCD_AVDD, true);
        time::sleep_ms(10);
        self.output(PORT_V, BL_PWM | BL_EN, false);
        self.level(PORT_V, BL_EN, true);
        self.pinmux.w32(PAD_LCD_RST, PAD_PULL_DOWN);
        self.output(PORT_V, LCD_RESET, false);
    }

    /// The backlight's PWM pad on channel 0, the pin driven by it
    /// (`display_backlight_pwm_init`).
    pub fn backlight_pad(&self) {
        let v = self.pinmux.r32(PAD_LCD_BL_PWM);
        self.pinmux
            .w32(PAD_LCD_BL_PWM, (v & !PAD_FUNC_MASK) | PAD_FUNC_PWM0);
        time::sleep_us(2);
        self.gpio_write(PORT_V, MSK_CNF, BL_PWM, false);
    }

    /// Panel reset line (high: out of reset).
    pub fn reset(&self, high: bool) {
        self.level(PORT_V, LCD_RESET, high);
    }

    /// AVDD channels off, the backlight pad parked on PWM0 tri-stated
    /// (`display_end`). WLED enable low: the driver is off.
    pub fn rails_off(&self) {
        self.level(PORT_I, LCD_AVDD, false);
        self.level(PORT_V, BL_EN, false);
        self.gpio_write(PORT_V, MSK_CNF, BL_PWM, false);
        self.pinmux
            .w32(PAD_LCD_BL_PWM, PAD_TRISTATE | PAD_PULL_DOWN | PAD_FUNC_PWM0);
    }
}

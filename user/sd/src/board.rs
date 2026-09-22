//! Switch SDMMC1 slot: card detect, pads, PMC I/O power, card and I/O rails.
//! Sequences from hekate bdk/storage/sdmmc_driver.c (`_sdmmc_config_sdmmc1`, `sdmmc1_disable_power`): hekate powers the slot off before the L4T jump.

use libnx::abi::err::ENODEV;
use libnx::abi::proto::power;
use libnx::drv::Mmio;
use libnx::ipc::{self, Msg};
use libnx::{ns, time};

pub const APB_MISC: u64 = 0x7000_0000;
pub const PINMUX: u64 = 0x7000_3000;
pub const GPIO: u64 = 0x6000_D000;
pub const PMC_PAGE: u64 = 0x7000_E000;

const GP_HIDREV: usize = 0x804;
const GP_SDMMC1_CLK_LPBK_CONTROL: usize = 0x8D4;
const GP_SDMMC1_PAD_CFGPADCTRL: usize = 0xA98;
const GP_VGPIO_GPIO_MUX_SEL: usize = 0xB74;
const HIDREV_MAJOR_T210B01: u32 = 2;

const PIN_SDMMC1_CLK: usize = 0x00;
const PIN_SDMMC1_CMD_DAT: [usize; 5] = [0x04, 0x08, 0x0C, 0x10, 0x14];
const PIN_DMIC3_CLK: usize = 0xB4;
const PIN_GPIO_PZ1: usize = 0x280;
const PULL_MASK: u32 = 3 << 2;
const PULL_DOWN: u32 = 1 << 2;
const PULL_UP: u32 = 2 << 2;
const INPUT_ENABLE: u32 = 1 << 6;
const SCHMT: u32 = 1 << 12;
const DRIVE_2X: u32 = 1 << 13;
/// Pinmux function 2 of these pins: GPIO-controlled.
const FUNC_GPIO: u32 = 2;

/// PMC page: registers from 0x400.
const PMC_NO_IOPOWER: usize = 0x400 + 0x44;
const PMC_PWR_DET_VAL: usize = 0x400 + 0xE4;
const PMC_SDMMC1: u32 = 1 << 12;

/// GPIO ports: E4 = card power switch, M0-M5 = SDMMC1 pads, Z1 = card
/// detect (low with a card in).
const PORT_E: usize = 4;
const PORT_M: usize = 12;
const PORT_Z: usize = 25;
const PADS: u32 = 0x3F;
const GPIO_CNF: usize = 0x80;
const GPIO_OE: usize = 0x90;
const GPIO_OUT: usize = 0xA0;
const GPIO_IN: usize = 0x30;

pub struct Board {
    misc: Mmio,
    pinmux: Mmio,
    gpio: Mmio,
    pmc: Mmio,
    power: Option<u32>,
    /// T210B01 (Mariko): other pad values, no slew override.
    pub mariko: bool,
}

impl Board {
    pub fn new(misc: Mmio, pinmux: Mmio, gpio: Mmio, pmc: Mmio) -> Self {
        let mariko = (misc.r32(GP_HIDREV) >> 4) & 0xF >= HIDREV_MAJOR_T210B01;
        let power = ns::wait_for_timeout("power", 2_000_000_000).ok();
        Board {
            misc,
            pinmux,
            gpio,
            pmc,
            power,
            mariko,
        }
    }

    /// Masked GPIO write (bits 15:8 select pins): no read-modify-write
    /// race with other GPIO users.
    fn gpio_set(&self, reg: usize, port: usize, pins: u32, value: u32) {
        let off = (port >> 2) * 0x100 + (port & 3) * 4;
        self.gpio.w32(reg + off, pins << 8 | (value & pins));
    }

    fn gpio_in(&self, port: usize, pin: u32) -> bool {
        let off = (port >> 2) * 0x100 + (port & 3) * 4;
        self.gpio.r32(GPIO_IN + off) & (1 << pin) != 0
    }

    pub fn card_present(&self) -> bool {
        self.pinmux
            .w32(PIN_GPIO_PZ1, INPUT_ENABLE | PULL_UP | FUNC_GPIO);
        self.misc.w32(GP_VGPIO_GPIO_MUX_SEL, 0);
        self.gpio_set(GPIO_CNF, PORT_Z, 1 << 1, 1 << 1);
        self.gpio_set(GPIO_OE, PORT_Z, 1 << 1, 0);
        time::sleep_us(100);
        !self.gpio_in(PORT_Z, 1)
    }

    /// Pads, controller I/O power, card power, then I/O rail at 3.3 V.
    pub fn power_on(&self) -> Result<(), i64> {
        // Deep loopback on the CLK pad: without it reads fail.
        self.misc.w32(GP_SDMMC1_CLK_LPBK_CONTROL, 1);
        let clk = self.pinmux.r32(PIN_SDMMC1_CLK) & !SCHMT;
        self.pinmux.w32(PIN_SDMMC1_CLK, clk);
        if clk != DRIVE_2X | INPUT_ENABLE | PULL_DOWN {
            let pull = if self.mariko { 0 } else { PULL_DOWN };
            self.pinmux
                .w32(PIN_SDMMC1_CLK, DRIVE_2X | INPUT_ENABLE | pull);
        }
        for pin in PIN_SDMMC1_CMD_DAT {
            self.pinmux.w32(pin, DRIVE_2X | INPUT_ENABLE | PULL_UP);
        }
        if self.mariko {
            for pin in [PIN_SDMMC1_CLK].into_iter().chain(PIN_SDMMC1_CMD_DAT) {
                self.pinmux.modify(pin, 0, SCHMT);
            }
        }

        self.pmc.modify(PMC_NO_IOPOWER, 0, PMC_SDMMC1);
        time::sleep_ms(1);
        self.pmc.modify(PMC_NO_IOPOWER, PMC_SDMMC1, 0);
        let _ = self.pmc.r32(PMC_NO_IOPOWER);

        // Card power switch; the card needs 3-10 ms to come up.
        self.pinmux.w32(PIN_DMIC3_CLK, PULL_DOWN | FUNC_GPIO);
        self.gpio_set(GPIO_CNF, PORT_E, 1 << 4, 1 << 4);
        self.gpio_set(GPIO_OUT, PORT_E, 1 << 4, 1 << 4);
        self.gpio_set(GPIO_OE, PORT_E, 1 << 4, 1 << 4);
        time::sleep_ms(10);

        // Pads told the I/O voltage is 3.3 V before the rail rises.
        self.pmc.modify(PMC_PWR_DET_VAL, 0, PMC_SDMMC1);
        let _ = self.pmc.r32(PMC_PWR_DET_VAL);
        self.io_rail(true)?;

        if !self.mariko {
            // Slew codes for a clean clock.
            self.misc
                .modify(GP_SDMMC1_PAD_CFGPADCTRL, 0xF000_0000, 0x5000_0000);
            let _ = self.misc.r32(GP_SDMMC1_PAD_CFGPADCTRL);
            time::sleep_ms(1);
        }
        Ok(())
    }

    /// Reverse order, pads discharged meanwhile; at least 1 ms off before
    /// a new power-up.
    pub fn power_off(&self) {
        self.pinmux.modify(PIN_SDMMC1_CLK, PULL_MASK, 0);
        self.pads_gpio(true);
        let _ = self.io_rail(false);
        time::sleep_ms(4);
        self.gpio_set(GPIO_OUT, PORT_E, 1 << 4, 0);
        time::sleep_ms(10);
        self.pmc.modify(PMC_NO_IOPOWER, 0, PMC_SDMMC1);
        self.pmc.modify(PMC_PWR_DET_VAL, 0, PMC_SDMMC1);
        let _ = self.pmc.r32(PMC_PWR_DET_VAL);
        self.pads_gpio(false);
        self.pinmux.modify(PIN_SDMMC1_CLK, 0, PULL_DOWN);
    }

    /// Discharge: pads as GPIO outputs driven high; else back to SDMMC1.
    fn pads_gpio(&self, discharge: bool) {
        let v = if discharge { PADS } else { 0 };
        self.gpio_set(GPIO_CNF, PORT_M, PADS, v);
        self.gpio_set(GPIO_OUT, PORT_M, PADS, v);
        self.gpio_set(GPIO_OE, PORT_M, PADS, v);
    }

    /// Standard 50-ohm drive when pad auto-calibration times out (3.3 V).
    pub fn pad_fallback(&self) {
        let code = if self.mariko { 0x808 } else { 0xC0C };
        self.misc
            .modify(GP_SDMMC1_PAD_CFGPADCTRL, 0x07F7_F000, code << 12);
        let _ = self.misc.r32(GP_SDMMC1_PAD_CFGPADCTRL);
    }

    fn io_rail(&self, on: bool) -> Result<(), i64> {
        let ep = self.power.ok_or(-ENODEV)?;
        let mut m = Msg::new(power::SD_POWER, 1, 0);
        m.mr[0] = on as u64;
        ipc::checked(ipc::call(ep, &m)?).map(|_| ())
    }
}

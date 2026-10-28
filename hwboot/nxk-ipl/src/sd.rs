//! The SD card for stage 1, through `nxk-sdcard` (the `sd` service's own
//! slot and card code): registers reached directly, waits polled, the
//! BPMP cache cleaned before a transfer and invalidated after (hekate
//! sdmmc_driver.c), the card's rail set on the PMIC directly.

use core::fmt;

use nxk_sdcard::board::{self, Board, Power};
use nxk_sdcard::{Card, Regs, Result, SDMMC1};

use crate::clock::{self, ClkRst};
use crate::hdr::*;
use crate::i2c::{self, I2C_5};
use crate::reg::{rd, wr};
use crate::soc::{self, MAINT_CLN_INV_WAY};
use crate::timer;
use crate::uart;

/// The driver's DMA page (descriptors, first block) in DRAM.
pub const DMA_PAGE: u32 = 0x8F00_0000;

pub struct Page(u32);

impl Regs for Page {
    fn r8(&self, off: usize) -> u8 {
        unsafe { ((self.0 as usize + off) as *const u8).read_volatile() }
    }
    fn w8(&self, off: usize, v: u8) {
        unsafe { ((self.0 as usize + off) as *mut u8).write_volatile(v) }
    }
    fn r16(&self, off: usize) -> u16 {
        unsafe { ((self.0 as usize + off) as *const u16).read_volatile() }
    }
    fn w16(&self, off: usize, v: u16) {
        unsafe { ((self.0 as usize + off) as *mut u16).write_volatile(v) }
    }
    fn r32(&self, off: usize) -> u32 {
        rd(self.0 + off as u32)
    }
    fn w32(&self, off: usize, v: u32) {
        wr(self.0 + off as u32, v)
    }
}

pub struct Host;

impl nxk_sdcard::Host for Host {
    fn now_us(&self) -> u64 {
        timer::now_us() as u64
    }

    fn sleep_us(&self, us: u64) {
        timer::usleep(us as u32);
    }

    fn dma_before(&self) {
        soc::bpmp_mmu_maintenance(17, false); // BPMP_MMU_MAINT_CLEAN_WAY
    }

    fn dma_after(&self) {
        soc::bpmp_mmu_maintenance(18, false); // BPMP_MMU_MAINT_INVALID_WAY
    }

    fn log(&self, args: fmt::Arguments) {
        uart::print(args);
    }
}

/// MAX77620 LDO2, as the `power` service sets it.
pub struct Rail;

const LDO2_CFG: u32 = 0x27;
const LDO2_3V3: u32 = (3_300_000 - 800_000) / 50_000;
const LDO_MODE_NORMAL: u32 = 3 << 6;

impl Power for Rail {
    fn sd_rail(&self, on: bool) -> Result<()> {
        let v = i2c::recv_byte(I2C_5, MAX77620_I2C_ADDR, LDO2_CFG) as u32;
        let set = |x: u32| i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, LDO2_CFG, x as u8).map_err(|_| -nxk_sdcard::EIO);
        if on {
            set(v & !0x3F | LDO2_3V3)?;
            timer::msleep(1);
            set(LDO_MODE_NORMAL | LDO2_3V3)?;
        } else {
            set(v & !0xC0)?;
        }
        timer::msleep(1);
        Ok(())
    }

    fn ldo2_1v8(&self) -> Result<()> {
        i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, LDO2_CFG, (0xC0 | 20) as u8).map_err(|_| -nxk_sdcard::EIO)
    }
}

/// SDMMC1 from PLLP 408 MHz / 8.5 = 48 MHz (`BASE_KHZ`), as `clk` does.
const SDMMC1_CLK: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_SDMMC1,
    index: CLK_L_SDMMC1,
    clk_src: 0,
    clk_div: 15,
};

pub type SdCard = Card<Page, Host>;
pub type SdBoard = Board<Page, Rail>;

/// Slot power cycle (239 ms off, hekate's), controller clock, card init,
/// a first block read (with the `sd` service's fallbacks).
pub fn open() -> Result<(SdCard, SdBoard)> {
    let board = Board::new(
        Page(board::APB_MISC as u32),
        Page(board::PINMUX as u32),
        Page(board::GPIO as u32),
        Page(board::PMC_PAGE as u32),
        Rail,
    );
    let mut card = Card::new(Page(SDMMC1 as u32), Host, DMA_PAGE as *mut u32, DMA_PAGE as u64);
    if !board.card_present(&Host) {
        uart::puts("[nxk-ipl] no card detected, probing anyway\n");
    }
    board.power_off(&Host);
    timer::msleep(239);
    board.power_on(&Host)?;
    clock::enable(&SDMMC1_CLK);
    card.init(&board, false, true)?;
    nxk_sdcard::recover(&mut card, &board)?;
    Ok((card, board))
}

/// SD clock and the slot off: the next stage starts from a cold slot.
pub fn close(card: &SdCard, board: &SdBoard) {
    card.stop();
    board.power_off(&Host);
    soc::bpmp_mmu_maintenance(MAINT_CLN_INV_WAY, false);
}

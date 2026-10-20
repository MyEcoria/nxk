//! A register file that touches nothing: the service runs the sequence on
//! it once before the real switch, so every page of code and data the
//! sequence uses is resident (a page fault inside the clock change would
//! stall it for milliseconds).

use crate::regs::*;
use crate::{Bank, Hw};

/// LPDDR4, one channel, PLL locked, clock change done, DLL as written.
#[derive(Default)]
pub struct Warm {
    dll: u32,
}

impl Hw for Warm {
    fn rd(&mut self, bank: Bank, off: u32) -> u32 {
        match (bank, off & 0xFFF, off >= 0x4000) {
            (Bank::Emc, EMC_CFG_DIG_DLL, _) => self.dll,
            (Bank::Emc, EMC_FBIO_CFG5, false) => 1,
            (Bank::Emc, EMC_INTSTATUS, false) => CLKCHANGE_COMPLETE_INT,
            (Bank::Emc, EMC_DIG_DLL_STATUS, false) => 0x28000,
            (Bank::Car, _, _)
                if off == CLK_RST_CONTROLLER_PLLM_BASE || off == CLK_RST_CONTROLLER_PLLMB_BASE =>
            {
                PLLM_LOCK
            }
            (Bank::Car, _, _) if off == CLK_RST_CONTROLLER_CLK_SOURCE_EMC => 4 << 29,
            _ => 0,
        }
    }

    fn wr(&mut self, bank: Bank, off: u32, val: u32) {
        if bank == Bank::Emc && off == EMC_CFG_DIG_DLL {
            self.dll = val;
        }
    }

    fn usleep(&mut self, _us: u32) {}
}

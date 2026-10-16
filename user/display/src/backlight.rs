//! The panel's backlight: PWM channel 0 (hekate bdk/display/di.c,
//! `display_backlight_brightness`: enable bit and an 8-bit duty in bits
//! 23:16). The loader left the block running, the pad and the backlight
//! supply set; the fan owns channel 1 of the same block.

use libnx::drv::{self, Mmio};

const PWM: u64 = 0x7000_A000;
const CSR_0: usize = 0;
const EN: u32 = 1 << 31;
/// Lowest brightness the slider gives: the screen stays readable.
pub const MIN_PCT: u8 = 10;

pub struct Backlight {
    pwm: Mmio,
}

/// PWM duty (0..=255) of a brightness: squared, as the eye sees it.
fn duty(pct: u8) -> u32 {
    let p = pct.min(100) as u32;
    (255 * p * p + 5000) / 10000
}

impl Backlight {
    /// `None` without the PWM page.
    pub fn open() -> Option<Self> {
        Mmio::map(drv::dev_slot(1), PWM, 0x1000)
            .ok()
            .map(|pwm| Self { pwm })
    }

    /// The brightness now, %: the one whose duty is closest (100 if the
    /// loader left the block off).
    pub fn percent(&self) -> u8 {
        let csr = self.pwm.r32(CSR_0);
        if csr & EN == 0 {
            return 100;
        }
        let d = (csr >> 16 & 0xFF) as i32;
        (MIN_PCT..=100)
            .min_by_key(|&p| (duty(p) as i32 - d).abs())
            .unwrap_or(100)
    }

    pub fn set(&self, pct: u8) {
        self.raw(duty(pct.clamp(MIN_PCT, 100)));
    }

    /// Dark: duty 0, the block stays enabled.
    pub fn off(&self) {
        self.raw(0);
    }

    fn raw(&self, duty: u32) {
        self.pwm.w32(CSR_0, EN | duty << 16);
    }
}

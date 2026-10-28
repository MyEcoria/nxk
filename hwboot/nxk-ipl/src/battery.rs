//! hekate's `_check_low_battery` (bootloader/main.c): the charger enabled,
//! and on a battery too low to boot a wait for the charger, the screen on
//! for 15 s at each change; not charging by then, the console powers off.
//! Vol+ and Vol- together boot anyway. The PMIC's low-battery shutdown,
//! off since hw_init, is armed again on the way out.

use crate::display;
use crate::hdr::*;
use crate::i2c::{self, I2C_1, I2C_5};
use crate::pmic;
use crate::soc::gpio_read;
use crate::timer::{bpmp_msleep, now_ms};

/// `bq24193_enable_charger`.
fn enable_charger() {
    let reg = i2c::recv_byte(I2C_1, BQ24193_I2C_ADDR, BQ24193_PORConfig) as u32;
    let reg = reg & !BQ24193_PORCONFIG_CHGCONFIG_MASK | BQ24193_PORCONFIG_CHGCONFIG_CHARGER_EN;
    let _ = i2c::send_byte(I2C_1, BQ24193_I2C_ADDR, BQ24193_PORConfig, reg as u8);
}

/// `BQ24193_ChargeStatus`: 0 not charging, 1 pre-charge, 2 fast, 3 done.
fn charge_status() -> u32 {
    (i2c::recv_byte(I2C_1, BQ24193_I2C_ADDR, BQ24193_Status) as u32 & BQ24193_STATUS_CHRG_MASK) >> 4
}

/// `MAX17050_AvgVCELL` in mV (0.625 mV units above bit 3).
fn battery_mv() -> u32 {
    let mut b = [0u8; 2];
    if i2c::recv_buf_small(&mut b, I2C_1, MAXIM17050_I2C_ADDR, MAX17050_AvgVCELL).is_err() {
        return 0;
    }
    (u16::from_le_bytes(b) as u32 >> 3) * 625 / 1000
}

fn enough_mv(charging: u32) -> u32 {
    if charging != 0 {
        3300
    } else {
        3100
    }
}

/// `btn_read`'s Vol+ and Vol- (GPIO X6, X7, low when pressed).
fn both_volume_keys() -> bool {
    !gpio_read(GPIO_PORT_X, GPIO_PIN_6) && !gpio_read(GPIO_PORT_X, GPIO_PIN_7)
}

/// `btn_read`'s power key (the PMIC's EN0).
fn power_key() -> bool {
    i2c::recv_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_ONOFFSTAT) as u32 & MAX77620_ONOFFSTAT_EN0 != 0
}

/// An empty battery, its charging mark lit or not (hekate's icons, drawn
/// as rectangles on the dark theme).
fn draw(charging: bool) {
    let (x, y, w, h) = (560, 300, 140, 80);
    display::rect(0, 0, 1280, 720, 0x1C1C1C);
    display::rect(x, y, w, h, 0xC8C8C8);
    display::rect(x + 6, y + 6, w - 12, h - 12, 0x1C1C1C);
    display::rect(x + w, y + 24, 10, h - 48, 0xC8C8C8);
    display::rect(x + 12, y + 12, 14, h - 24, 0xE04040);
    display::rect(x + 40, y + h + 30, w - 80, 12, if charging { 0x3BC3E3 } else { 0x1C1C1C });
}

/// `power_set_state(POWER_OFF_RESET)`: no wake-up, soft reset of the PMIC.
fn power_off() -> ! {
    display::brightness(0, 1000);
    let reg = i2c::recv_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_ONOFFCNFG2) as u32;
    let reg = reg & !(MAX77620_ONOFFCNFG2_SFT_RST_WK | MAX77620_ONOFFCNFG2_WK_ALARM1 | MAX77620_ONOFFCNFG2_WK_ALARM2);
    let _ = i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_ONOFFCNFG2, reg as u8);
    let _ = i2c::send_byte(I2C_5, MAX77620_I2C_ADDR, MAX77620_REG_ONOFFCNFG1, MAX77620_ONOFFCNFG1_SFT_RST as u8);
    crate::halt()
}

/// `_check_low_battery`. True if the panel was lit for it (then left at
/// brightness 0, ready for the boot screen).
pub fn check() -> bool {
    enable_charger();
    let charging = charge_status();
    let mv = battery_mv();
    crate::report::set(crate::report::BATTERY_MV, mv);
    crate::println!("[nxk-ipl] battery {} mV, charger status {}", mv, charging);
    let mut lit = false;
    if mv != 0 && mv <= enough_mv(charging) {
        crate::println!("[nxk-ipl] battery low ({} mV): waiting for the charger", mv);
        lit = wait(charging);
    }
    pmic::low_battery_monitor(true);
    lit
}

/// The loop, from the status read first (`shown` differs from it at the
/// start, so the screen comes on at once).
fn wait(first: u32) -> bool {
    let mut shown = (first == 0) as u32;
    let (mut screen_on, mut lit) = (false, false);
    let mut off_at = 0;
    loop {
        bpmp_msleep(250);
        let charging = charge_status();
        if battery_mv() > enough_mv(charging) {
            break;
        }
        if screen_on && shown != charging {
            draw(charging != 0);
        }
        if screen_on && off_at < now_ms() {
            if charging == 0 {
                pmic::low_battery_monitor(true);
                power_off();
            }
            display::brightness(0, 1000);
            screen_on = false;
        }
        if shown != charging || power_key() {
            if !screen_on {
                if !lit {
                    display::init();
                    display::window_a();
                    display::backlight_pwm_init();
                    lit = true;
                }
                draw(charging != 0);
                display::brightness(100, 1000);
                screen_on = true;
            }
            off_at = now_ms() + 15_000;
        }
        if both_volume_keys() {
            break;
        }
        shown = charging;
    }
    if screen_on {
        display::brightness(0, 1000);
    }
    lit
}

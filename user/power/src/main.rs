//! power: board power, RTC wall-clock sync, TMP451 temperatures, SD rail.
//! RTC is read-only; its BCD/12-24h mode is honored as left.

#![no_std]
#![no_main]

use libnx::abi::err::EINVAL;
use libnx::abi::proto::power as proto;
use libnx::drv;
use libnx::i2c::I2c;
use libnx::ipc::Msg;
use libnx::{println, sys, time};

const PMIC: u8 = 0x3C;
const RTC: u8 = 0x68;
const TMP451: u8 = 0x4C;

/// MAX77620 LDO2 (SD card I/O): voltage in bits 5:0 (0.8 V + 50 mV steps),
/// power mode in bits 7:6 (3 = normal), as hekate bdk/power/max7762x.c.
const LDO2_CFG: u8 = 0x27;
const LDO2_3V3: u8 = ((3_300_000 - 800_000) / 50_000) as u8;
const LDO_MODE_NORMAL: u8 = 3 << 6;
/// MAX77620 LDO1 (XUSB and PCIe rail, 25 mV steps): its documented
/// default, 1.05 V. The Wi-Fi chip hangs off the PCIe UPHY lanes.
const LDO1_CFG: u8 = 0x25;
const LDO1_1V05: u8 = ((1_050_000 - 800_000) / 25_000) as u8;

/// Only 3.3 V: 1.8 V needs the card's CMD11 handshake first.
fn sd_power(i2c: &I2c, on: bool) -> Result<(), i64> {
    let v = i2c.read_reg(5, PMIC, LDO2_CFG)?;
    if on {
        i2c.write_reg(5, PMIC, LDO2_CFG, (v & !0x3F) | LDO2_3V3)?;
        time::sleep_ms(1);
        i2c.write_reg(5, PMIC, LDO2_CFG, LDO_MODE_NORMAL | LDO2_3V3)?;
    } else {
        i2c.write_reg(5, PMIC, LDO2_CFG, v & !0xC0)?;
    }
    time::sleep_ms(1);
    Ok(())
}

/// PCIe/XUSB rail at its default voltage: nothing else on the board
/// shares LDO1, and XUSB is happy at the same 1.05 V.
fn pcie_power(i2c: &I2c, on: bool) -> Result<(), i64> {
    let v = i2c.read_reg(5, PMIC, LDO1_CFG)?;
    if on {
        i2c.write_reg(5, PMIC, LDO1_CFG, LDO_MODE_NORMAL | LDO1_1V05)?;
    } else {
        i2c.write_reg(5, PMIC, LDO1_CFG, v & !0xC0)?;
    }
    time::sleep_ms(1);
    Ok(())
}

fn bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0xF)
}

/// Days since 1 January 1970 (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RTC time, in seconds since 1970 (UTC).
fn rtc_seconds(i2c: &I2c) -> Result<i64, i64> {
    // Copy current time into readable registers (RBUDR).
    i2c.write_reg(5, RTC, 0x04, 1 << 4)?;
    time::sleep_ms(16);
    let ctl = i2c.read_reg(5, RTC, 0x03)?;
    let (is_bcd, h24) = (ctl & 1 != 0, ctl & 2 != 0);
    let conv = |v: u8| if is_bcd { bcd(v) } else { v };
    let mut r = [0u8; 7];
    i2c.transfer(5, RTC, &[0x07], &mut r)?;
    let (sec, min) = (conv(r[0] & 0x7F), conv(r[1] & 0x7F));
    let mut hour = conv(r[2] & 0x3F);
    if !h24 {
        hour %= 12;
        if r[2] & (1 << 6) != 0 {
            hour += 12;
        }
    }
    let month = conv(r[4] & 0x1F).max(1);
    let year = 2000 + conv(r[5] & 0x7F) as i64;
    let day = conv(r[6] & 0x3F).max(1);
    let days = days_from_civil(year, month as u32, day as u32);
    Ok(days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64)
}

fn sync_time(i2c: &I2c, sysctl: u32) -> Result<i64, i64> {
    let s = rtc_seconds(i2c)?;
    sys::clock_set_realtime(sysctl, s as u64 * 1_000_000_000)?;
    Ok(s)
}

/// Temperature in millidegrees (high byte + 4 fraction bits).
fn temperature(i2c: &I2c, hi: u8, lo: u8) -> Result<i64, i64> {
    let h = i2c.read_reg(1, TMP451, hi)? as i64;
    let l = (i2c.read_reg(1, TMP451, lo)? >> 4) as i64;
    Ok(h * 1000 + l * 1000 / 16)
}

libnx::entry!(main);

fn main() -> i32 {
    let sysctl = drv::dev_slot(0);
    let i2c = I2c::connect().expect("power: i2c service");
    match sync_time(&i2c, sysctl) {
        Ok(s) => {
            let d = s / 86_400;
            println!(
                "[power] RTC time: day {} since 1970, {:02}:{:02}:{:02} UTC",
                d,
                s % 86_400 / 3600,
                s % 3600 / 60,
                s % 60
            );
        }
        Err(e) => println!("[power] RTC unreadable ({})", libnx::abi::err::name(e)),
    }
    let ep = drv::publish("power").expect("power: publish");
    drv::serve(ep, |rx| {
        Some(match rx.msg.label() {
            proto::OFF => {
                println!("[power] powering off");
                let r = i2c
                    .read_reg(5, PMIC, 0x41)
                    .and_then(|v| i2c.write_reg(5, PMIC, 0x41, v | 1 << 1));
                match r {
                    Ok(()) => {
                        time::sleep_ms(1000);
                        Msg::error(-libnx::abi::err::EIO) // still here: failure
                    }
                    Err(e) => Msg::error(e),
                }
            }
            proto::REBOOT => match sys::system_power(sysctl, libnx::abi::power::REBOOT) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::TEMPERATURE => {
                match (temperature(&i2c, 0x00, 0x15), temperature(&i2c, 0x01, 0x10)) {
                    (Ok(local), Ok(remote)) => {
                        let mut m = Msg::new(0, 2, 0);
                        m.mr[0] = local as u64;
                        m.mr[1] = remote as u64;
                        m
                    }
                    (Err(e), _) | (_, Err(e)) => Msg::error(e),
                }
            }
            proto::SYNC_TIME => match sync_time(&i2c, sysctl) {
                Ok(_) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::SD_POWER => match sd_power(&i2c, rx.msg.mr[0] != 0) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::PCIE_POWER => match pcie_power(&i2c, rx.msg.mr[0] != 0) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            _ => Msg::error(-EINVAL),
        })
    })
}

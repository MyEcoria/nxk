//! power: board power, wall clock (RTC plus a network-time offset,
//! `ntp`), TMP451 temperatures, SD, PCIe and touchscreen rails, the
//! charger's input status, the fuel gauge.
//! RTC is read-only; its BCD/12-24h mode is honored as left.
//! `MEASURE` cuts the charger's input to read the battery's own load.
//! The TMP451 is started again as hekate's `tmp451_init` does.

#![no_std]
#![no_main]

extern crate alloc;

mod measure;
mod ntp;

use libnx::abi::err::EINVAL;
use libnx::abi::proto::power as proto;
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::Msg;
use libnx::{println, sys, time};

const PMIC: u8 = 0x3C;
const RTC: u8 = 0x68;
const TMP451: u8 = 0x4C;
/// Battery charger on I2C1 (hekate bdk/power/bq24193.h): status
/// register, power good (input present).
const BQ24193: u8 = 0x6B;
const BQ24193_STATUS: u8 = 0x08;
const BQ24193_PG: u8 = 1 << 2;
const BQ24193_CHRG_SHIFT: u8 = 4;
/// Battery fuel gauge on I2C1 (hekate bdk/power/max17050.c). The board's
/// sense resistor is 5 mOhm and the gauge's current gain 2: LSB 156.25 uA,
/// 0.5 mAh.
const MAX17050: u8 = 0x36;
const MAX_REPCAP: u8 = 0x05;
const MAX_REPSOC: u8 = 0x06;
const MAX_AGE: u8 = 0x07;
const MAX_TEMP: u8 = 0x08;
const MAX_VCELL: u8 = 0x09;
const MAX_CURRENT: u8 = 0x0A;
const MAX_AVGCURRENT: u8 = 0x0B;
const MAX_FULLCAP: u8 = 0x10;
const MAX_CYCLES: u8 = 0x17;
const MAX_DESIGNCAP: u8 = 0x18;
/// TMP451 registers (hekate bdk/thermal/tmp451.h): configuration and
/// conversion rate are written at 0x09, 0x0A, read back at 0x03, 0x04.
const TMP451_CONFIG: u8 = 0x09;
const TMP451_CONFIG_READ: u8 = 0x03;
const TMP451_RATE: u8 = 0x0A;
const TMP451_OFFSET: [u8; 2] = [0x11, 0x12];
/// ALERT masked, converting (bit 6 = shut down), 0-127 C range.
const TMP451_RUN: u8 = 0x80;
const TMP451_SHUTDOWN: u8 = 1 << 6;
/// Conversion rate codes: 31 ms, 4 s.
const RATE_31MS: u8 = 9;
const RATE_4S: u8 = 2;

const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;

/// MAX77620 LDO2 (SD card I/O): voltage in bits 5:0 (0.8 V + 50 mV steps),
/// power mode in bits 7:6 (3 = normal), as hekate bdk/power/max7762x.c.
const LDO2_CFG: u8 = 0x27;
const LDO2_3V3: u8 = ((3_300_000 - 800_000) / 50_000) as u8;
const LDO_MODE_NORMAL: u8 = 3 << 6;
/// MAX77620 LDO1 (XUSB and PCIe rail, 25 mV steps): its documented
/// default, 1.05 V. The Wi-Fi chip hangs off the PCIe UPHY lanes.
const LDO1_CFG: u8 = 0x25;
const LDO1_1V05: u8 = ((1_050_000 - 800_000) / 25_000) as u8;
/// MAX77620 LDO6 (touchscreen, ambient light sensor): 2.9 V, as hekate
/// `touch_power_on`. Nyx turns it off before chaining to nxk.
const LDO6_CFG: u8 = 0x2F;
const LDO6_2V9: u8 = ((2_900_000 - 800_000) / 50_000) as u8;

/// A 50 mV-step LDO: voltage, then normal mode (hekate
/// `max7762x_regulator_set_voltage` then `_enable`), or off.
fn ldo(i2c: &I2c, cfg: u8, volt: u8, on: bool) -> Result<(), i64> {
    let v = i2c.read_reg(5, PMIC, cfg)?;
    if on {
        i2c.write_reg(5, PMIC, cfg, (v & !0x3F) | volt)?;
        time::sleep_ms(1);
        i2c.write_reg(5, PMIC, cfg, LDO_MODE_NORMAL | volt)?;
    } else {
        i2c.write_reg(5, PMIC, cfg, v & !0xC0)?;
    }
    time::sleep_ms(1);
    Ok(())
}

/// Only 3.3 V: 1.8 V needs the card's CMD11 handshake first.
fn sd_power(i2c: &I2c, on: bool) -> Result<(), i64> {
    ldo(i2c, LDO2_CFG, LDO2_3V3, on)
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
pub(crate) fn rtc_seconds(i2c: &I2c) -> Result<i64, i64> {
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

/// The kernel wall clock from the RTC and the network-time offset.
pub(crate) fn sync_time(i2c: &I2c, sysctl: u32) -> Result<i64, i64> {
    let s = ntp::real_seconds(rtc_seconds(i2c)?);
    sys::clock_set_realtime(sysctl, s as u64 * 1_000_000_000)?;
    Ok(s)
}

/// hekate `tmp451_init`: Nyx shuts the sensor down (`tmp451_end`) before
/// chaining to nxk, freezing its last reading. Mariko's SoC diode reads
/// 12.5 C high. Returns the configuration found.
fn sensor_start(i2c: &I2c, mariko: bool) -> Result<u8, i64> {
    let was = i2c.read_reg(1, TMP451, TMP451_CONFIG_READ)?;
    i2c.write_reg(1, TMP451, TMP451_CONFIG, TMP451_RUN)?;
    let offset = if mariko { [0xF3, 0x80] } else { [0, 0] };
    for (reg, v) in TMP451_OFFSET.into_iter().zip(offset) {
        i2c.write_reg(1, TMP451, reg, v)?;
    }
    // A fresh reading at once, then one every 4 s.
    i2c.write_reg(1, TMP451, TMP451_RATE, RATE_31MS)?;
    time::sleep_ms(70);
    i2c.write_reg(1, TMP451, TMP451_RATE, RATE_4S)?;
    Ok(was)
}

/// Temperature in millidegrees (high byte + 4 fraction bits).
fn temperature(i2c: &I2c, hi: u8, lo: u8) -> Result<i64, i64> {
    let h = i2c.read_reg(1, TMP451, hi)? as i64;
    let l = (i2c.read_reg(1, TMP451, lo)? >> 4) as i64;
    Ok(h * 1000 + l * 1000 / 16)
}

/// A 16-bit little-endian fuel gauge register.
fn gauge(i2c: &I2c, reg: u8) -> Result<u16, i64> {
    let mut r = [0u8; 2];
    i2c.transfer(1, MAX17050, &[reg], &mut r)?;
    Ok(u16::from_le_bytes(r))
}

/// Gauge current register, in uA.
fn gauge_current(i2c: &I2c, reg: u8) -> Result<u64, i64> {
    Ok((gauge(i2c, reg)? as i16 as i64 * 15_625 / 100) as u64)
}

/// Gauge charge register (0.5 mAh units), in mAh.
fn gauge_mah(i2c: &I2c, reg: u8) -> Result<u64, i64> {
    Ok(gauge(i2c, reg)? as u64 / 2)
}

/// Cell voltage, in mV (78.125 uV steps in bits 15:3).
fn cell_mv(i2c: &I2c) -> Result<u64, i64> {
    Ok((gauge(i2c, MAX_VCELL)? >> 3) as u64 * 625 / 1000)
}

fn battery(i2c: &I2c) -> Result<Msg, i64> {
    let soc = gauge(i2c, MAX_REPSOC)? as u64;
    let mv = cell_mv(i2c)?;
    let avg = gauge_current(i2c, MAX_AVGCURRENT)?;
    let temp = (gauge(i2c, MAX_TEMP)? as i16 as i64 * 1000 / 256) as u64;
    let caps = gauge_mah(i2c, MAX_REPCAP)? | gauge_mah(i2c, MAX_FULLCAP)? << 32;
    let chg = i2c.read_reg(1, BQ24193, BQ24193_STATUS)? >> BQ24193_CHRG_SHIFT & 3;
    Ok(Msg::from_words(0, &[soc, mv, avg, temp, caps, chg as u64]))
}

fn battery_health(i2c: &I2c) -> Result<Msg, i64> {
    let age = (gauge(i2c, MAX_AGE)? >> 8) as u64;
    let cycles = gauge(i2c, MAX_CYCLES)? as u64;
    let design = gauge_mah(i2c, MAX_DESIGNCAP)?;
    let now = gauge_current(i2c, MAX_CURRENT)?;
    Ok(Msg::from_words(0, &[age, cycles, design, now]))
}

libnx::entry!(main);

/// Cores on or off; retries while one is still on its way off.
fn cores(sysctl: u32, mask: u64) -> Result<u64, i64> {
    for _ in 0..100 {
        match sys::set_cores(sysctl, mask) {
            Err(e) if e == -libnx::abi::err::EAGAIN => time::sleep_ms(1),
            Err(e) => return Err(e),
            Ok(()) => return Ok(u64::from(mask.count_ones())),
        }
    }
    Err(-libnx::abi::err::EAGAIN)
}

fn main() -> i32 {
    let mariko = Mmio::map(drv::dev_slot(0), APB_MISC, 0x1000)
        .is_ok_and(|m| (m.r32(GP_HIDREV) >> 4) & 0xF >= 2);
    let sysctl = drv::dev_slot(1);
    let i2c = I2c::connect().expect("power: i2c service");
    match sensor_start(&i2c, mariko) {
        Ok(was) if was & TMP451_SHUTDOWN != 0 => {
            println!("[power] temperature sensor was shut down, started")
        }
        Ok(_) => {}
        Err(e) => println!(
            "[power] temperature sensor missing ({})",
            libnx::abi::err::name(e)
        ),
    }
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
    // A previous instance may have died mid-measurement.
    if let Ok(true) = measure::clear_hiz(&i2c) {
        println!("[power] charger input was cut, restored");
    }
    if libnx::thread::spawn(move || ntp::run(sysctl)).is_err() {
        println!("[power] no network time thread");
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
            proto::TOUCH_POWER => match ldo(&i2c, LDO6_CFG, LDO6_2V9, rx.msg.mr[0] != 0) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::SUPPLY => match i2c.read_reg(1, BQ24193, BQ24193_STATUS) {
                Ok(st) => {
                    let src = match st & BQ24193_PG {
                        0 => proto::SUPPLY_BATTERY,
                        _ => proto::SUPPLY_EXTERNAL,
                    };
                    Msg::from_words(0, &[src, st as u64])
                }
                Err(e) => Msg::error(e),
            },
            proto::BATTERY => battery(&i2c).unwrap_or_else(Msg::error),
            proto::BATTERY_HEALTH => battery_health(&i2c).unwrap_or_else(Msg::error),
            proto::MEASURE => measure::start(rx.msg.mr[0], rx.msg.mr[1]),
            proto::MEASURE_RESULT => measure::result(),
            proto::SUSPEND_TEST => match sys::suspend_test(sysctl, rx.msg.mr[0]) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::CORES => match cores(sysctl, rx.msg.mr[0]) {
                Ok(n) => Msg::from_words(0, &[n]),
                Err(e) => Msg::error(e),
            },
            _ => Msg::error(-EINVAL),
        })
    })
}

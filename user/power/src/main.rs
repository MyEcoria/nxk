//! power : alimentation, heure et capteurs de la carte.
//!
//! - Heure murale : lue au demarrage dans la RTC du PMIC MAX77620 (I2C5,
//!   0x68), puis confiee au noyau (`clock_set`), qui la tient a jour.
//! - Extinction : bit PWR_OFF du PMIC (I2C5, 0x3C) ; redemarrage : noyau.
//! - Temperatures : capteur TMP451 (I2C1, 0x4C), puce et boitier.
//!
//! La RTC n'est que lue : son mode (BCD, 12/24 h) est respecte tel que le
//! systeme d'origine l'a regle.

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

fn bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0xF)
}

/// Jours depuis le 1er janvier 1970 (algorithme de H. Hinnant).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Heure de la RTC, en secondes depuis 1970 (UTC).
fn rtc_seconds(i2c: &I2c) -> Result<i64, i64> {
    // Copie de l'heure courante dans les registres lisibles (RBUDR).
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

/// Temperature en milliemes de degre (octet haut + 4 bits de fraction).
fn temperature(i2c: &I2c, hi: u8, lo: u8) -> Result<i64, i64> {
    let h = i2c.read_reg(1, TMP451, hi)? as i64;
    let l = (i2c.read_reg(1, TMP451, lo)? >> 4) as i64;
    Ok(h * 1000 + l * 1000 / 16)
}

libnx::entry!(main);

fn main() -> i32 {
    let sysctl = drv::dev_slot(0);
    let i2c = I2c::connect().expect("power : service i2c");
    match sync_time(&i2c, sysctl) {
        Ok(s) => {
            let d = s / 86_400;
            println!("[power] heure lue dans la RTC : jour {} depuis 1970, {:02}:{:02}:{:02} UTC", d, s % 86_400 / 3600, s % 3600 / 60, s % 60);
        }
        Err(e) => println!("[power] RTC illisible ({})", libnx::abi::err::name(e)),
    }
    let ep = drv::publish("power").expect("power : publication");
    drv::serve(ep, |rx| {
        Some(match rx.msg.label() {
            proto::OFF => {
                println!("[power] extinction");
                let r = i2c.read_reg(5, PMIC, 0x41).and_then(|v| i2c.write_reg(5, PMIC, 0x41, v | 1 << 1));
                match r {
                    Ok(()) => {
                        time::sleep_ms(1000);
                        Msg::error(-libnx::abi::err::EIO) // toujours la : echec
                    }
                    Err(e) => Msg::error(e),
                }
            }
            proto::REBOOT => match sys::system_power(sysctl, libnx::abi::power::REBOOT) {
                Ok(()) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            proto::TEMPERATURE => match (temperature(&i2c, 0x00, 0x15), temperature(&i2c, 0x01, 0x10)) {
                (Ok(local), Ok(remote)) => {
                    let mut m = Msg::new(0, 2, 0);
                    m.mr[0] = local as u64;
                    m.mr[1] = remote as u64;
                    m
                }
                (Err(e), _) | (_, Err(e)) => Msg::error(e),
            },
            proto::SYNC_TIME => match sync_time(&i2c, sysctl) {
                Ok(_) => Msg::new(0, 0, 0),
                Err(e) => Msg::error(e),
            },
            _ => Msg::error(-EINVAL),
        })
    })
}

//! clk : horloges et resets des peripheriques (controleur CAR).
//!
//! Le CAR regle aussi les coeurs et la memoire : il n'est confie qu'a ce
//! service, qui n'expose aux pilotes que l'activation de leur propre
//! peripherique. Sequence et valeurs reprises de hekate (bdk/soc/clock.c).

#![no_std]
#![no_main]

use libnx::abi::err::EINVAL;
use libnx::abi::proto::clk as proto;
use libnx::drv::{self, Mmio};
use libnx::ipc::Msg;
use libnx::{println, time};

const CAR: u64 = 0x6000_6000;
/// Registre CLR d'une paire SET/CLR.
const CLR: usize = 4;

/// (registre RST_DEV_x_SET, registre CLK_ENB_x_SET, source, bit, valeur de source)
struct Clock {
    reset: usize,
    enable: usize,
    source: usize,
    bit: u32,
    src: u32,
}

/// Source 6 (oscillateur 19,2 MHz) : diviseur 4 -> 4,8 MHz (100 kHz), 1 ->
/// 19,2 MHz (400 kHz). SDMMC : PLLP 408 MHz / 8,5 = 48 MHz, le controleur
/// divise ensuite lui-meme.
fn table(dev: u64) -> Option<Clock> {
    let i2c = |reset, enable, source, bit, div: u32| Clock { reset, enable, source, bit, src: (6 << 29) | (div - 1) };
    Some(match dev {
        proto::I2C1 => i2c(0x300, 0x320, 0x124, 12, 4),
        proto::I2C2 => i2c(0x308, 0x328, 0x198, 22, 1),
        proto::I2C3 => i2c(0x310, 0x330, 0x1B8, 3, 1),
        proto::I2C4 => i2c(0x430, 0x440, 0x3C4, 7, 4),
        proto::I2C5 => i2c(0x308, 0x328, 0x128, 15, 1),
        proto::I2C6 => i2c(0x290, 0x284, 0x65C, 6, 4),
        proto::SDMMC1 => Clock { reset: 0x300, enable: 0x320, source: 0x150, bit: 14, src: 15 },
        proto::SDMMC4 => Clock { reset: 0x300, enable: 0x320, source: 0x164, bit: 15, src: 15 },
        _ => return None,
    })
}

fn enable(car: &Mmio, c: &Clock) {
    let m = 1u32 << c.bit;
    car.w32(c.reset, m); // en reset
    car.w32(c.enable + CLR, m); // horloge coupee
    car.w32(c.source, c.src);
    car.w32(c.enable, m); // horloge active
    time::sleep_us(2);
    car.w32(c.reset + CLR, m); // sortie du reset
    let _ = car.r32(c.reset);
}

fn disable(car: &Mmio, c: &Clock) {
    let m = 1u32 << c.bit;
    car.w32(c.reset, m);
    car.w32(c.enable + CLR, m);
}

libnx::entry!(main);

fn main() -> i32 {
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("clk : registres CAR");
    let ep = drv::publish("clk").expect("clk : publication");
    println!("[clk] service des horloges pret");
    drv::serve(ep, |rx| {
        let r = match (rx.msg.label(), table(rx.msg.mr[0])) {
            (proto::ENABLE, Some(c)) => {
                enable(&car, &c);
                0
            }
            (proto::DISABLE, Some(c)) => {
                disable(&car, &c);
                0
            }
            _ => EINVAL,
        };
        Some(Msg::new(r as u32, 0, 0))
    })
}

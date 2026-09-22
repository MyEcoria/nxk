//! clk: device clocks/resets (CAR); values from hekate (bdk/soc/clock.c).
//! Also reports PMC power-partition status (STATUS).

#![no_std]
#![no_main]

use libnx::abi::err::{EINVAL, EIO, ENODEV};
use libnx::abi::proto::clk as proto;
use libnx::drv::{self, Mmio};
use libnx::ipc::Msg;
use libnx::{println, time};

const CAR: u64 = 0x6000_6000;
/// CLR register of a SET/CLR pair.
const CLR: usize = 4;

/// PMC page (registers from 0x400, hekate bdk/soc/pmc_t210.h).
const PMC_PAGE: u64 = 0x7000_E000;
const PMC_CLAMP_STATUS: usize = 0x400 + 0x2C;
const PMC_PWRGATE_STATUS: usize = 0x400 + 0x38;
/// XUSB partitions (hekate bdk/soc/pmc.h, POWER_RAIL_XUSBx): A = SuperSpeed,
/// C = host.
const PART_XUSBA: u32 = 1 << 20;
const PART_XUSBC: u32 = 1 << 22;

/// CAR bank (L, H, U, V, W, X): state in RST_DEVICES_x and CLK_OUT_ENB_x,
/// driven via RST_DEV_x_SET and CLK_ENB_x_SET (+ CLR).
struct Bank {
    rst: usize,
    enb: usize,
    rst_set: usize,
    enb_set: usize,
}

const L: Bank = Bank {
    rst: 0x004,
    enb: 0x010,
    rst_set: 0x300,
    enb_set: 0x320,
};
const H: Bank = Bank {
    rst: 0x008,
    enb: 0x014,
    rst_set: 0x308,
    enb_set: 0x328,
};
const U: Bank = Bank {
    rst: 0x00C,
    enb: 0x018,
    rst_set: 0x310,
    enb_set: 0x330,
};
const V: Bank = Bank {
    rst: 0x358,
    enb: 0x360,
    rst_set: 0x430,
    enb_set: 0x440,
};
const W: Bank = Bank {
    rst: 0x35C,
    enb: 0x364,
    rst_set: 0x438,
    enb_set: 0x448,
};
const X: Bank = Bank {
    rst: 0x28C,
    enb: 0x280,
    rst_set: 0x290,
    enb_set: 0x284,
};

/// One clock: bank, bit, source register (0: none) with bits to keep
/// and the value to set.
struct Clock {
    bank: &'static Bank,
    bit: u32,
    source: usize,
    keep: u32,
    src: u32,
}

/// One device: its clocks in enable order plus PMC partitions that must
/// be powered (0: always-on domain).
struct Device {
    clocks: &'static [Clock],
    parts: u32,
}

/// Source 6 (19.2 MHz oscillator): divider 4 -> 4.8 MHz (100 kHz),
/// 1 -> 19.2 MHz (400 kHz).
const fn i2c(bank: &'static Bank, bit: u32, source: usize, div: u32) -> Clock {
    Clock {
        bank,
        bit,
        source,
        keep: 0,
        src: (6 << 29) | (div - 1),
    }
}

const I2C1: [Clock; 1] = [i2c(&L, 12, 0x124, 4)];
const I2C2: [Clock; 1] = [i2c(&H, 22, 0x198, 1)];
const I2C3: [Clock; 1] = [i2c(&U, 3, 0x1B8, 1)];
const I2C4: [Clock; 1] = [i2c(&V, 7, 0x3C4, 4)];
const I2C5: [Clock; 1] = [i2c(&H, 15, 0x128, 1)];
const I2C6: [Clock; 1] = [i2c(&X, 6, 0x65C, 4)];
/// SDMMC: PLLP 408 MHz / 8.5 = 48 MHz, the controller divides further itself.
const SDMMC1: [Clock; 1] = [Clock {
    bank: &L,
    bit: 14,
    source: 0x150,
    keep: 0,
    src: 15,
}];
const SDMMC4: [Clock; 1] = [Clock {
    bank: &L,
    bit: 15,
    source: 0x164,
    keep: 0,
    src: 15,
}];
/// xHCI host gates (hekate device-mode order); SuperSpeed source back on
/// always-on CLK_M, host source untouched.
const XUSB_HOST: [Clock; 3] = [
    Clock {
        bank: &W,
        bit: 15,
        source: 0,
        keep: 0,
        src: 0,
    },
    Clock {
        bank: &W,
        bit: 28,
        source: 0x610,
        keep: 0x1FFF_FF00,
        src: 0,
    },
    Clock {
        bank: &U,
        bit: 25,
        source: 0,
        keep: 0,
        src: 0,
    },
];

fn table(dev: u64) -> Option<Device> {
    let (clocks, parts): (&'static [Clock], u32) = match dev {
        proto::I2C1 => (&I2C1, 0),
        proto::I2C2 => (&I2C2, 0),
        proto::I2C3 => (&I2C3, 0),
        proto::I2C4 => (&I2C4, 0),
        proto::I2C5 => (&I2C5, 0),
        proto::I2C6 => (&I2C6, 0),
        proto::SDMMC1 => (&SDMMC1, 0),
        proto::SDMMC4 => (&SDMMC4, 0),
        proto::XUSB_HOST => (&XUSB_HOST, PART_XUSBA | PART_XUSBC),
        _ => return None,
    };
    Some(Device { clocks, parts })
}

/// Partitions powered and unclamped. Without PMC access, assume off:
/// a missing driver beats a hung bus.
fn powered(pmc: Option<&Mmio>, parts: u32) -> bool {
    parts == 0
        || pmc.is_some_and(|p| {
            p.r32(PMC_PWRGATE_STATUS) & parts == parts && p.r32(PMC_CLAMP_STATUS) & parts == 0
        })
}

fn status(car: &Mmio, pmc: Option<&Mmio>, d: &Device) -> u64 {
    let mut st = 0;
    if d.clocks
        .iter()
        .all(|c| car.r32(c.bank.enb) & (1 << c.bit) != 0)
    {
        st |= proto::ST_CLOCK;
    }
    if d.clocks
        .iter()
        .all(|c| car.r32(c.bank.rst) & (1 << c.bit) == 0)
    {
        st |= proto::ST_OUT_OF_RESET;
    }
    if powered(pmc, d.parts) {
        st |= proto::ST_POWERED;
    }
    st
}

fn enable(car: &Mmio, pmc: Option<&Mmio>, d: &Device) -> i64 {
    if d.parts != 0 {
        // PMC is not owned by clk: partition off, touch nothing.
        if !powered(pmc, d.parts) {
            return ENODEV;
        }
        // Already up (firmware loaded by an earlier stage): a reset
        // would lose it.
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
    }
    for c in d.clocks {
        let m = 1u32 << c.bit;
        car.w32(c.bank.rst_set, m); // assert reset
        car.w32(c.bank.enb_set + CLR, m); // clock off
        if c.source != 0 {
            car.w32(c.source, (car.r32(c.source) & c.keep) | c.src);
        }
        car.w32(c.bank.enb_set, m); // clock on
        time::sleep_us(2);
        car.w32(c.bank.rst_set + CLR, m); // release reset
        let _ = car.r32(c.bank.rst);
    }
    if status(car, pmc, d) == proto::ST_READY {
        0
    } else {
        EIO
    }
}

fn disable(car: &Mmio, d: &Device) {
    for c in d.clocks.iter().rev() {
        let m = 1u32 << c.bit;
        car.w32(c.bank.rst_set, m);
        car.w32(c.bank.enb_set + CLR, m);
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("clk: CAR registers");
    let pmc = Mmio::map(drv::dev_slot(1), PMC_PAGE, 0x1000).ok();
    let ep = drv::publish("clk").expect("clk: publish");
    println!("[clk] clock service ready");
    drv::serve(ep, |rx| {
        let mut st = 0;
        let r = match (rx.msg.label(), table(rx.msg.mr[0])) {
            (proto::ENABLE, Some(d)) => enable(&car, pmc.as_ref(), &d),
            (proto::DISABLE, Some(d)) => {
                disable(&car, &d);
                0
            }
            (proto::STATUS, Some(d)) => {
                st = status(&car, pmc.as_ref(), &d);
                0
            }
            _ => EINVAL,
        };
        let mut m = Msg::new(r as u32, 1, 0);
        m.mr[0] = st;
        Some(m)
    })
}

//! clk: device clocks/resets (CAR); values from hekate (bdk/soc/clock.c).
//! Also reports PMC power-partition status (STATUS), and brings up the
//! USB2 PLLs and wake detectors (PMC USB_AO) for the OTG device
//! controller (hekate bdk/usb/usbd.c).

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
/// UTMIP ID and VBUS wake detectors: power-down bits 2, 3.
const PMC_USB_AO: usize = 0x400 + 0xF0;
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
const Y: Bank = Bank {
    rst: 0x2A4,
    enb: 0x298,
    rst_set: 0x2A8,
    enb_set: 0x29C,
};

/// USB2 PLLs (38.4 MHz oscillator): PLLU 480 MHz, UTMIPLL 960 MHz.
const PLLU_BASE: usize = 0xC0;
const PLLU_MISC: usize = 0xCC;
const PLL_ENABLE: u32 = 1 << 30;
const PLL_LOCK: u32 = 1 << 27;
/// PLLU USB, HSIC, ICUSB and 48M outputs.
const PLLU_OUTPUTS: u32 = 0x2E0_0000;
const UTMIP_PLL_CFG0: usize = 0x480;
const UTMIP_PLL_CFG1: usize = 0x484;
const UTMIP_PLL_CFG2: usize = 0x488;
const UTMIP_XTAL_CLOCKEN: u32 = 1 << 30;
const UTMIPLL_HW_PWRDN_CFG0: usize = 0x52C;
const UTMIPLL_LOCK: u32 = 1 << 31;
/// XUSB_PADCTL reset (bank W bit 14): the pads are shared with XUSB.
const RST_DEV_W_CLR: usize = 0x43C;
const PADCTL_RESET: u32 = 1 << 14;
/// Fuse registers visible (CFG_ALL_VISIBLE), set by hekate at boot.
const MISC_CLK_ENB: usize = 0x48;
const FUSE_VISIBLE: u32 = 1 << 28;

/// One clock: bank, bit, source register (0: none) with bits to keep
/// and the value to set; `reset`: it also has a reset line.
struct Clock {
    bank: &'static Bank,
    bit: u32,
    source: usize,
    keep: u32,
    src: u32,
    reset: bool,
}

/// Extra condition for `ST_POWERED` (and setup for ENABLE).
#[derive(PartialEq)]
enum Needs {
    Nothing,
    /// PLLU running and locked, UTMIPLL programmed.
    UsbPlls,
    /// Fuse registers visible.
    FuseVisible,
}

/// One device: its clocks in enable order plus PMC partitions that must
/// be powered (0: always-on domain).
struct Device {
    clocks: &'static [Clock],
    parts: u32,
    needs: Needs,
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
        reset: true,
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
    reset: true,
}];
const SDMMC4: [Clock; 1] = [Clock {
    bank: &L,
    bit: 15,
    source: 0x164,
    keep: 0,
    src: 15,
    reset: true,
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
        reset: true,
    },
    Clock {
        bank: &W,
        bit: 28,
        source: 0x610,
        keep: 0x1FFF_FF00,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &U,
        bit: 25,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
];

/// USB2 OTG controller (USBD, bank L bit 22).
const USBD: [Clock; 1] = [Clock {
    bank: &L,
    bit: 22,
    source: 0,
    keep: 0,
    src: 0,
    reset: true,
}];
/// USB2 pad tracking: gate only, divider 6 (hekate).
const USB2_TRK: [Clock; 1] = [Clock {
    bank: &Y,
    bit: 18,
    source: 0x6CC,
    keep: 0xFFFF_FF00,
    src: 6,
    reset: false,
}];
/// Fuse controller (bank H bit 7): status only, for readers of fuses.
const FUSE: [Clock; 1] = [Clock {
    bank: &H,
    bit: 7,
    source: 0,
    keep: 0,
    src: 0,
    reset: true,
}];

fn table(dev: u64) -> Option<Device> {
    let needs = match dev {
        proto::USBD => Needs::UsbPlls,
        proto::FUSE => Needs::FuseVisible,
        _ => Needs::Nothing,
    };
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
        proto::USBD => (&USBD, 0),
        proto::USB2_TRK => (&USB2_TRK, 0),
        proto::FUSE => (&FUSE, 0),
        _ => return None,
    };
    Some(Device {
        clocks,
        parts,
        needs,
    })
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
        .all(|c| !c.reset || car.r32(c.bank.rst) & (1 << c.bit) == 0)
    {
        st |= proto::ST_OUT_OF_RESET;
    }
    let extra = match d.needs {
        Needs::Nothing => true,
        Needs::UsbPlls => car.r32(PLLU_BASE) & (PLL_ENABLE | PLL_LOCK) == PLL_ENABLE | PLL_LOCK,
        Needs::FuseVisible => car.r32(MISC_CLK_ENB) & FUSE_VISIBLE != 0,
    };
    if powered(pmc, d.parts) && extra {
        st |= proto::ST_POWERED;
    }
    st
}

/// Wait up to `us` for `bits` in a CAR register.
fn wait_bits(car: &Mmio, reg: usize, bits: u32, us: u64) -> bool {
    car.wait(reg, bits, bits, us)
}

/// PLLU (if not already running) then UTMIPLL, as hekate's USB init.
/// UTMIPLL lock only bounds the wait there too.
fn usb_plls(car: &Mmio, pmc: Option<&Mmio>) -> i64 {
    let pllu = car.r32(PLLU_BASE);
    if pllu & (PLL_ENABLE | PLL_LOCK) != PLL_ENABLE | PLL_LOCK {
        // Reference clock off while programming: DIVM 2, DIVN 25, 480 MHz.
        car.modify(PLLU_MISC, 0, 1 << 29);
        let cfg = (pllu & 0xFFE0_0000) | (1 << 24) | (1 << 16) | (0x19 << 8) | 2;
        car.w32(PLLU_BASE, cfg);
        car.w32(PLLU_BASE, cfg | PLL_ENABLE);
        if !wait_bits(car, PLLU_BASE, PLL_LOCK, 1000) {
            println!("[clk] PLLU: no lock");
            return EIO;
        }
        time::sleep_us(10);
    }
    car.modify(PLLU_BASE, 0, PLLU_OUTPUTS);
    // UTMIPLL: IDDQ under software control and off, then program it
    // with the PHY crystal clock gated.
    car.modify(UTMIPLL_HW_PWRDN_CFG0, 3, 1);
    time::sleep_us(10);
    car.modify(UTMIP_PLL_CFG2, UTMIP_XTAL_CLOCKEN, 0);
    car.w32(
        UTMIP_PLL_CFG0,
        (car.r32(UTMIP_PLL_CFG0) & 0xFF00_00FF) | (25 << 16) | (1 << 8),
    );
    car.w32(
        UTMIP_PLL_CFG2,
        (car.r32(UTMIP_PLL_CFG2) & 0xFF00_003F) | (24 << 18),
    );
    car.w32(
        UTMIP_PLL_CFG1,
        (car.r32(UTMIP_PLL_CFG1) & 0x07FF_A000) | (1 << 15) | 375,
    );
    wait_bits(car, UTMIPLL_HW_PWRDN_CFG0, UTMIPLL_LOCK, 20);
    car.modify(UTMIP_PLL_CFG2, 0, UTMIP_XTAL_CLOCKEN);
    // Samplers A-D powered up (force-powerdown bits off, powerup on).
    car.w32(
        UTMIP_PLL_CFG2,
        (car.r32(UTMIP_PLL_CFG2) & 0xFEFF_FFEA) | 0x200_0000 | 0x28 | 2,
    );
    if let Some(p) = pmc {
        p.modify(PMC_USB_AO, 0xC, 0);
    }
    0
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
    if d.needs == Needs::FuseVisible {
        // A reset would lose the sensed fuses: status only.
        return EINVAL;
    }
    if d.needs == Needs::UsbPlls {
        let r = usb_plls(car, pmc);
        if r != 0 {
            return r;
        }
    }
    for c in d.clocks {
        let m = 1u32 << c.bit;
        if c.reset {
            car.w32(c.bank.rst_set, m); // assert reset
        }
        car.w32(c.bank.enb_set + CLR, m); // clock off
        if c.source != 0 {
            car.w32(c.source, (car.r32(c.source) & c.keep) | c.src);
        }
        car.w32(c.bank.enb_set, m); // clock on
        time::sleep_us(2);
        if c.reset {
            car.w32(c.bank.rst_set + CLR, m); // release reset
            let _ = car.r32(c.bank.rst);
        }
    }
    if d.needs == Needs::UsbPlls {
        car.w32(RST_DEV_W_CLR, PADCTL_RESET);
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
        if c.reset {
            car.w32(c.bank.rst_set, m);
        }
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
            (proto::DISABLE, Some(d)) if d.needs != Needs::FuseVisible => {
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

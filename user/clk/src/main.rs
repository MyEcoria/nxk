//! clk : horloges et resets des peripheriques (controleur CAR).
//!
//! Le CAR regle aussi les coeurs et la memoire : il n'est confie qu'a ce
//! service, qui n'expose aux pilotes que l'activation de leur propre
//! peripherique. Sequence et valeurs reprises de hekate (bdk/soc/clock.c).
//!
//! Il lit aussi, sans jamais y ecrire, l'etat des partitions d'alimentation
//! du PMC : un bloc hors tension ou encore verrouille (clamp) bloque le bus
//! au premier acces, et c'est ici qu'un pilote apprend s'il peut toucher a
//! ses registres (STATUS).

#![no_std]
#![no_main]

use libnx::abi::err::{EINVAL, EIO, ENODEV};
use libnx::abi::proto::clk as proto;
use libnx::drv::{self, Mmio};
use libnx::ipc::Msg;
use libnx::{println, time};

const CAR: u64 = 0x6000_6000;
/// Registre CLR d'une paire SET/CLR.
const CLR: usize = 4;

/// Page du PMC (registres a partir de 0x400, hekate bdk/soc/pmc_t210.h).
const PMC_PAGE: u64 = 0x7000_E000;
const PMC_CLAMP_STATUS: usize = 0x400 + 0x2C;
const PMC_PWRGATE_STATUS: usize = 0x400 + 0x38;
/// Partitions XUSB (hekate bdk/soc/pmc.h, POWER_RAIL_XUSBx) : A = SuperSpeed,
/// C = hote.
const PART_XUSBA: u32 = 1 << 20;
const PART_XUSBC: u32 = 1 << 22;

/// Banc du CAR (L, H, U, V, W, X) : etat lu dans RST_DEVICES_x et
/// CLK_OUT_ENB_x, commandes par RST_DEV_x_SET et CLK_ENB_x_SET (+ CLR).
struct Bank {
    rst: usize,
    enb: usize,
    rst_set: usize,
    enb_set: usize,
}

const L: Bank = Bank { rst: 0x004, enb: 0x010, rst_set: 0x300, enb_set: 0x320 };
const H: Bank = Bank { rst: 0x008, enb: 0x014, rst_set: 0x308, enb_set: 0x328 };
const U: Bank = Bank { rst: 0x00C, enb: 0x018, rst_set: 0x310, enb_set: 0x330 };
const V: Bank = Bank { rst: 0x358, enb: 0x360, rst_set: 0x430, enb_set: 0x440 };
const W: Bank = Bank { rst: 0x35C, enb: 0x364, rst_set: 0x438, enb_set: 0x448 };
const X: Bank = Bank { rst: 0x28C, enb: 0x280, rst_set: 0x290, enb_set: 0x284 };

/// Une horloge : banc, bit, registre de source (0 : aucun) avec les bits a
/// conserver et la valeur a poser.
struct Clock {
    bank: &'static Bank,
    bit: u32,
    source: usize,
    keep: u32,
    src: u32,
}

/// Un peripherique : ses horloges dans l'ordre d'activation et les
/// partitions PMC qui doivent etre allumees (0 : domaine toujours alimente).
struct Device {
    clocks: &'static [Clock],
    parts: u32,
}

/// Source 6 (oscillateur 19,2 MHz) : diviseur 4 -> 4,8 MHz (100 kHz), 1 ->
/// 19,2 MHz (400 kHz).
const fn i2c(bank: &'static Bank, bit: u32, source: usize, div: u32) -> Clock {
    Clock { bank, bit, source, keep: 0, src: (6 << 29) | (div - 1) }
}

const I2C1: [Clock; 1] = [i2c(&L, 12, 0x124, 4)];
const I2C2: [Clock; 1] = [i2c(&H, 22, 0x198, 1)];
const I2C3: [Clock; 1] = [i2c(&U, 3, 0x1B8, 1)];
const I2C4: [Clock; 1] = [i2c(&V, 7, 0x3C4, 4)];
const I2C5: [Clock; 1] = [i2c(&H, 15, 0x128, 1)];
const I2C6: [Clock; 1] = [i2c(&X, 6, 0x65C, 4)];
/// SDMMC : PLLP 408 MHz / 8,5 = 48 MHz, le controleur divise ensuite lui-meme.
const SDMMC1: [Clock; 1] = [Clock { bank: &L, bit: 14, source: 0x150, keep: 0, src: 15 }];
const SDMMC4: [Clock; 1] = [Clock { bank: &L, bit: 15, source: 0x164, keep: 0, src: 15 }];
/// Hote xHCI, comme hekate pour le mode peripherique (bdk/usb/xusbd.c) :
/// porte commune des sources XUSB (CLK_W_XUSB), logique SuperSpeed
/// (CLK_W_XUSB_SS) puis hote (CLK_U_XUSB_HOST). La source SuperSpeed est
/// remise sur CLK_M, toujours en marche (hekate la laisse ainsi en coupant
/// l'USB) : la PLLU et ses 120 MHz relevent de la mise en service complete,
/// avec le microcode. Celle de l'hote n'est pas touchee (CLK_M au reset).
const XUSB_HOST: [Clock; 3] = [
    Clock { bank: &W, bit: 15, source: 0, keep: 0, src: 0 },
    Clock { bank: &W, bit: 28, source: 0x610, keep: 0x1FFF_FF00, src: 0 },
    Clock { bank: &U, bit: 25, source: 0, keep: 0, src: 0 },
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

/// Partitions allumees et deverrouillees. Sans acces au PMC, on les tient
/// pour coupees : mieux vaut un pilote absent qu'un bus bloque.
fn powered(pmc: Option<&Mmio>, parts: u32) -> bool {
    parts == 0 || pmc.is_some_and(|p| p.r32(PMC_PWRGATE_STATUS) & parts == parts && p.r32(PMC_CLAMP_STATUS) & parts == 0)
}

fn status(car: &Mmio, pmc: Option<&Mmio>, d: &Device) -> u64 {
    let mut st = 0;
    if d.clocks.iter().all(|c| car.r32(c.bank.enb) & (1 << c.bit) != 0) {
        st |= proto::ST_CLOCK;
    }
    if d.clocks.iter().all(|c| car.r32(c.bank.rst) & (1 << c.bit) == 0) {
        st |= proto::ST_OUT_OF_RESET;
    }
    if powered(pmc, d.parts) {
        st |= proto::ST_POWERED;
    }
    st
}

fn enable(car: &Mmio, pmc: Option<&Mmio>, d: &Device) -> i64 {
    if d.parts != 0 {
        // Le PMC n'est pas a clk : partition coupee, on ne touche a rien.
        if !powered(pmc, d.parts) {
            return ENODEV;
        }
        // Deja en service (microcode charge par un etage precedent) : un
        // reset le perdrait.
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
    }
    for c in d.clocks {
        let m = 1u32 << c.bit;
        car.w32(c.bank.rst_set, m); // en reset
        car.w32(c.bank.enb_set + CLR, m); // horloge coupee
        if c.source != 0 {
            car.w32(c.source, (car.r32(c.source) & c.keep) | c.src);
        }
        car.w32(c.bank.enb_set, m); // horloge active
        time::sleep_us(2);
        car.w32(c.bank.rst_set + CLR, m); // sortie du reset
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
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("clk : registres CAR");
    let pmc = Mmio::map(drv::dev_slot(1), PMC_PAGE, 0x1000).ok();
    let ep = drv::publish("clk").expect("clk : publication");
    println!("[clk] service des horloges pret");
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

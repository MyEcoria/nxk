//! gc: the game card reader. The cartridge's detect switch (GPIO S3, pad
//! CAM_I2C_SDA), the Lotus3 ASIC's rail (MAX77620 LDO3, 3.1 V) and reset
//! (GPIO BB3), and its SDMMC2 controller (`nxk-sdcard::gc`). Pins and rail
//! from Atmosphere's GPIO tables and hekate's `max7762x`; the bring-up
//! follows hekate's `sdmmc_storage_init_gc`.
//!
//! The ASIC is powered only for a `PROBE`, which stops where the ASIC's
//! bootrom does: it answers tuning and CMD13, and nothing else without
//! Nintendo's signed firmware. Loading that burns OTP fuses, so nothing
//! here ever sends CMD60.

#![no_std]
#![no_main]

use libnx::abi::err::{self, EINVAL};
use libnx::abi::proto::{clk, gc as proto, RESUME, SUSPEND};
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::Msg;
use libnx::{println, time};
use nxk_sdcard::gc::{Gc, SDMMC2_OFFSET};
use nxk_sdcard::{Host, Regs, SDMMC1};

const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;

/// Pad CAM_I2C_SDA is GPIO S3: pinmux function 2, input buffer on, pulled
/// up (the cartridge grounds it). `input` moves this pad off I2C3 with a
/// read-modify-write, so the function is set again before every read.
const PIN_CAM_I2C_SDA: usize = 0xD8;
const FUNC_GPIO: u32 = 2;
const INPUT_ENABLE: u32 = 1 << 6;
const PULL_UP: u32 = 2 << 2;
const PULL_MASK: u32 = 3 << 2;
const PARKED: u32 = 1 << 5;
const TRISTATE: u32 = 1 << 4;

/// GPIO ports, plain registers (the masked ones did not take on an
/// Erista, hwboot/README.md run 4): S3 detect, BB3 reset.
const PORT_S: usize = 18;
const PORT_BB: usize = 27;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const GPIO_IN: usize = 0x30;
const DETECT: u32 = 1 << 3;
const RESET: u32 = 1 << 3;

/// MAX77620 on the power I2C bus: LDO3 (the ASIC) holds its voltage in
/// bits 5:0 (800 mV + 50 mV steps) and its enable in bits 7:6.
const PWR_BUS: u8 = 5;
const PMIC: u8 = 0x3C;
const LDO3_CFG: u8 = 0x29;
const LDO3_CFG2: u8 = 0x2A;
const FPS_LDO3: u8 = 0x49;
const LDO3_3V1: u8 = 46;
const LDO_ON: u8 = 0xC0;

/// Waits the open documentation does not give: generous ones.
const RAIL_SETTLE_MS: u64 = 20;
const RESET_SETTLE_MS: u64 = 20;

const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}

/// SDMMC2's block inside SDMMC1's page: offsets from 0, and nothing of
/// SDMMC1 (the `sd` service's) reachable through it.
struct Window(Mmio);

impl Window {
    fn at(off: usize) -> usize {
        assert!(off < 0x200, "gc: register outside SDMMC2");
        SDMMC2_OFFSET + off
    }
}

impl Regs for Window {
    fn r8(&self, off: usize) -> u8 {
        self.0.r8(Self::at(off))
    }
    fn w8(&self, off: usize, v: u8) {
        self.0.w8(Self::at(off), v)
    }
    fn r16(&self, off: usize) -> u16 {
        self.0.r16(Self::at(off))
    }
    fn w16(&self, off: usize, v: u16) {
        self.0.w16(Self::at(off), v)
    }
    fn r32(&self, off: usize) -> u32 {
        self.0.r32(Self::at(off))
    }
    fn w32(&self, off: usize, v: u32) {
        self.0.w32(Self::at(off), v)
    }
}

/// A plain register page.
struct Page(Mmio);

impl Regs for Page {
    fn r8(&self, off: usize) -> u8 {
        self.0.r8(off)
    }
    fn w8(&self, off: usize, v: u8) {
        self.0.w8(off, v)
    }
    fn r16(&self, off: usize) -> u16 {
        self.0.r16(off)
    }
    fn w16(&self, off: usize, v: u16) {
        self.0.w16(off, v)
    }
    fn r32(&self, off: usize) -> u32 {
        self.0.r32(off)
    }
    fn w32(&self, off: usize, v: u32) {
        self.0.w32(off, v)
    }
}

/// Commands are polled: time and the console only.
struct Timing;

impl Host for Timing {
    fn now_us(&self) -> u64 {
        time::now_ns() / 1000
    }
    fn sleep_us(&self, us: u64) {
        if us >= 1000 && us.is_multiple_of(1000) {
            time::sleep_ms(us / 1000);
        } else {
            time::sleep_us(us);
        }
    }
    fn wait(&self, timeout_us: u64, done: &dyn Fn() -> bool, _xfer: bool) {
        time::poll(timeout_us, done);
    }
    fn log(&self, args: core::fmt::Arguments) {
        libnx::print!("{}", args);
    }
}

/// What a probe reached.
#[derive(Clone, Copy)]
struct Report {
    present: u64,
    stages: u64,
    /// 0: every step done, else the errno of the one after `stages`.
    error: i64,
    status: u64,
    rail: u64,
    ms: u64,
}

impl Report {
    const NONE: Report = Report {
        present: proto::PRESENT_UNKNOWN,
        stages: 0,
        error: 0,
        status: proto::NO_STATUS,
        rail: 0,
        ms: 0,
    };

    fn reply(&self) -> Msg {
        Msg::from_words(
            0,
            &[
                self.present,
                self.stages,
                self.error as u64,
                self.status,
                self.rail,
                self.ms,
            ],
        )
    }
}

struct Reader {
    sdmmc: Mmio,
    misc: Page,
    pinmux: Page,
    gpio: Page,
    i2c: Option<I2c>,
    mariko: bool,
    last: Report,
}

impl Reader {
    /// Cartridge switch: low with a cartridge in. The pad is put in GPIO
    /// mode on every call, then read.
    fn present(&self) -> u64 {
        let pad = self.pinmux.r32(PIN_CAM_I2C_SDA);
        let want =
            (pad & !(PULL_MASK | PARKED | TRISTATE | 3)) | INPUT_ENABLE | PULL_UP | FUNC_GPIO;
        if pad != want {
            self.pinmux.w32(PIN_CAM_I2C_SDA, want);
        }
        let (cnf, oe) = (port(GPIO_CNF, PORT_S), port(GPIO_OE, PORT_S));
        if self.gpio.r32(cnf) & DETECT == 0 {
            self.gpio.modify(cnf, 0, DETECT);
        }
        if self.gpio.r32(oe) & DETECT != 0 {
            self.gpio.modify(oe, DETECT, 0);
        }
        time::sleep_us(100);
        if self.gpio.r32(port(GPIO_IN, PORT_S)) & DETECT == 0 {
            proto::PRESENT_YES
        } else {
            proto::PRESENT_NO
        }
    }

    /// Reset line as an output, low or high.
    fn reset_line(&self, high: bool) {
        let (cnf, out, oe) = (
            port(GPIO_CNF, PORT_BB),
            port(GPIO_OUT, PORT_BB),
            port(GPIO_OE, PORT_BB),
        );
        self.gpio.modify(cnf, 0, RESET);
        self.gpio.modify(
            out,
            if high { 0 } else { RESET },
            if high { RESET } else { 0 },
        );
        self.gpio.modify(oe, 0, RESET);
        let _ = self.gpio.r32(oe);
    }

    fn pmic(&self, reg: u8) -> Option<u8> {
        self.i2c.as_ref()?.read_reg(PWR_BUS, PMIC, reg).ok()
    }

    /// LDO3 CFG, CFG2 and FPS as found, for the report.
    fn rail_state(&self) -> u64 {
        let b = |r| self.pmic(r).unwrap_or(0xFF) as u64;
        b(LDO3_CFG) | (b(LDO3_CFG2) << 8) | (b(FPS_LDO3) << 16)
    }

    fn rail(&self, on: bool) -> Result<(), i64> {
        let i2c = self.i2c.as_ref().ok_or(-err::ENODEV)?;
        let v = if on { LDO_ON | LDO3_3V1 } else { LDO3_3V1 };
        i2c.write_reg(PWR_BUS, PMIC, LDO3_CFG, v)
    }

    /// Power the ASIC, release its reset, bring SDMMC2 up, tune it, ask
    /// for its status, power it down. Each step's bit is kept as it is
    /// reached; the first failure ends the probe.
    fn probe(&mut self) -> Report {
        let start = time::now_ns();
        let mut r = Report::NONE;
        r.present = self.present();
        r.rail = self.rail_state();
        println!(
            "[gc] cartridge {}, LDO3 cfg {:#04x} cfg2 {:#04x} fps {:#04x}",
            if r.present == proto::PRESENT_YES {
                "in"
            } else {
                "not detected"
            },
            r.rail & 0xFF,
            r.rail >> 8 & 0xFF,
            r.rail >> 16 & 0xFF
        );
        let up = self.bring_up(&mut r);
        self.power_down();
        if let Err(e) = up {
            r.error = e;
            println!("[gc] stopped after step {:#x}: {}", r.stages, err::name(e));
        }
        r.ms = (time::now_ns() - start) / 1_000_000;
        self.last = r;
        r
    }

    fn bring_up(&self, r: &mut Report) -> Result<(), i64> {
        // Reset held low before the rail rises.
        self.reset_line(false);
        self.rail(true)?;
        time::sleep_ms(RAIL_SETTLE_MS);
        r.stages |= proto::ST_RAIL;
        libnx::clk::enable(clk::SDMMC2)?;
        if !libnx::clk::ready(clk::SDMMC2) {
            return Err(-err::ENODEV);
        }
        self.reset_line(true);
        time::sleep_ms(RESET_SETTLE_MS);
        r.stages |= proto::ST_RESET;

        let gc = Gc::new(Window(self.sdmmc), Timing, self.mariko);
        gc.start(&self.misc, &self.pinmux)?;
        r.stages |= proto::ST_HOST;
        time::sleep_ms(1);
        let tuned = gc.tune();
        if tuned.is_ok() {
            r.stages |= proto::ST_TUNED;
        }
        // CMD13 is information: the bootrom may not answer it.
        match gc.status() {
            Ok(s) => {
                r.stages |= proto::ST_STATUS;
                r.status = s as u64;
            }
            Err(e) => println!("[gc] CMD13: {}", err::name(e)),
        }
        gc.stop();
        tuned
    }

    /// Reset low, then the rail and the clocks off: the ASIC sleeps
    /// unpowered between probes.
    fn power_down(&self) {
        self.reset_line(false);
        let _ = self.rail(false);
        let _ = libnx::clk::disable(clk::SDMMC2);
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let map = |k, base| Mmio::map(drv::dev_slot(k), base, 0x1000);
    let (Ok(sdmmc), Ok(misc), Ok(pinmux), Ok(gpio)) = (
        map(0, SDMMC1),
        map(1, APB_MISC),
        map(2, PINMUX),
        map(3, GPIO),
    ) else {
        println!("[gc] no game card reader: registers refused");
        return 1;
    };
    let mariko = (misc.r32(GP_HIDREV) >> 4) & 0xF >= 2;
    // Nothing is touched until a request: the ASIC stays unpowered.
    let mut reader = Reader {
        sdmmc,
        misc: Page(misc),
        pinmux: Page(pinmux),
        gpio: Page(gpio),
        i2c: I2c::connect().ok(),
        mariko,
        last: Report::NONE,
    };
    let ep = drv::publish("gc").expect("gc: publish");
    println!("[gc] game card reader ready");
    drv::serve(ep, |rx| {
        Some(match rx.msg.label() {
            proto::STATUS => {
                let mut r = reader.last;
                r.present = reader.present();
                r.reply()
            }
            proto::PROBE => reader.probe().reply(),
            // Nothing is powered between probes.
            SUSPEND | RESUME => Msg::new(0, 0, 0),
            _ => Msg::error(-EINVAL),
        })
    })
}

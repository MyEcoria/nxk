//! cpufreq: the CPU clock at sys-clk's steps, on PLLX, with the CPU rail
//! (MAX77621 on I2C5) at the voltage NVIDIA's L4T gives a PLLX-sourced
//! CPU of this chip's speedo (`dvfs`). Erista Switch only. Every boot
//! keeps the loader's clock; back to it when the SoC reaches 80 C.

#![no_std]
#![no_main]

mod dvfs;

use dvfs::Pll;
use libnx::abi::err::{self, EINVAL, EIO, ENODEV, ERANGE, ETIMEDOUT};
use libnx::abi::proto::{clk, cpufreq as proto, power};
use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg};
use libnx::ns::Service;
use libnx::{println, time};

const CAR: u64 = 0x6000_6000;
/// The G cluster's clock sources, 4 bits per CPU state (run in bits
/// 7:4): 8 = PLLX_OUT0_LJ (hekate's setting), 4 = PLLP_OUT0, where the CPU
/// waits while PLLX relocks. CCLKG, as Linux and Atmosphère's warmboot
/// switch a running CPU (hekate writes the 0x20 alias before starting it).
const CCLKG_BURST_POLICY: usize = 0x368;
const CCLK_SOURCES: u32 = 0xFFFF;
const CCLK_PLLX: u32 = 0x8888;
const CCLK_PLLP: u32 = 0x4444;
const PLLX_BASE: usize = 0xE0;
const PLLX_MISC: usize = 0xE4;
const PLL_BYPASS: u32 = 1 << 31;
const PLL_ENABLE: u32 = 1 << 30;
const PLL_LOCK: u32 = 1 << 27;
const PLLX_DIVIDERS: u32 = 0x1F << 20 | 0xFF << 8 | 0xFF;
const PLLX_MISC_LOCK_ENABLE: u32 = 1 << 18;
/// PLLP reaches the CPU only through this branch: selecting PLLP with it
/// off stops every core (Linux clk-super.c, Atmosphère warmboot).
const CLK_OUT_ENB_Y: usize = 0x298;
const CLK_ENB_Y_SET: usize = 0x29C;
const PLLP_OUT_CPU: u32 = 1 << 31;
const PLLP_KHZ: u64 = 408_000;
/// Steps of `probe`'s countdown: about 20 µs at 1 GHz.
const PROBE_LOOPS: u64 = 20_000;

const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;
/// Fuse registers at +0x800 (hekate bdk/soc/fuse.h).
const FUSE_PAGE: u64 = 0x7000_F000;
const FUSE_SKU_INFO: usize = 0x800 + 0x110;
const FUSE_CPU_SPEEDO_0: usize = 0x800 + 0x114;
const FUSE_SPARE_BIT: usize = 0x800 + 0x380;
/// The Switch's Erista SKU: CPU speedo ID 3 in Linux, the ODN tables.
const SKU_ERISTA: u32 = 0x83;

/// MAX77621 CPU regulator (hekate bdk/power/max7762x.h): voltage code
/// in VOUT bits 6:0 next to the enable bit, mirrored in VOUT_DVS.
const MAX77621: u8 = 0x1B;
const VOUT: u8 = 0x00;
const VOUT_DVS: u8 = 0x01;
const VOUT_CODE: u8 = 0x7F;
const VOUT_ENABLE: u8 = 0x80;

/// SoC temperature check while above the boot clock.
const HOT_CHECK_NS: u64 = 2_000_000_000;

static POWER: Service = Service::optional("power");

struct Cpu {
    car: Mmio,
    i2c: I2c,
    /// CPU speedo value; 0: unsupported, `BOOT` only.
    speedo: u32,
    allowed: u64,
    /// The loader's PLLX dividers and rail code.
    boot_pll: Pll,
    boot_code: u8,
    /// Step set (MHz), `proto::BOOT`.
    step: u64,
    hot: bool,
}

impl Cpu {
    fn clock_khz(&self) -> u64 {
        match (self.car.r32(CCLKG_BURST_POLICY) >> 4) & 0xF {
            8 => Pll::from_base(self.car.r32(PLLX_BASE)).khz(),
            4 => PLLP_KHZ,
            _ => 0,
        }
    }

    fn rail_code(&self) -> Result<u8, i64> {
        Ok(self.i2c.read_reg(5, MAX77621, VOUT)? & VOUT_CODE)
    }

    /// hekate `max7762x_regulator_set_voltage`: VOUT, then VOUT_DVS,
    /// then 1 ms for the ramp (12.5 mV/µs); checked by reading back.
    fn set_rail(&self, code: u8) -> Result<(), i64> {
        if dvfs::rail_uv(code) > dvfs::MAX_UV {
            return Err(-ERANGE);
        }
        let v = (self.i2c.read_reg(5, MAX77621, VOUT)? & !VOUT_CODE) | code;
        self.i2c.write_reg(5, MAX77621, VOUT, v)?;
        self.i2c.write_reg(5, MAX77621, VOUT_DVS, VOUT_ENABLE | v)?;
        time::sleep_ms(1);
        match self.rail_code()? {
            c if c == code => Ok(()),
            _ => Err(-EIO),
        }
    }

    /// PLLX off, new dividers, on, locked (Linux PLL_POST_LOCK_DELAY after).
    fn relock(&self, pll: Pll) -> bool {
        let car = &self.car;
        car.modify(PLLX_BASE, PLL_ENABLE, 0);
        car.modify(PLLX_BASE, PLLX_DIVIDERS | PLL_BYPASS, pll.base());
        car.modify(PLLX_MISC, 0, PLLX_MISC_LOCK_ENABLE);
        car.modify(PLLX_BASE, 0, PLL_ENABLE);
        let locked = car.wait(PLLX_BASE, PLL_LOCK, PLL_LOCK, 2000);
        time::sleep_us(50);
        locked
    }

    /// PLLX relocked at `pll` while the CPU runs from PLLP. PLLX is only
    /// turned off once the CPU measurably slowed down to PLLP. A failed
    /// lock goes back to the old dividers, or stays on PLLP (408 MHz,
    /// safe at any rail voltage).
    fn set_pll(&self, pll: Pll) -> Result<(), i64> {
        let car = &self.car;
        let old = Pll::from_base(car.r32(PLLX_BASE));
        let khz = old.khz();
        // Under 1.3x PLLP the probe cannot tell the two clocks apart.
        if khz * 10 < PLLP_KHZ * 13 {
            return Err(-EIO);
        }
        car.w32(CLK_ENB_Y_SET, PLLP_OUT_CPU);
        time::sleep_us(2);
        if car.r32(CLK_OUT_ENB_Y) & PLLP_OUT_CPU == 0 {
            println!("[cpufreq] PLLP branch to the CPU stays off: no change");
            return Err(-EIO);
        }
        let policy = car.r32(CCLKG_BURST_POLICY) & !CCLK_SOURCES;
        let fast = probe();
        car.w32(CCLKG_BURST_POLICY, policy | CCLK_PLLP);
        let _ = car.r32(CCLKG_BURST_POLICY);
        time::sleep_us(2);
        let slow = probe();
        // At least half the expected slowdown (khz / 408 MHz).
        if slow * 2 * PLLP_KHZ < fast * (khz + PLLP_KHZ) {
            car.w32(CCLKG_BURST_POLICY, policy | CCLK_PLLX);
            println!(
                "[cpufreq] CPU not slowed by PLLP ({} -> {} ticks): PLLX kept, no change",
                fast, slow
            );
            return Err(-EIO);
        }
        let r = if self.relock(pll) {
            Ok(())
        } else if self.relock(old) {
            Err(-EIO)
        } else {
            println!("[cpufreq] PLLX does not lock: CPU left on PLLP");
            return Err(-EIO);
        };
        car.w32(CCLKG_BURST_POLICY, policy | CCLK_PLLX);
        time::sleep_us(2);
        let now = Pll::from_base(car.r32(PLLX_BASE)).khz();
        println!(
            "[cpufreq] PLLX {} MHz, measured {} MHz{}",
            now / 1000,
            PLLP_KHZ * slow / probe().max(1) / 1000,
            if r.is_ok() {
                ""
            } else {
                " (new dividers did not lock)"
            }
        );
        r
    }

    /// The rail never drops below what either clock needs: raised before
    /// a faster clock, lowered after a slower one.
    fn apply(&mut self, pll: Pll, code: u8) -> Result<(), i64> {
        let now = self.rail_code()?;
        if code > now {
            self.set_rail(code)?;
        }
        // The clock is still one the old rail ran.
        if let Err(e) = self.set_pll(pll) {
            if code > now {
                let _ = self.set_rail(now);
            }
            return Err(e);
        }
        if code < now {
            self.set_rail(code)?;
        }
        Ok(())
    }

    fn set(&mut self, mhz: u64) -> Result<(), i64> {
        if mhz == proto::BOOT && self.step == proto::BOOT {
            return Ok(());
        }
        if mhz == proto::BOOT {
            let r = self.apply(self.boot_pll, self.boot_code);
            if r.is_ok() {
                self.step = proto::BOOT;
            }
            return r;
        }
        let i = proto::STEPS_MHZ
            .iter()
            .position(|&s| s == mhz)
            .ok_or(-EINVAL)?;
        if self.speedo == 0 {
            return Err(-ENODEV);
        }
        if self.allowed & 1 << i == 0 {
            return Err(-ERANGE);
        }
        let pll = dvfs::pllx(mhz).ok_or(-EINVAL)?;
        let code = dvfs::rail_code(dvfs::voltage_uv(self.speedo, i));
        self.apply(pll, code)?;
        self.step = mhz;
        self.hot = false;
        Ok(())
    }

    fn status(&self) -> Msg {
        let mut m = Msg::new(0, 6, 0);
        m.mr[0] = self.clock_khz();
        m.mr[1] = self.rail_code().map_or(0, |c| dvfs::rail_uv(c) as u64);
        m.mr[2] = self.speedo as u64;
        m.mr[3] =
            (self.hot as u64 * proto::ST_HOT) | ((self.speedo == 0) as u64 * proto::ST_UNSUPPORTED);
        m.mr[4] = self.allowed;
        m.mr[5] = self.step;
        m
    }

    /// Above the boot clock, back to it once the SoC reaches `HOT_MC`.
    fn check_heat(&mut self) {
        if self.step == proto::BOOT || self.step <= self.boot_pll.khz() / 1000 {
            return;
        }
        let Ok(m) = POWER.call(&Msg::new(power::TEMPERATURE, 0, 0)) else {
            return;
        };
        let t = m.mr[1] as i64;
        if t >= proto::HOT_MC {
            let r = self.set(proto::BOOT);
            self.hot = true;
            println!(
                "[cpufreq] SoC at {}.{} C: back to the boot clock ({})",
                t / 1000,
                t % 1000 / 100,
                match r {
                    Ok(()) => "done",
                    Err(e) => err::name(e),
                }
            );
        }
    }
}

/// Counter ticks of a dependent countdown, which scale with the CPU
/// clock: fewest of 5 runs, as an interrupt only lengthens one.
fn probe() -> u64 {
    (0..5)
        .map(|_| {
            let t0 = time::ticks();
            // SAFETY: registers only.
            unsafe {
                core::arch::asm!(
                    "1: subs {n}, {n}, #1",
                    "b.ne 1b",
                    n = inout(reg) PROBE_LOOPS => _,
                    options(nomem, nostack),
                );
            }
            time::ticks() - t0
        })
        .min()
        .unwrap_or(0)
}

/// CPU speedo value, or why this console keeps the boot clock.
fn chip(car: &Mmio, i2c: &I2c) -> Result<u32, &'static str> {
    let misc = Mmio::map(drv::dev_slot(1), APB_MISC, 0x1000).map_err(|_| "no APB_MISC")?;
    if (misc.r32(GP_HIDREV) >> 4) & 0xF >= 2 {
        return Err("Mariko: not supported yet");
    }
    // Fuse reads hang the bus unless the fuse block is clocked.
    if !libnx::clk::ready(clk::FUSE) {
        return Err("fuses not readable");
    }
    let fuse = Mmio::map(drv::dev_slot(2), FUSE_PAGE, 0x1000).map_err(|_| "no fuses")?;
    let sku = fuse.r32(FUSE_SKU_INFO) & 0xFF;
    if sku != SKU_ERISTA {
        println!(
            "[cpufreq] SKU {:#04x}, not the Switch's {:#04x}",
            sku, SKU_ERISTA
        );
        return Err("not an Erista Switch");
    }
    let spare = |b: usize| fuse.r32(FUSE_SPARE_BIT + 4 * b) & 1;
    let rev = spare(4) << 2 | spare(3) << 1 | spare(2);
    let speedo = dvfs::speedo(rev, fuse.r32(FUSE_CPU_SPEEDO_0)).ok_or("CPU speedo not fused")?;
    let cclkg = car.r32(CCLKG_BURST_POLICY);
    if (cclkg >> 4) & 0xF != 8 || car.r32(PLLX_BASE) & PLL_ENABLE == 0 {
        println!(
            "[cpufreq] CCLKG {:#010x}, PLLX {:#010x}",
            cclkg,
            car.r32(PLLX_BASE)
        );
        return Err("CPU not on PLLX");
    }
    match i2c.read_reg(5, MAX77621, VOUT) {
        Ok(v) if v & VOUT_ENABLE != 0 => Ok(speedo),
        Ok(_) => Err("CPU regulator off"),
        Err(_) => Err("CPU regulator unreadable"),
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("cpufreq: CAR registers");
    let i2c = I2c::connect().expect("cpufreq: i2c service");
    let speedo = match chip(&car, &i2c) {
        Ok(s) => s,
        Err(why) => {
            println!("[cpufreq] boot clock only: {}", why);
            0
        }
    };
    let mut cpu = Cpu {
        boot_pll: Pll::from_base(car.r32(PLLX_BASE)),
        boot_code: 0,
        car,
        i2c,
        speedo,
        allowed: if speedo == 0 {
            0
        } else {
            dvfs::allowed(speedo)
        },
        step: proto::BOOT,
        hot: false,
    };
    cpu.boot_code = match cpu.rail_code() {
        Ok(c) => c,
        Err(_) => {
            cpu.speedo = 0;
            cpu.allowed = 0;
            0
        }
    };
    if cpu.speedo != 0 {
        let top = (0..proto::STEPS_MHZ.len())
            .rev()
            .find(|&i| cpu.allowed & 1 << i != 0)
            .map_or(0, |i| proto::STEPS_MHZ[i]);
        println!(
            "[cpufreq] ready: {} MHz, {} mV, speedo {}, steps up to {} MHz",
            cpu.clock_khz() / 1000,
            dvfs::rail_uv(cpu.boot_code) / 1000,
            cpu.speedo,
            top
        );
    }
    let ep = drv::publish("cpufreq").expect("cpufreq: publish");
    let mut next = time::now_ns() + HOT_CHECK_NS;
    loop {
        let wait = next.saturating_sub(time::now_ns()).max(1);
        match ipc::recv_timeout(ep, wait) {
            Ok(rx) => {
                let reply = match rx.msg.label() {
                    proto::STATUS => cpu.status(),
                    proto::SET => match cpu.set(rx.msg.mr[0]) {
                        Ok(()) => cpu.status(),
                        Err(e) => Msg::error(e),
                    },
                    _ => Msg::error(-EINVAL),
                };
                let _ = ipc::reply(&reply);
            }
            Err(e) if e != -ETIMEDOUT => time::sleep_ms(10),
            Err(_) => {}
        }
        if time::now_ns() >= next {
            cpu.check_heat();
            next = time::now_ns() + HOT_CHECK_NS;
        }
    }
}

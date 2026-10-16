//! fan: the fan on PWM channel 1, its 5V rail and its tachometer, as
//! hekate bdk/thermal/fan.c. Every 2 s its speed follows the SoC
//! temperature (`power`) with Nyx's steps, full speed from 60 C; or a
//! duty set by hand. Nyx stops it before chaining to nxk.

#![no_std]
#![no_main]

use libnx::abi::err::{self, EINVAL, ENODEV, ETIMEDOUT};
use libnx::abi::proto::{clk, fan as proto, power};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::ns::{self, Service};
use libnx::{println, time};

const PWM: u64 = 0x7000_A000;
const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
const PMC_PAGE: u64 = 0x7000_E000;
const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;

/// PWM channel 1 (hekate PWM_CONTROLLER_PWM_CSR_1): enable, duty in
/// bits 30:16 in 1/256 (bit 24 alone: 100%).
const PWM_CSR_1: usize = 0x10;
const PWM_EN: u32 = 1 << 31;
/// The fan's input is inverted: a 100% high output stops it.
const PWM_STOP: u32 = PWM_EN | 0x100 << 16;
/// Duty steps: 1 (960 RPM) to 236 (11000 RPM), hekate `fan_set_duty`.
const FULL: u32 = 236;
/// Nyx's highest step, kept without a temperature.
const SAFE: u32 = 102;

/// Pinmux (hekate bdk/soc/pinmux.h): pads of GPIO A5 (5V regulator
/// enable), CC4 (USB VBUS as 5V source), S7 (tachometer), V4 (PWM1).
const PINMUX_SATA_LED_ACTIVE: usize = 0x4C;
const PINMUX_USB_VBUS_EN0: usize = 0x1A8;
const PINMUX_CAM1_PWDN: usize = 0x1EC;
const PINMUX_LCD_GPIO2: usize = 0x20C;
const PULL_DOWN: u32 = 1 << 2;
const PULL_UP: u32 = 2 << 2;
const TRISTATE: u32 = 1 << 4;
const PARKED: u32 = 1 << 5;
const INPUT_ENABLE: u32 = 1 << 6;
const LPDR: u32 = 1 << 8;
/// Pad function 1 (PWM1 on LCD_GPIO2).
const FUNC1: u32 = 1;

/// GPIO ports, plain registers: the masked ones did not take on an
/// Erista (hwboot/README.md, run 4).
const PORT_A: usize = 0;
const PORT_S: usize = 18;
const PORT_V: usize = 21;
const PORT_CC: usize = 28;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const GPIO_INT_STA: usize = 0x40;
const GPIO_INT_ENB: usize = 0x50;
const GPIO_INT_LVL: usize = 0x60;
const GPIO_INT_CLR: usize = 0x70;
const REGULATOR_5V: u32 = 1 << 5;
const VBUS_5V: u32 = 1 << 4;
const TACH: u32 = 1 << 7;
const PWM_PAD: u32 = 1 << 4;
const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}

/// PMC (registers from 0x400): GPIO pads powered, and told they run
/// at 1.8 V (hekate `regulator_5v_enable`).
const PMC_NO_IOPOWER: usize = 0x400 + 0x44;
const PMC_PWR_DET_VAL: usize = 0x400 + 0xE4;
const PMC_GPIO: u32 = 1 << 21;

const PERIOD_NS: u64 = 2_000_000_000;
const CALL_NS: u64 = 200_000_000;
/// Speed measured this long after a change (spin-up), then each minute.
const SETTLE_NS: u64 = 3_000_000_000;
const RECHECK_NS: u64 = 60_000_000_000;
const WINDOW_NS: u64 = 1_000_000_000;
/// Two tachometer pulses per turn.
const PULSES: u64 = 2;
const HOT_C: i64 = proto::HOT_MC / 1000;
/// Full speed lasts until the SoC is back below this.
const COOL_C: i64 = HOT_C - 3;
/// Failed readings in a row before the sensor counts as lost.
const LOST: u32 = 3;

static POWER: Service = Service::optional("power");

/// Nyx `fan_set_from_temp` (whole degrees): 40 and 41 keep the duty.
fn nyx_duty(c: i64, cur: u32) -> u32 {
    match c {
        52.. => 102,
        47.. => 76,
        42.. => 51,
        ..=39 => 0,
        _ => cur,
    }
}

fn percent(duty: u32) -> u64 {
    (duty as u64 * 100 + FULL as u64 / 2) / FULL as u64
}

struct Fan {
    pwm: Mmio,
    pinmux: Mmio,
    gpio: Mmio,
    pmc: Mmio,
    erista: bool,
    tach: Option<Irq>,
    duty: u32,
    rail: bool,
    /// Other users of the 5V rail (`proto::RAIL_*` bits).
    users: u64,
}

impl Fan {
    fn gpio_set(&self, reg: usize, p: usize, pins: u32, value: u32) {
        self.gpio.modify(port(reg, p), pins & !value, pins & value);
        let _ = self.gpio.r32(port(reg, p));
    }

    fn output(&self, p: usize, pin: u32, high: bool) {
        self.gpio_set(GPIO_CNF, p, pin, pin);
        self.gpio_set(GPIO_OUT, p, pin, if high { pin } else { 0 });
        self.gpio_set(GPIO_OE, p, pin, pin);
    }

    /// hekate's first `fan_set_duty`: tachometer input (Mariko has an
    /// external pull-up), PWM1 stopped and driving pad V4.
    fn start(&mut self) {
        let pull = if self.erista { PULL_UP } else { 0 };
        self.pinmux
            .w32(PINMUX_CAM1_PWDN, TRISTATE | INPUT_ENABLE | pull | 1);
        self.gpio_set(GPIO_CNF, PORT_S, TACH, TACH);
        self.gpio_set(GPIO_OE, PORT_S, TACH, 0);
        self.pwm.w32(PWM_CSR_1, PWM_STOP);
        self.pinmux.w32(PINMUX_LCD_GPIO2, FUNC1);
        self.gpio_set(GPIO_CNF, PORT_V, PWM_PAD, 0);
        self.set(0);
    }

    /// hekate `regulator_5v_enable`/`_disable`: the battery's 5V
    /// regulator (A5); Erista's USB VBUS source (CC4) stays off.
    fn power(&mut self, on: bool) {
        if on == self.rail {
            return;
        }
        if on {
            self.pinmux.w32(PINMUX_SATA_LED_ACTIVE, FUNC1);
            self.output(PORT_A, REGULATOR_5V, true);
            if self.erista {
                self.pinmux.w32(PINMUX_USB_VBUS_EN0, LPDR | FUNC1);
                self.output(PORT_CC, VBUS_5V, false);
            }
            // Written only if needed: `sd` shares NO_IOPOWER.
            for reg in [PMC_NO_IOPOWER, PMC_PWR_DET_VAL] {
                if self.pmc.r32(reg) & PMC_GPIO != 0 {
                    self.pmc.modify(reg, PMC_GPIO, 0);
                    let _ = self.pmc.r32(reg);
                }
            }
        } else {
            self.gpio_set(GPIO_OUT, PORT_A, REGULATOR_5V, 0);
            if self.erista {
                self.gpio_set(GPIO_OUT, PORT_CC, VBUS_5V, 0);
            }
        }
        self.rail = on;
    }

    /// hekate `fan_set_duty`: stopped, the rail goes off (unless a Joy-Con
    /// needs it) and the pad is parked (pulled down).
    fn set(&mut self, duty: u32) {
        let duty = duty.min(FULL);
        if duty == 0 {
            self.pwm.w32(PWM_CSR_1, PWM_STOP);
            self.power(self.users != 0);
            self.pinmux.w32(
                PINMUX_LCD_GPIO2,
                INPUT_ENABLE | PARKED | TRISTATE | PULL_DOWN,
            );
        } else {
            self.power(true);
            self.pwm.w32(PWM_CSR_1, PWM_EN | (FULL - duty) << 16);
            self.pinmux.w32(PINMUX_LCD_GPIO2, FUNC1);
        }
        self.duty = duty;
    }

    /// Rising edges on S7 for 1 s, counted on the bank's interrupt
    /// (hekate polls the pin for 2 s instead). Returns RPM.
    fn measure(&self) -> Option<u64> {
        let irq = self.tach.as_ref()?;
        let (sta, clr) = (port(GPIO_INT_STA, PORT_S), port(GPIO_INT_CLR, PORT_S));
        // Edge (not level, not both edges), rising.
        self.gpio_set(
            GPIO_INT_LVL,
            PORT_S,
            TACH | TACH << 8 | TACH << 16,
            TACH | TACH << 8,
        );
        self.gpio.w32(clr, TACH);
        self.gpio_set(GPIO_INT_ENB, PORT_S, TACH, TACH);
        let end = time::now_ns() + WINDOW_NS;
        let mut edges = 0;
        loop {
            let left = end.saturating_sub(time::now_ns());
            if left == 0 {
                break;
            }
            if irq.wait(left) {
                if self.gpio.r32(sta) & TACH != 0 {
                    edges += 1;
                    self.gpio.w32(clr, TACH);
                }
                irq.ack();
            }
        }
        self.gpio_set(GPIO_INT_ENB, PORT_S, TACH, 0);
        self.gpio.w32(clr, TACH);
        Some(edges * 60_000_000_000 / (PULSES * WINDOW_NS))
    }
}

/// What the loop decides from: the last reading and the user's choice.
struct Control {
    /// Duty set by hand (steps).
    manual: Option<u32>,
    /// SoC temperature (millidegrees), kept through a few failures.
    temp: Option<i64>,
    fails: u32,
    hot: bool,
    rpm: Option<u64>,
    /// Next speed measurement (0: none planned).
    measure_at: u64,
}

impl Control {
    fn read(&mut self) {
        let t = POWER
            .call_timeout(&Msg::new(power::TEMPERATURE, 0, 0), CALL_NS)
            .map(|m| m.mr[1] as i64);
        match t {
            Ok(t) => (self.temp, self.fails) = (Some(t), 0),
            Err(_) => {
                self.fails += 1;
                if self.fails >= LOST {
                    self.temp = None;
                }
            }
        }
    }

    fn lost(&self) -> bool {
        self.fails >= LOST
    }

    /// Duty for the current reading; `cur` before the first one.
    fn target(&mut self, cur: u32) -> u32 {
        let c = self.temp.map(|t| t.div_euclid(1000));
        self.hot = c.is_some_and(|c| c >= HOT_C || self.hot && c >= COOL_C);
        match (c, self.manual) {
            _ if self.hot => FULL,
            _ if self.lost() => self.manual.unwrap_or(0).max(SAFE),
            (_, Some(m)) => m,
            (Some(c), None) => nyx_duty(c, cur),
            (None, None) => cur,
        }
    }

    fn status(&self, fan: &Fan) -> Msg {
        let mut st = 0;
        if self.manual.is_some() {
            st |= proto::ST_MANUAL;
        }
        if self.hot {
            st |= proto::ST_HOT;
        }
        if self.lost() {
            st |= proto::ST_NO_SENSOR;
        }
        Msg::from_words(
            0,
            &[
                percent(fan.duty),
                st,
                self.temp.map_or(proto::NO_TEMP, |t| t as u64),
                self.rpm.unwrap_or(proto::NO_RPM),
                self.manual.map_or(proto::AUTO, percent),
            ],
        )
    }
}

/// Apply the target; a change plans a speed measurement once settled.
fn step(fan: &mut Fan, ctl: &mut Control) {
    let duty = ctl.target(fan.duty);
    if duty != fan.duty {
        fan.set(duty);
        ctl.rpm = None;
        ctl.measure_at = time::now_ns() + SETTLE_NS;
    }
}

fn request(fan: &mut Fan, ctl: &mut Control, m: &Msg) -> Msg {
    match m.label() {
        proto::STATUS => ctl.status(fan),
        proto::SET => {
            ctl.manual = match m.mr[0] {
                proto::AUTO => None,
                p @ 0..=100 => Some((p * FULL as u64 / 100) as u32),
                _ => return Msg::error(-EINVAL),
            };
            step(fan, ctl);
            ctl.status(fan)
        }
        proto::MEASURE => match fan.measure() {
            Some(rpm) => {
                ctl.rpm = Some(rpm);
                Msg::from_words(0, &[rpm])
            }
            None => Msg::error(-ENODEV),
        },
        proto::RAIL => {
            let bit = m.mr[0];
            if !matches!(bit, proto::RAIL_JC_R | proto::RAIL_JC_L) {
                return Msg::error(-EINVAL);
            }
            if m.mr[1] != 0 {
                fan.users |= bit;
            } else {
                fan.users &= !bit;
            }
            fan.power(fan.duty != 0 || fan.users != 0);
            Msg::from_words(0, &[fan.users])
        }
        _ => Msg::error(-EINVAL),
    }
}

/// The PWM block clocked (`clk` never resets it if the backlight runs
/// on channel 0): touching an unclocked block hangs the bus.
fn pwm_clock() -> Result<(), i64> {
    libnx::clk::enable(clk::PWM)?;
    if libnx::clk::ready(clk::PWM) {
        Ok(())
    } else {
        Err(-ENODEV)
    }
}

fn open() -> Result<Fan, i64> {
    let map = |k, base| Mmio::map(drv::dev_slot(k), base, 0x1000);
    let erista = (map(4, APB_MISC)?.r32(GP_HIDREV) >> 4) & 0xF < 2;
    pwm_clock()?;
    let (pwm, pinmux, gpio, pmc) = (
        map(0, PWM)?,
        map(1, PINMUX)?,
        map(2, GPIO)?,
        map(3, PMC_PAGE)?,
    );
    let mut fan = Fan {
        pwm,
        pinmux,
        gpio,
        pmc,
        erista,
        tach: Irq::new(drv::dev_slot(5)).ok(),
        duty: 0,
        rail: true,
        users: 0,
    };
    fan.start();
    Ok(fan)
}

libnx::entry!(main);

fn main() -> i32 {
    let mut fan = match open() {
        Ok(f) => Some(f),
        Err(e) => {
            println!("[fan] no fan control ({})", err::name(e));
            None
        }
    };
    let _ = ns::wait_for_timeout("power", 5_000_000_000);
    let mut ctl = Control {
        manual: None,
        temp: None,
        fails: 0,
        hot: false,
        rpm: None,
        measure_at: 0,
    };
    if let Some(f) = fan.as_mut() {
        ctl.read();
        step(f, &mut ctl);
        match ctl.temp {
            Some(t) => println!(
                "[fan] ready: SoC {}.{} C, duty {}%",
                t / 1000,
                t % 1000 / 100,
                percent(f.duty)
            ),
            None => println!("[fan] ready, no temperature yet"),
        }
    }
    let ep = drv::publish("fan").expect("fan: publish");
    let mut tick = time::now_ns() + PERIOD_NS;
    loop {
        // Without the hardware, only answer (ENODEV).
        let wait = match (&fan, ctl.measure_at) {
            (None, _) => 0,
            (Some(_), 0) => tick,
            (Some(_), m) => tick.min(m),
        };
        let wait = match wait {
            0 => 0,
            t => t.saturating_sub(time::now_ns()).max(1),
        };
        match ipc::recv_timeout(ep, wait) {
            Ok(rx) => {
                let reply = match fan.as_mut() {
                    Some(f) => request(f, &mut ctl, &rx.msg),
                    None => Msg::error(-ENODEV),
                };
                let _ = ipc::reply(&reply);
            }
            Err(e) if e != -ETIMEDOUT => time::sleep_ms(10),
            Err(_) => {}
        }
        let Some(f) = fan.as_mut() else { continue };
        let now = time::now_ns();
        if now >= tick {
            ctl.read();
            step(f, &mut ctl);
            tick = now + PERIOD_NS;
        }
        if ctl.measure_at != 0 && now >= ctl.measure_at {
            ctl.rpm = f.measure();
            ctl.measure_at = match f.duty {
                0 => 0,
                _ => time::now_ns() + RECHECK_NS,
            };
        }
    }
}

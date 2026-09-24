//! input: touchscreen and keyboard events. The touch controller is
//! powered, reset and set sensing at start (hekate leaves it off), then
//! read when its interrupt line (GPIO X1) says it has events; without the
//! line, polled while a client waits (100 Hz with a finger down, 25 Hz
//! for 3 s after, then 2 Hz). Clients wait via READ, each for the kinds
//! it asks; drivers inject via INJECT.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ETIMEDOUT};
use libnx::abi::proto::{input as proto, power};
use libnx::drv::{self, Irq, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg, Received};
use libnx::{ns, println, sys, time};

/// STM FingerTip S (FTM4CD60DA1BE / FTM4CD50TA1BE) on I2C3, commands as
/// hekate bdk/input/touch.h. Reads use a repeated start: after a stop,
/// the chip sends an extra byte first.
const TOUCH_BUS: u8 = 3;
const TOUCH_ADDR: u8 = 0x49;
const READ_ONE_EVENT: u8 = 0x85;
const MS_MT_SENSE_ON: u8 = 0x93;
const CLEAR_EVENT_STACK: u8 = 0xA1;
const SWITCH_SENSE_MODE: u8 = 0xC3;
const FINGER_MODE: u8 = 0x01;
/// Hardware register access: 0xB6, 16-bit address; reads start with a
/// dummy byte.
const HW_REG: u8 = 0xB6;
const REG_CHIP_ID: u8 = 0x04;
/// Pending event count (x2).
const REG_EVENT_COUNT: u8 = 0x23;
const REG_SYS_RESET: u8 = 0x28;
const EV_CONTROLLER_READY: u8 = 0x10;
const POLL_NS: u64 = 10_000_000;
/// No finger down, soon after a contact.
const POLL_NEAR_NS: u64 = 40_000_000;
const NEAR_NS: u64 = 3_000_000_000;
/// Long without contact: the controller keeps its events, a quick tap
/// is still seen whole.
const POLL_IDLE_NS: u64 = 500_000_000;

/// GPIO registers as hekate bdk/soc/gpio.c: bank of 4 ports every 0x100,
/// 4 bytes per port; masked writes set the bits of their high byte to
/// their low byte.
const GPIO: u64 = 0x6000_D000;
const PINMUX: u64 = 0x7000_3000;
const fn port(p: usize, reg: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}
const PORT_J: usize = 9;
const PORT_X: usize = 23;
const GPIO_INT_LVL: usize = port(PORT_X, 0x60);
const GPIO_INT_CLR: usize = port(PORT_X, 0x70);
const GPIO_CNF_MSK: usize = 0x80;
const GPIO_OE_MSK: usize = 0x90;
const GPIO_OUT_MSK: usize = 0xA0;
const GPIO_INT_ENB_MSK: usize = port(PORT_X, 0xD0);
/// Touch controller interrupt: GPIO X1, active low (Switch device tree).
const PIN: u32 = 1 << 1;
/// Touch controller reset: GPIO J7, active low.
const RST: u32 = 1 << 7;

/// Pads (hekate bdk/soc/pinmux.h; Horizon's Icosa table in Atmosphere
/// pinmux_initial_pad_config_icosa.inc).
const PINMUX_DAP4_SCLK: usize = 0x150; // GPIO J7
const PINMUX_GEN3_I2C_SCL: usize = 0xCC;
const PINMUX_GEN3_I2C_SDA: usize = 0xD0;
const PINMUX_CAM_I2C_SCL: usize = 0xD4;
const PINMUX_CAM_I2C_SDA: usize = 0xD8;
const PINMUX_TOUCH_INT: usize = 0x220; // GPIO X1
const PINMUX_FUNC: u32 = 3;
const PINMUX_PULL_MASK: u32 = 3 << 2;
const PINMUX_PULL_DOWN: u32 = 1 << 2;
const PINMUX_PULL_UP: u32 = 2 << 2;
const PINMUX_TRISTATE: u32 = 1 << 4;
const PINMUX_PARKED: u32 = 1 << 5;
const PINMUX_INPUT_ENABLE: u32 = 1 << 6;
/// Interrupts in a row without an event: the line is stuck, poll instead.
const STUCK: u32 = 64;

/// Write-only command (a stop after it).
fn command(i2c: &I2c, w: &[u8]) -> bool {
    i2c.transfer(TOUCH_BUS, TOUCH_ADDR, w, &mut []).is_ok()
}

fn read(i2c: &I2c, w: &[u8], r: &mut [u8]) -> bool {
    i2c.write_read(TOUCH_BUS, TOUCH_ADDR, w, r).is_ok()
}

fn event_count(i2c: &I2c) -> u8 {
    let mut b = [0u8; 2];
    if read(i2c, &[HW_REG, 0x00, REG_EVENT_COUNT], &mut b) {
        b[1] >> 1
    } else {
        0
    }
}

/// Wait up to `ms` for event `id` (other events are dropped).
fn wait_event(i2c: &I2c, id: u8, ms: u64) -> bool {
    let end = time::now_ns() + ms * 1_000_000;
    loop {
        if event_count(i2c) != 0 {
            let mut ev = [0u8; 8];
            if read(i2c, &[READ_ONE_EVENT], &mut ev) && ev[0] == id && ev[1] == 0 {
                return true;
            }
        }
        if time::now_ns() > end {
            return false;
        }
        time::sleep_us(500);
    }
}

/// Power-up as hekate touch_power_on: hekate turns LDO6 off and holds
/// the reset low before it jumps to us. Rail, I2C3 pads, reset release.
fn touch_power_on(gpio: Mmio, pinmux: Mmio) {
    pinmux.w32(PINMUX_DAP4_SCLK, PINMUX_PULL_DOWN | 1);
    gpio.w32(port(PORT_J, GPIO_CNF_MSK), RST << 8 | RST);
    gpio.w32(port(PORT_J, GPIO_OUT_MSK), RST << 8);
    gpio.w32(port(PORT_J, GPIO_OE_MSK), RST << 8 | RST);
    time::sleep_us(20);
    let rail = ns::wait_for_timeout("power", 2_000_000_000).and_then(|ep| {
        let mut m = Msg::new(power::TOUCH_POWER, 1, 0);
        m.mr[0] = 1;
        ipc::checked(ipc::call(ep, &m)?)
    });
    if let Err(e) = rail {
        println!("[input] touch rail: {}", libnx::abi::err::name(e));
    }
    // I2C3 on the GEN3 pads. The CAM (I2C4) pads mux to I2C3 too at
    // reset, and their SDA is tied to the game card's 1.8 V: detach them.
    pinmux.w32(PINMUX_GEN3_I2C_SCL, PINMUX_INPUT_ENABLE);
    pinmux.w32(PINMUX_GEN3_I2C_SDA, PINMUX_INPUT_ENABLE);
    pinmux.modify(PINMUX_CAM_I2C_SCL, PINMUX_FUNC, 1);
    pinmux.modify(PINMUX_CAM_I2C_SDA, PINMUX_FUNC, 1);
    time::sleep_ms(1);
    gpio.w32(port(PORT_J, GPIO_OUT_MSK), RST << 8 | RST);
    time::sleep_ms(10);
}

/// Reset, then finger sensing (hekate touch_init, touch_sense_enable).
/// None: no answer; else the chip ID and whether the reset was seen.
fn touch_init(i2c: &I2c) -> Option<(u16, bool)> {
    wait_event(i2c, EV_CONTROLLER_READY, 20);
    let mut info = [0u8; 7];
    if !read(i2c, &[HW_REG, 0x00, REG_CHIP_ID], &mut info) {
        return None;
    }
    let chip = u16::from_be_bytes([info[1], info[2]]);
    let reset = (0..3).any(|_| {
        let sent = command(i2c, &[HW_REG, 0x00, REG_SYS_RESET, 0x80]);
        time::sleep_ms(10);
        sent && wait_event(i2c, EV_CONTROLLER_READY, 20)
    });
    let on = command(i2c, &[SWITCH_SENSE_MODE, FINGER_MODE])
        && command(i2c, &[MS_MT_SENSE_ON])
        && command(i2c, &[CLEAR_EVENT_STACK]);
    on.then_some((chip, reset))
}

/// The controller's interrupt line, level low.
struct TouchIrq {
    gpio: Mmio,
    irq: Irq,
}

impl TouchIrq {
    fn new(gpio: Mmio, pinmux: Mmio) -> Option<Self> {
        let irq = Irq::new(drv::dev_slot(2)).ok()?;
        // Input with a pull-up, as Horizon sets it.
        pinmux.modify(
            PINMUX_TOUCH_INT,
            PINMUX_PARKED | PINMUX_PULL_MASK,
            PINMUX_INPUT_ENABLE | PINMUX_TRISTATE | PINMUX_PULL_UP,
        );
        // GPIO (not the pad's function), input; interrupt on level (not
        // edge, not both edges), low.
        gpio.w32(port(PORT_X, GPIO_CNF_MSK), PIN << 8 | PIN);
        gpio.w32(port(PORT_X, GPIO_OE_MSK), PIN << 8);
        gpio.modify(GPIO_INT_LVL, PIN | PIN << 8 | PIN << 16, 0);
        gpio.w32(GPIO_INT_CLR, PIN);
        gpio.w32(GPIO_INT_ENB_MSK, PIN << 8 | PIN);
        sys::notify_bind(irq.notification()).ok()?;
        Some(Self { gpio, irq })
    }

    /// Events read: the status is latched again if the line stays low.
    fn done(&self) {
        self.gpio.w32(GPIO_INT_CLR, PIN);
        self.irq.ack();
    }

    fn off(&self) {
        self.gpio.w32(GPIO_INT_ENB_MSK, PIN << 8);
        self.gpio.w32(GPIO_INT_CLR, PIN);
    }
}

#[derive(Clone, Copy)]
struct Event([u64; 5]);

/// `e` is of a kind in `mask` (`1 << KIND_*`; 0: all).
fn wants(mask: u64, e: &Event) -> bool {
    mask == 0 || e.0[0] < 64 && mask & (1 << e.0[0]) != 0
}

fn reply(e: Event) -> Msg {
    let mut m = Msg::new(0, 5, 0);
    m.mr[..5].copy_from_slice(&e.0);
    m
}

/// Pending events in the touch controller; true if a finger is still
/// down afterwards.
fn poll_touch(i2c: &I2c, out: &mut VecDeque<Event>, mut down: bool) -> bool {
    for _ in 0..event_count(i2c).min(32) {
        let mut ev = [0u8; 8];
        if !read(i2c, &[READ_ONE_EVENT], &mut ev) {
            return down;
        }
        let code = match ev[0] & 0x0F {
            0x03 => proto::TOUCH_DOWN,
            0x05 => proto::TOUCH_MOVE,
            0x04 => proto::TOUCH_UP,
            _ => continue, // other events (controller ready, state...)
        };
        let finger = (ev[0] >> 4) as u64;
        let x = (ev[1] as u64) << 4 | (ev[3] >> 4) as u64;
        let y = (ev[2] as u64) << 4 | (ev[3] & 0xF) as u64;
        down = code != proto::TOUCH_UP;
        out.push_back(Event([proto::KIND_TOUCH, code, finger, x, y]));
    }
    down
}

libnx::entry!(main);

fn main() -> i32 {
    let i2c = I2c::connect().expect("input: i2c service");
    let pads = Mmio::map(drv::dev_slot(0), GPIO, 0x1000)
        .ok()
        .zip(Mmio::map(drv::dev_slot(1), PINMUX, 0x1000).ok());
    if let Some((gpio, pinmux)) = pads {
        touch_power_on(gpio, pinmux);
    }
    let init = (0..3).find_map(|_| touch_init(&i2c));
    let touch = init.is_some();
    let mut line = match pads {
        Some((gpio, pinmux)) if touch => TouchIrq::new(gpio, pinmux),
        _ => None,
    };
    let ep = drv::publish("input").expect("input: publish");
    let how = match line {
        Some(_) => "interrupt on GPIO X1",
        None => "polled",
    };
    match init {
        None => println!("[input] touchscreen missing (no answer on I2C3)"),
        Some((chip, true)) => println!("[input] touchscreen {:04x} ready ({})", chip, how),
        Some((chip, false)) => println!(
            "[input] touchscreen {:04x} ready, reset not acknowledged ({})",
            chip, how
        ),
    }
    let mut stuck = 0;

    let mut queue: VecDeque<Event> = VecDeque::new();
    // Waiting readers: reply token, deadline (0: none), kinds wanted.
    let mut readers: Vec<(u64, u64, u64)> = Vec::new();
    let mut down = false;
    // Last time a finger was seen.
    let mut contact = 0;
    loop {
        // Poll only while a reader waits or a finger is down (else idle);
        // wake up for the nearest reader deadline too.
        let near = time::now_ns() < contact + NEAR_NS;
        let polled = touch && line.is_none();
        let mut wait = match (polled, down, readers.is_empty()) {
            (true, true, _) => POLL_NS,
            (true, false, false) if near => POLL_NEAR_NS,
            (true, false, false) => POLL_IDLE_NS,
            _ => 0,
        };
        if let Some(d) = readers.iter().map(|r| r.1).filter(|&d| d != 0).min() {
            let left = d.saturating_sub(time::now_ns()).max(1);
            wait = if wait == 0 { left } else { wait.min(left) };
        }
        let mut rx: Option<Received> = match ipc::recv_timeout(ep, wait) {
            Ok(r) => Some(r),
            Err(e) if e == -ETIMEDOUT => None,
            Err(_) => None,
        };
        let irq = rx.as_ref().is_some_and(|r| r.msg.is_notify());
        if irq {
            rx = None;
        }
        if polled || irq && line.is_some() {
            let n = queue.len();
            down = poll_touch(&i2c, &mut queue, down);
            let seen = down || queue.len() != n;
            if seen {
                contact = time::now_ns();
            }
            if let Some(l) = line.as_ref().filter(|_| irq) {
                stuck = if seen { 0 } else { stuck + 1 };
                if stuck < STUCK {
                    l.done();
                } else {
                    l.off();
                    line = None;
                    println!("[input] touch interrupt stuck low, polling instead");
                }
            }
        }
        if let Some(rx) = rx {
            match rx.msg.label() {
                proto::READ => {
                    let mask = if rx.msg.words() >= 2 { rx.msg.mr[1] } else { 0 };
                    match queue.iter().position(|e| wants(mask, e)) {
                        Some(i) => {
                            let e = queue.remove(i).unwrap();
                            let _ = ipc::reply(&reply(e));
                        }
                        None => {
                            let t = rx.msg.mr[0];
                            let deadline = if t == 0 { 0 } else { time::now_ns() + t };
                            readers.push((rx.token, deadline, mask));
                        }
                    }
                }
                proto::INJECT => {
                    let mut e = [0u64; 5];
                    e.copy_from_slice(&rx.msg.mr[..5]);
                    queue.push_back(Event(e));
                    let _ = ipc::reply(&Msg::new(0, 0, 0));
                }
                _ => {
                    let _ = ipc::reply(&Msg::error(-EINVAL));
                }
            }
        }
        // Deliver to waiting readers (oldest first, each its kinds), then
        // expired deadlines.
        let mut k = 0;
        while k < readers.len() {
            let (tok, _, mask) = readers[k];
            match queue.iter().position(|e| wants(mask, e)) {
                Some(i) => {
                    let e = queue.remove(i).unwrap();
                    readers.remove(k);
                    let _ = ipc::reply_to(tok, &reply(e));
                }
                None => k += 1,
            }
        }
        let now = time::now_ns();
        readers.retain(|&(tok, d, _)| {
            if d != 0 && now >= d {
                let _ = ipc::reply_to(tok, &Msg::error(-ETIMEDOUT));
                false
            } else {
                true
            }
        });
        // Events with no reader: keep only a bounded number.
        while queue.len() > 256 {
            queue.pop_front();
        }
    }
}

//! input: touchscreen and keyboard events. The touch controller is read
//! when its interrupt line (GPIO X1) says it has events; without the
//! line, polled while a client waits (100 Hz with a finger down, 25 Hz
//! for 3 s after, then 2 Hz). Clients wait via READ, each for the kinds
//! it asks; drivers inject via INJECT.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::err::{self, EINVAL, EIO, ETIMEDOUT};
use libnx::abi::proto::{input as proto, power};
use libnx::drv::{self, Irq, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg, Received};
use libnx::{ns, println, sys, time};

/// STMicro FingerTip S controller on I2C3, as hekate bdk/input/touch.c.
const TOUCH_BUS: u8 = 3;
const TOUCH_ADDR: u8 = 0x49;
const READ_ONE_EVENT: u8 = 0x85;
const MS_MT_SENSE_ON: u8 = 0x93;
const CLEAR_EVENT_STACK: u8 = 0xA1;
const SWITCH_SENSE_MODE: u8 = 0xC3;
const FINGER_MODE: u8 = 0x01;
/// Hardware registers (u16 big-endian address, reads lead with a dummy
/// byte): 0x0004 chip ID and firmware, 0x0023 event count (x2), 0x0028
/// system reset.
const HW_REG: u8 = 0xB6;
const EV_CONTROLLER_READY: u8 = 0x10;
const CHIP_ID: u16 = 0x3670;
const POLL_NS: u64 = 10_000_000;
/// No finger down, soon after a contact.
const POLL_NEAR_NS: u64 = 40_000_000;
const NEAR_NS: u64 = 3_000_000_000;
/// Long without contact: the controller keeps its events, a quick tap
/// is still seen whole.
const POLL_IDLE_NS: u64 = 500_000_000;

/// GPIO registers as hekate bdk/soc/gpio.c: 4 ports per bank (0x100
/// each), 4 bytes per port; masked writes set the bits of their high
/// byte to their low byte.
const GPIO: u64 = 0x6000_D000;
const PINMUX: u64 = 0x7000_3000;
const fn port(p: usize, reg: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}
const PORT_J: usize = 9;
const PORT_X: usize = 23;
const GPIO_INT_LVL: usize = 0x60;
const GPIO_INT_CLR: usize = 0x70;
const GPIO_CNF_MSK: usize = 0x80;
const GPIO_OE_MSK: usize = 0x90;
const GPIO_OUT_MSK: usize = 0xA0;
const GPIO_INT_ENB_MSK: usize = 0xD0;
/// Touch controller interrupt: GPIO X1, active low (Switch device tree,
/// tegra210-nintendo-switch.dts).
const PIN: u32 = 1 << 1;
/// Touch controller reset: GPIO J7 (pad DAP4_SCLK), active low.
const RESET: u32 = 1 << 7;
/// Pinmux (hekate bdk/soc/pinmux.h): PINMUX_AUX_GPIO_X1_AUD is parked
/// at reset, its input buffer enabled here.
const PINMUX_X1: usize = 0x18C;
const PINMUX_DAP4_SCLK: usize = 0x150;
/// I2C3 pads, then I2C4's, taken off the I2C3 function.
const PINMUX_I2C3: [usize; 2] = [0xCC, 0xD0];
const PINMUX_I2C4: [usize; 2] = [0xD4, 0xD8];
const PINMUX_PULL_DOWN: u32 = 1 << 2;
const PINMUX_PARKED: u32 = 1 << 5;
const PINMUX_INPUT_ENABLE: u32 = 1 << 6;
/// Interrupts in a row without an event: the line is stuck, poll instead.
const STUCK: u32 = 64;

/// Command `cmd` then a read, one transaction as the controller wants.
fn touch_read(i2c: &I2c, cmd: &[u8], out: &mut [u8]) -> Result<(), i64> {
    i2c.write_read(TOUCH_BUS, TOUCH_ADDR, cmd, out)
}

fn touch_cmd(i2c: &I2c, cmd: &[u8]) -> Result<(), i64> {
    i2c.transfer(TOUCH_BUS, TOUCH_ADDR, cmd, &mut [])
}

fn event_count(i2c: &I2c) -> Result<u8, i64> {
    let mut c = [0u8; 2];
    touch_read(i2c, &[HW_REG, 0x00, 0x23], &mut c)?;
    Ok(c[1] >> 1)
}

/// Wait at most `ms` for event `id` with status `status`, dropping the
/// others (hekate `_touch_wait_event`).
fn wait_event(i2c: &I2c, id: u8, status: u8, ms: u64) -> bool {
    let end = time::now_ns() + ms * 1_000_000;
    loop {
        if event_count(i2c).unwrap_or(0) > 0 {
            let mut ev = [0u8; 8];
            if touch_read(i2c, &[READ_ONE_EVENT], &mut ev).is_ok() && ev[..2] == [id, status] {
                return true;
            }
        }
        if time::now_ns() > end {
            return false;
        }
        time::sleep_us(500);
    }
}

/// hekate `touch_power_on`: Nyx powers the panel off (`hw_deinit`)
/// before chaining to nxk. Reset held while LDO6 and I2C3 come up.
fn power_on(gpio: &Mmio, pinmux: &Mmio) -> Result<(), i64> {
    pinmux.w32(PINMUX_DAP4_SCLK, PINMUX_PULL_DOWN | 1);
    gpio.w32(port(PORT_J, GPIO_CNF_MSK), RESET << 8 | RESET);
    gpio.w32(port(PORT_J, GPIO_OUT_MSK), RESET << 8);
    gpio.w32(port(PORT_J, GPIO_OE_MSK), RESET << 8 | RESET);
    time::sleep_us(20);
    let ep = ns::wait_for_timeout("power", 2_000_000_000)?;
    let mut m = Msg::new(power::TOUCH_POWER, 1, 0);
    m.mr[0] = 1;
    ipc::checked(ipc::call(ep, &m)?)?;
    for p in PINMUX_I2C3 {
        pinmux.w32(p, PINMUX_INPUT_ENABLE);
    }
    for p in PINMUX_I2C4 {
        pinmux.modify(p, 0, 1);
    }
    time::sleep_ms(1);
    gpio.w32(port(PORT_J, GPIO_OUT_MSK), RESET << 8 | RESET);
    time::sleep_ms(10);
    Ok(())
}

/// hekate `touch_init`: system reset, finger mode, sensing on. Returns
/// the chip ID and firmware version.
fn touch_init(i2c: &I2c) -> Result<(u16, u16), i64> {
    let mut info = [0u8; 7];
    touch_read(i2c, &[HW_REG, 0x00, 0x04], &mut info)?;
    let reset = (0..3).any(|_| {
        let sent = touch_cmd(i2c, &[HW_REG, 0x00, 0x28, 0x80]).is_ok();
        time::sleep_ms(10);
        sent && wait_event(i2c, EV_CONTROLLER_READY, 0, 20)
    });
    if !reset {
        return Err(-ETIMEDOUT);
    }
    touch_cmd(i2c, &[SWITCH_SENSE_MODE, FINGER_MODE])?;
    touch_cmd(i2c, &[MS_MT_SENSE_ON])?;
    touch_cmd(i2c, &[CLEAR_EVENT_STACK])?;
    Ok((
        u16::from_be_bytes([info[1], info[2]]),
        u16::from_be_bytes([info[3], info[4]]),
    ))
}

/// Power the panel up and start it, 3 tries (hekate). The emulator's
/// controller has no chip ID: a wrong one is only reported.
fn touch_start(i2c: &I2c, gpio: &Mmio, pinmux: &Mmio) -> Result<(), i64> {
    power_on(gpio, pinmux)?;
    wait_event(i2c, EV_CONTROLLER_READY, 0, 20);
    let mut r = Err(-EIO);
    for _ in 0..3 {
        r = touch_init(i2c);
        if r.is_ok() {
            break;
        }
    }
    let (chip, fw) = r?;
    if chip != CHIP_ID {
        println!(
            "[input] touch controller ID {:#06x}, not {:#06x}",
            chip, CHIP_ID
        );
    } else {
        println!(
            "[input] touch controller {:#06x}, firmware {:#06x}",
            chip, fw
        );
    }
    Ok(())
}

/// The controller's interrupt line, level low.
struct TouchIrq {
    gpio: Mmio,
    irq: Irq,
}

impl TouchIrq {
    fn new(gpio: Mmio, pinmux: &Mmio) -> Option<Self> {
        let irq = Irq::new(drv::dev_slot(2)).ok()?;
        pinmux.modify(PINMUX_X1, PINMUX_PARKED, PINMUX_INPUT_ENABLE);
        // GPIO (not the pad's function), input, interrupt on level (not
        // edge, not both edges) low.
        gpio.w32(port(PORT_X, GPIO_CNF_MSK), PIN << 8 | PIN);
        gpio.w32(port(PORT_X, GPIO_OE_MSK), PIN << 8);
        gpio.modify(port(PORT_X, GPIO_INT_LVL), PIN | PIN << 8 | PIN << 16, 0);
        gpio.w32(port(PORT_X, GPIO_INT_CLR), PIN);
        gpio.w32(port(PORT_X, GPIO_INT_ENB_MSK), PIN << 8 | PIN);
        sys::notify_bind(irq.notification()).ok()?;
        Some(Self { gpio, irq })
    }

    /// Events read: the status is latched again if the line stays low.
    fn done(&self) {
        self.gpio.w32(port(PORT_X, GPIO_INT_CLR), PIN);
        self.irq.ack();
    }

    fn off(&self) {
        self.gpio.w32(port(PORT_X, GPIO_INT_ENB_MSK), PIN << 8);
        self.gpio.w32(port(PORT_X, GPIO_INT_CLR), PIN);
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
    let Ok(count) = event_count(i2c) else {
        return down;
    };
    for _ in 0..count.min(32) {
        let mut ev = [0u8; 8];
        if touch_read(i2c, &[READ_ONE_EVENT], &mut ev).is_err() {
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
    let pads = (
        Mmio::map(drv::dev_slot(0), GPIO, 0x1000),
        Mmio::map(drv::dev_slot(1), PINMUX, 0x1000),
    );
    let started = match pads {
        (Ok(gpio), Ok(pinmux)) => touch_start(&i2c, &gpio, &pinmux).map(|_| (gpio, pinmux)),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    let touch = started.is_ok();
    let mut line = match started {
        Ok((gpio, pinmux)) => TouchIrq::new(gpio, &pinmux),
        Err(e) => {
            drv::report(
                "input",
                &alloc::format!("touchscreen missing ({})", err::name(e)),
            );
            None
        }
    };
    let ep = drv::publish("input").expect("input: publish");
    if touch {
        println!(
            "[input] touchscreen ready ({})",
            if line.is_some() {
                "interrupt on GPIO X1"
            } else {
                "polled"
            }
        );
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

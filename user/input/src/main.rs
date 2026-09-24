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

use libnx::abi::err::{EINVAL, ETIMEDOUT};
use libnx::abi::proto::input as proto;
use libnx::drv::{self, Irq, Mmio};
use libnx::i2c::I2c;
use libnx::ipc::{self, Msg, Received};
use libnx::{println, sys, time};

const TOUCH_ADDR: u8 = 0x49;
const READ_ONE_EVENT: u8 = 0x85;
const MS_MT_SENSE_ON: u8 = 0x93;
/// Register read: 0x0023 = pending event count (x2).
const READ_REG: u8 = 0xB6;
const POLL_NS: u64 = 10_000_000;
/// No finger down, soon after a contact.
const POLL_NEAR_NS: u64 = 40_000_000;
const NEAR_NS: u64 = 3_000_000_000;
/// Long without contact: the controller keeps its events, a quick tap
/// is still seen whole.
const POLL_IDLE_NS: u64 = 500_000_000;

/// Touch controller interrupt: GPIO X1, active low (Switch device tree,
/// tegra210-nintendo-switch.dts). Registers as hekate bdk/soc/gpio.c:
/// port 23, bank 5 (0x100 each), 4 bytes per port; masked writes set
/// the bits of their high byte to their low byte.
const GPIO: u64 = 0x6000_D000;
const PINMUX: u64 = 0x7000_3000;
const fn port_x(reg: usize) -> usize {
    reg + (23 >> 2) * 0x100 + (23 & 3) * 4
}
const GPIO_INT_LVL: usize = port_x(0x60);
const GPIO_INT_CLR: usize = port_x(0x70);
const GPIO_CNF_MSK: usize = port_x(0x80);
const GPIO_OE_MSK: usize = port_x(0x90);
const GPIO_INT_ENB_MSK: usize = port_x(0xD0);
const PIN: u32 = 1 << 1;
/// PINMUX_AUX_GPIO_X1_AUD (hekate bdk/soc/pinmux.h): parked at reset,
/// its input buffer enabled here.
const PINMUX_X1: usize = 0x18C;
const PINMUX_PARKED: u32 = 1 << 5;
const PINMUX_INPUT_ENABLE: u32 = 1 << 6;
/// Interrupts in a row without an event: the line is stuck, poll instead.
const STUCK: u32 = 64;

/// The controller's interrupt line, level low.
struct TouchIrq {
    gpio: Mmio,
    irq: Irq,
}

impl TouchIrq {
    fn new() -> Option<Self> {
        let gpio = Mmio::map(drv::dev_slot(0), GPIO, 0x1000).ok()?;
        let pinmux = Mmio::map(drv::dev_slot(1), PINMUX, 0x1000).ok()?;
        let irq = Irq::new(drv::dev_slot(2)).ok()?;
        pinmux.modify(PINMUX_X1, PINMUX_PARKED, PINMUX_INPUT_ENABLE);
        gpio.w32(GPIO_CNF_MSK, PIN << 8 | PIN); // GPIO, not the pad's function
        gpio.w32(GPIO_OE_MSK, PIN << 8); // input
                                         // Level (not edge, not both edges), low.
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
    let mut count = [0u8; 2];
    if i2c
        .transfer(3, TOUCH_ADDR, &[READ_REG, 0x00, 0x23], &mut count)
        .is_err()
    {
        return down;
    }
    for _ in 0..(count[1] >> 1).min(32) {
        let mut ev = [0u8; 8];
        if i2c
            .transfer(3, TOUCH_ADDR, &[READ_ONE_EVENT], &mut ev)
            .is_err()
        {
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
    let touch = i2c
        .transfer(3, TOUCH_ADDR, &[MS_MT_SENSE_ON], &mut [])
        .is_ok();
    let mut line = if touch { TouchIrq::new() } else { None };
    let ep = drv::publish("input").expect("input: publish");
    println!(
        "[input] touchscreen {}",
        match (touch, &line) {
            (false, _) => "missing",
            (true, Some(_)) => "ready (interrupt on GPIO X1)",
            (true, None) => "ready (polled)",
        }
    );
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

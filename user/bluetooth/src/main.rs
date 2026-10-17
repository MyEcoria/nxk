//! bluetooth: the BCM4356's Bluetooth half, published as "bt". The chip
//! sits on UART-D (HCI over H4, RTS/CTS) with BT_RST on GPIO H4 (Switch
//! device tree `bt_rst_ph4`, Atmosphere's `BtRst`: the same pins on every
//! board). Joy-Cons and Pro Controllers paired with their sync button
//! (`bt pair`) reconnect on a button press; their buttons and sticks go
//! to `input` like those of the Joy-Cons on the rails.

#![no_std]
#![no_main]

extern crate alloc;

mod bcm;
mod hci;
mod host;
mod store;
mod uart;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{self, EBUSY, EINVAL, ENETDOWN, ETIMEDOUT};
use libnx::abi::proto::{bt, clk, input};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::ns::Service;
use libnx::{ns, println, sys, time};

use hci::Hci;
use host::Host;
use store::{addr_word, word_addr, Store};
use uart::Uart;

const UART_PAGE: u64 = 0x7000_6000;
const UARTD: usize = 0x300;
const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
/// UART-D interrupt: SPI 90.
const IRQ_SLOT: u32 = 3;

/// Pinmux (Atmosphere `pinmux_pad_characters.inc`, icosa initial config):
/// UART4 pads on their UARTD function, inputs pulled down; the wake and
/// reset lines as GPIOs.
const PM_UART4_TX: usize = 0x114;
const PM_UART4_RX: usize = 0x118;
const PM_UART4_RTS: usize = 0x11C;
const PM_UART4_CTS: usize = 0x120;
const PM_AP_WAKE_BT: usize = 0x1C0;
const PM_BT_RST: usize = 0x1C4;
const PM_BT_WAKE_AP: usize = 0x1C8;
const PM_LOW_BITS: u32 = 0x7F;
const PM_PULL_DOWN: u32 = 1 << 2;
const PM_INPUT_ENABLE: u32 = 1 << 6;

const PORT_H: usize = 7;
const AP_WAKE_BT: u32 = 1 << 3;
const BT_RST: u32 = 1 << 4;
const BT_WAKE_AP: u32 = 1 << 5;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const GPIO_IN: usize = 0x30;
const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}

/// Off long enough for the chip's regulators to drop, then the boot of
/// its ROM (Linux `hci_bcm` waits on the same order).
const POWER_OFF_MS: u64 = 50;
const POWER_ON_MS: u64 = 200;
/// The firmware is on the card: wait for `fs` (mounted after `sd`).
const FS_WAIT_NS: u64 = 30_000_000_000;
/// Checks while something is under way (pairing, a controller being
/// set up), and the safety net otherwise: reports come by interrupt.
const BUSY_NS: u64 = 20_000_000;
const IDLE_NS: u64 = 200_000_000;

static INPUT: Service = Service::new("input", 10_000_000_000);

struct Pads {
    gpio: Mmio,
    pinmux: Mmio,
}

impl Pads {
    fn set(&self, reg: usize, pin: u32, high: bool) {
        self.gpio.modify(
            port(reg, PORT_H),
            if high { 0 } else { pin },
            if high { pin } else { 0 },
        );
        let _ = self.gpio.r32(port(reg, PORT_H));
    }

    fn setup(&self) {
        let pm = |off, v| self.pinmux.modify(off, PM_LOW_BITS, v);
        pm(PM_UART4_TX, 0);
        pm(PM_UART4_RX, PM_INPUT_ENABLE | PM_PULL_DOWN);
        pm(PM_UART4_RTS, 0);
        pm(PM_UART4_CTS, PM_INPUT_ENABLE | PM_PULL_DOWN);
        // Input buffers on too: GPIO_IN then reads the level driven.
        pm(PM_AP_WAKE_BT, PM_INPUT_ENABLE);
        pm(PM_BT_RST, PM_INPUT_ENABLE);
        pm(PM_BT_WAKE_AP, PM_INPUT_ENABLE);
        // Wake line: device-wakeup, active high (p2180 / smaug device
        // trees, the same H3 / H4 / H5 wiring); Linux hci_bcm raises it
        // with the power. The chip's host wake is an input.
        self.set(GPIO_OUT, AP_WAKE_BT, false);
        self.set(GPIO_CNF, AP_WAKE_BT, true);
        self.set(GPIO_OE, AP_WAKE_BT, true);
        self.set(GPIO_CNF, BT_WAKE_AP, true);
        self.set(GPIO_OE, BT_WAKE_AP, false);
        self.set(GPIO_CNF, BT_RST, true);
        self.set(GPIO_OE, BT_RST, true);
    }

    /// Pinmux of the UART and control pads, GPIO H CNF / OE / OUT / IN.
    fn regs(&self) -> ([u32; 7], [u32; 4]) {
        let pm = [
            PM_UART4_TX,
            PM_UART4_RX,
            PM_UART4_RTS,
            PM_UART4_CTS,
            PM_AP_WAKE_BT,
            PM_BT_RST,
            PM_BT_WAKE_AP,
        ]
        .map(|o| self.pinmux.r32(o));
        let g = [GPIO_CNF, GPIO_OE, GPIO_OUT, GPIO_IN].map(|r| self.gpio.r32(port(r, PORT_H)));
        (pm, g)
    }

    /// RX and CTS pad levels (PI5, PI7) read as GPIO inputs for a moment:
    /// an idle chip holds its TX high, a ready one its RTS low.
    fn line_levels(&self) -> u32 {
        const PORT_I: usize = 8;
        const PINS: u32 = (1 << 5) | (1 << 7);
        let g = &self.gpio;
        g.modify(port(GPIO_OE, PORT_I), PINS, 0);
        g.modify(port(GPIO_CNF, PORT_I), 0, PINS);
        time::sleep_us(50);
        let v = g.r32(port(GPIO_IN, PORT_I));
        g.modify(port(GPIO_CNF, PORT_I), PINS, 0);
        v
    }

    /// BT_RST: low holds the Bluetooth core off (Wi-Fi has its own line).
    fn power(&self, on: bool) {
        // BT_WAKE is active low for the patched firmware (Erista).
        self.set(GPIO_OUT, AP_WAKE_BT, false);
        self.set(GPIO_OUT, BT_RST, on);
    }
}

enum Adapter {
    Down(Box<Hci>, Store, String),
    Up(Box<Host>),
}

/// Power cycle and bring-up; on failure the core is left off.
fn start(mut hci: Hci, mut store: Store, pads: &Pads) -> Adapter {
    hci.uart.irq_off();
    pads.power(false);
    time::sleep_ms(POWER_OFF_MS);
    pads.power(true);
    time::sleep_ms(POWER_ON_MS);
    let addr = store.local_addr();
    match bcm::bring_up(&mut hci, addr, &|on| pads.set(GPIO_OUT, AP_WAKE_BT, on)) {
        Ok(chip) => {
            println!("[bt] ready: {}", chip.describe());
            hci.uart.irq_on();
            Adapter::Up(Box::new(Host::new(hci, chip, store)))
        }
        Err(why) => {
            // What the pins and the UART look like, for a run on hardware.
            let (pm, g) = pads.regs();
            let lines = pads.line_levels();
            // The 32 kHz clock: MAX77620 CNFG1_32K, AME_GPIO, GPIO4 (I2C5).
            let pmic: Vec<i64> = libnx::i2c::I2c::connect()
                .map(|i| {
                    [0x03u8, 0x40, 0x3A]
                        .iter()
                        .map(|&r| i.read_reg(5, 0x3C, r).map_or(-1, |v| v as i64))
                        .collect()
                })
                .unwrap_or_default();
            let why = alloc::format!(
                "{} [port I in {:#x} (RX bit 5, CTS bit 7); pmic 32k/ame/gpio4 {:x?}; uart lcr/mcr/lsr/msr/ier/irda {:x?}, rx bytes {} (first {:02x?}), resyncs {}, tx stalls {}, errors {}; pinmux tx/rx/rts/cts/wake/rst/hostwake {:x?}; gpio H cnf/oe/out/in {:x?}]",
                why,
                lines,
                pmic,
                hci.uart.regs(),
                hci.rx_seen,
                hci.rx_log,
                hci.resyncs,
                hci.tx_stalls,
                hci.uart.errors,
                pm,
                g
            );
            println!("[bt] no adapter: {}", why);
            pads.power(false);
            Adapter::Down(Box::new(hci), store, why)
        }
    }
}

fn restart(a: Adapter, pads: &Pads) -> Adapter {
    match a {
        Adapter::Down(hci, store, _) => start(*hci, store, pads),
        Adapter::Up(h) => {
            let h = *h;
            start(h.hci, h.store, pads)
        }
    }
}

fn status(a: &Adapter) -> Msg {
    let (st, conn, paired, addr, text) = match a {
        Adapter::Down(_, s, why) => (bt::ST_DOWN, 0, s.pads.len(), 0, why.clone()),
        Adapter::Up(h) => (
            if h.pairing.is_some() {
                bt::ST_PAIRING
            } else {
                bt::ST_READY
            },
            h.connected(),
            h.store.pads.len(),
            addr_word(&h.chip.addr),
            h.describe(),
        ),
    };
    ipc::pack_at(0, &[st, conn as u64, paired as u64, addr], text.as_bytes())
}

fn list(a: &Adapter) -> Msg {
    let pads: Vec<_> = match a {
        Adapter::Down(_, s, _) => s
            .pads
            .iter()
            .map(|p| (p.addr, p.kind, false, bt::BATTERY_UNKNOWN, p.name.clone()))
            .collect(),
        Adapter::Up(h) => h.list(),
    };
    let mut b = Vec::new();
    let mut n = 0;
    for (addr, kind, conn, battery, name) in pads {
        let name = &name.as_bytes()[..name.len().min(bt::MAX_NAME)];
        if b.len() + bt::RECORD + name.len() > ipc::MAX_BYTES - 64 {
            break;
        }
        b.extend_from_slice(&addr);
        b.extend_from_slice(&[kind, conn as u8, battery, name.len() as u8]);
        b.extend_from_slice(name);
        n += 1;
    }
    ipc::pack_at(0, &[n], &b)
}

fn flush(events: &mut Vec<[u64; input::WORDS]>) {
    for e in events.drain(..) {
        let _ = INPUT.call_timeout(&Msg::from_words(input::INJECT, &e), 100_000_000);
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let map = |k, base| Mmio::map(drv::dev_slot(k), base, 0x1000);
    let (Ok(uarts), Ok(pinmux), Ok(gpio)) = (map(0, UART_PAGE), map(1, PINMUX), map(2, GPIO))
    else {
        println!("[bt] no register access");
        return 1;
    };
    let irq = match Irq::new(drv::dev_slot(IRQ_SLOT)) {
        Ok(i) => Some(i),
        Err(e) => {
            println!("[bt] no UART-D interrupt ({}), polling", err::name(e));
            None
        }
    };
    if let Err(e) = libnx::clk::enable(clk::UARTD) {
        println!("[bt] UART-D clock: error {}", err::name(e));
        return 1;
    }
    let pads = Pads { gpio, pinmux };
    pads.setup();
    let ep = match drv::publish("bt") {
        Ok(ep) => ep,
        Err(e) => {
            println!("[bt] cannot publish ({})", err::name(e));
            return 1;
        }
    };
    if let Some(i) = irq.as_ref() {
        let _ = sys::notify_bind(i.notification());
    }
    let _ = ns::wait_for_timeout("fs", FS_WAIT_NS);
    let hci = Hci::new(Uart::new(uarts.offset(UARTD)));
    let mut adapter = start(hci, Store::load(), &pads);

    // `PAIR` caller waiting for the first controller paired.
    let mut waiter: Option<u64> = None;
    loop {
        let mut wait = IDLE_NS;
        if let Adapter::Up(h) = &mut adapter {
            h.poll();
            flush(&mut h.events);
            if let Some(t) = waiter {
                if let Some((a, name)) = h.paired.first() {
                    let _ = ipc::reply_to(t, &ipc::pack_at(0, &[addr_word(a)], name.as_bytes()));
                    waiter = None;
                } else if h.pairing.is_none() {
                    let _ = ipc::reply_to(t, &Msg::error(-ETIMEDOUT));
                    waiter = None;
                }
            }
            if h.busy() {
                wait = BUSY_NS;
            }
        }
        let rx = match ipc::recv_timeout(ep, wait) {
            Ok(rx) => rx,
            Err(_) => continue,
        };
        if rx.msg.is_notify() {
            if let Adapter::Up(h) = &mut adapter {
                h.poll();
                flush(&mut h.events);
            }
            if let Some(i) = irq.as_ref() {
                i.ack();
            }
            continue;
        }
        let word = |k: usize| if rx.msg.words() > k { rx.msg.mr[k] } else { 0 };
        let reply = match (rx.msg.label(), &mut adapter) {
            (bt::STATUS, a) => Some(status(a)),
            (bt::LIST, a) => Some(list(a)),
            (bt::RETRY, _) => {
                if let Some(t) = waiter.take() {
                    let _ = ipc::reply_to(t, &Msg::error(-ETIMEDOUT));
                }
                let a = core::mem::replace(
                    &mut adapter,
                    Adapter::Down(
                        Box::new(Hci::new(Uart::new(uarts.offset(UARTD)))),
                        Store::default(),
                        String::new(),
                    ),
                );
                adapter = restart(a, &pads);
                Some(status(&adapter))
            }
            (bt::FORGET, Adapter::Up(h)) => {
                let w = word(0);
                let n = h.forget((w != 0).then(|| word_addr(w)));
                Some(Msg::from_words(0, &[n as u64]))
            }
            (bt::FORGET, Adapter::Down(_, s, _)) => {
                let w = word(0);
                let n = s.forget((w != 0).then(|| word_addr(w)).as_ref());
                Some(Msg::from_words(0, &[n as u64]))
            }
            (bt::PAIR, Adapter::Down(..)) => Some(Msg::error(-ENETDOWN)),
            (bt::PAIR, Adapter::Up(h)) => {
                let (window, wait_first) = (word(0), word(1) != 0);
                if window == 0 && !wait_first {
                    h.stop_pairing();
                    Some(Msg::new(0, 0, 0))
                } else if wait_first && waiter.is_some() {
                    Some(Msg::error(-EBUSY))
                } else {
                    h.start_pairing(if window == 0 {
                        bt::PAIR_DEFAULT_NS
                    } else {
                        window
                    });
                    if wait_first {
                        waiter = Some(rx.token);
                        None
                    } else {
                        Some(Msg::new(0, 0, 0))
                    }
                }
            }
            _ => Some(Msg::error(-EINVAL)),
        };
        if let Some(r) = reply {
            let _ = ipc::reply_to(rx.token, &r);
        }
    }
}

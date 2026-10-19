//! joycon: the Joy-Cons on the console's rails (handheld mode) as a
//! gamepad, as hekate's `bdk/input/joycon.c` drives them: UART-B (right)
//! and UART-C (left), detected on GPIO H6 / E6, powered from the 5V rail
//! (`fan`), handshaken at 1 Mbaud, switched to 3 Mbaud, polled every
//! 15 ms. Buttons and sticks go to `input` as `KIND_BUTTON` and
//! `KIND_STICK` events. Joy-Cons only run from the rail here: no
//! charging, no Bluetooth pairing data. Asleep (`proto::SUSPEND`): rails
//! released, UART clocks off, pads parked, no polling; `RESUME` handshakes
//! the attached Joy-Cons again as at boot.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::proto::joycon as proto;
use libnx::abi::proto::{clk, fan, input, RESUME, SUSPEND};
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::jc;
use libnx::ns::Service;
use libnx::{println, time};

const UART_PAGE: u64 = 0x7000_6000;
const PINMUX: u64 = 0x7000_3000;
const GPIO: u64 = 0x6000_D000;
const APB_MISC: u64 = 0x7000_0000;
const GP_HIDREV: usize = 0x804;

/// 16550 registers (hekate bdk/soc/uart.h), 4 bytes apart.
const THR: usize = 0x00;
const IER: usize = 0x04;
const FCR: usize = 0x08;
const LCR: usize = 0x0C;
const MCR: usize = 0x10;
const LSR: usize = 0x14;
const SPR: usize = 0x1C;
const IRDA_CSR: usize = 0x20;
const LCR_8N1: u32 = 3;
const LCR_STOP: u32 = 1 << 2;
const LCR_DLAB: u32 = 1 << 7;
const FCR_EN: u32 = 1;
const FCR_RX_CLR: u32 = 1 << 1;
const FCR_TX_CLR: u32 = 1 << 2;
/// Manual RTS: the Joy-Con is held off but while we read.
const MCR_RTS: u32 = 1 << 1;
const LSR_RDR: u32 = 1;
const LSR_THRE: u32 = 1 << 5;
const LSR_TMTY: u32 = 1 << 6;
/// The Joy-Con lines are inverted on TX and RTS.
const INVERT_TXD: u32 = 1 << 1;
const INVERT_RTS: u32 = 1 << 3;
/// Gap that ends a reply.
const RX_GAP_NS: u64 = 250_000;
const RX_PAUSE_NS: u64 = 2_000_000;

/// Pinmux (hekate bdk/soc/pinmux.h).
const PM_INPUT_ENABLE: u32 = 1 << 6;
const PM_TRISTATE: u32 = 1 << 4;
const PM_PULL_DOWN: u32 = 1 << 2;
const PM_DRIVE_4X: u32 = 3 << 13;
/// Rail "is attached" pads, shared with the UART TX nets.
const PM_GPIO_PE6: usize = 0x248;
const PM_GPIO_PH6: usize = 0x250;
/// UART B and C TX pads: pad function UART, or GPIO G0 / D1 to detect.
const PM_UART2_TX: usize = 0xF4;
const PM_UART3_TX: usize = 0x104;
/// Charge enable pads: SPDIF_IN (GPIO CC3, left), GPIO_PK3 (right).
const PM_SPDIF_IN: usize = 0x1A4;
const PM_GPIO_PK3: usize = 0x260;

/// GPIO ports, plain registers (the masked ones did not take on an
/// Erista, as `fan` found).
const PORT_D: usize = 3;
const PORT_E: usize = 4;
const PORT_G: usize = 6;
const PORT_H: usize = 7;
const PORT_K: usize = 10;
const PORT_CC: usize = 28;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;
const GPIO_IN: usize = 0x30;
const fn port(reg: usize, p: usize) -> usize {
    reg + (p >> 2) * 0x100 + (p & 3) * 4
}

/// Rail wire protocol (hekate): UART header, wired commands.
const MAGIC_RX: [u8; 3] = [0x19, 0x81, 0x03];
const WIRED_HID: u8 = 0x92;
const WIRED_INIT_REPLY: u8 = 0x94;
const WIRED_HANDSHAKE: u8 = 0xA5;
const CMD_GET_INFO: u8 = 0x01;
const CMD_HID_CONN: u8 = 0x10;
const CMD_HID_DISC: u8 = 0x11;
const CMD_SET_HIDRATE: u8 = 0x12;
const CMD_SET_BRATE: u8 = 0x20;
const HID_OUTPUT: u8 = 0x01;
const HID_RUMBLE: u8 = 0x10;
const SUBCMD_RUMBLE_CTL: u8 = 0x48;
const SUBCMD_CHARGE_SET: u8 = 0x51;
/// Charger setting: 100 mA off, EN2 off (supply only, no charge).
const CHARGE_SUPPLY: u8 = 0x00;
const ID_L: u8 = 0x01;
const ID_R: u8 = 0x02;
const ID_HORI: u8 = 0x20;

const WAKE: [u8; 4] = [0xA1, 0xA2, 0xA3, 0xA4];
const HANDSHAKE: [u8; 12] = [
    0x19, 0x01, 0x03, 0x07, 0x00, 0xA5, 0x02, 0x01, 0x7E, 0x00, 0x00, 0x00,
];
const GET_INFO: [u8; 12] = [
    0x19, 0x01, 0x03, 0x07, 0x00, 0x91, 0x01, 0x00, 0x00, 0x00, 0x00, 0x24,
];
const SWITCH_BRATE: [u8; 20] = [
    0x19, 0x01, 0x03, 0x0F, 0x00, 0x91, 0x20, 0x08, 0x00, 0x00, 0xBD, 0xB1, 0xC0, 0xC6, 0x2D, 0x00,
    0x00, 0x00, 0x00, 0x00,
];
const HID_DISCONNECT: [u8; 12] = [
    0x19, 0x01, 0x03, 0x07, 0x00, 0x91, 0x11, 0x00, 0x00, 0x00, 0x00, 0x0E,
];
/// Output report rate 15 ms.
const SET_HID_RATE: [u8; 16] = [
    0x19, 0x01, 0x03, 0x0B, 0x00, 0x91, 0x12, 0x04, 0x00, 0x00, 0x12, 0xA6, 0x0F, 0x00, 0x00, 0x00,
];
const HID_CONNECT: [u8; 12] = [
    0x19, 0x01, 0x03, 0x07, 0x00, 0x91, 0x10, 0x00, 0x00, 0x00, 0x00, 0x3D,
];
/// Asks for an input report; the contents do not matter.
const PAD_STATUS: [u8; 13] = [
    0x19, 0x01, 0x03, 0x08, 0x00, 0x92, 0x00, 0x01, 0x00, 0x00, 0x69, 0x2D, 0x1F,
];
const RUMBLE_MUTE: [u8; 8] = [0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40];
const RUMBLE_INIT: [u8; 8] = [0xC2, 0xC8, 0x03, 0x72, 0xC2, 0xC8, 0x03, 0x72];

const POLL_NS: u64 = 15_000_000;
/// Longest wait for the reply to a poll.
const REPLY_NS: u64 = 12_000_000;
/// Silent this long: the Joy-Con is initialized again (hekate: 1800 ms).
const SILENT_NS: u64 = 1_800_000_000;
const RETRY_NS: u64 = 1_000_000_000;
/// Samples of the rail detect lines in a row before a change counts.
const DEBOUNCE: u8 = 3;
const RAIL_REFRESH_NS: u64 = 5_000_000_000;
const STATS_POLLS: u32 = 600;

const UARTS: [u64; 2] = [clk::UARTB, clk::UARTC];

static FAN: Service = Service::new("fan", 10_000_000_000);
static INPUT: Service = Service::new("input", 10_000_000_000);

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Right,
    Left,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Right => "right",
            Side::Left => "left",
        }
    }
    fn clk(self) -> u64 {
        match self {
            Side::Right => clk::UARTB,
            Side::Left => clk::UARTC,
        }
    }
    fn rail(self) -> u64 {
        match self {
            Side::Right => fan::RAIL_JC_R,
            Side::Left => fan::RAIL_JC_L,
        }
    }
    fn pad(self) -> jc::Pad {
        match self {
            Side::Right => jc::Pad::right().source(input::SRC_RAIL_R),
            Side::Left => jc::Pad::left().source(input::SRC_RAIL_L),
        }
    }
}

struct Pads {
    gpio: Mmio,
    pinmux: Mmio,
}

impl Pads {
    fn set(&self, reg: usize, p: usize, pin: u32, high: bool) {
        self.gpio.modify(
            port(reg, p),
            if high { 0 } else { pin },
            if high { pin } else { 0 },
        );
        let _ = self.gpio.r32(port(reg, p));
    }

    fn input(&self, p: usize, pin: u32) {
        self.set(GPIO_CNF, p, pin, true);
        self.set(GPIO_OE, p, pin, false);
    }

    fn output(&self, p: usize, pin: u32, high: bool) {
        self.set(GPIO_CNF, p, pin, true);
        self.set(GPIO_OUT, p, pin, high);
        self.set(GPIO_OE, p, pin, true);
    }

    fn read(&self, p: usize, pin: u32) -> bool {
        self.gpio.r32(port(GPIO_IN, p)) & pin != 0
    }

    /// Pads of the UARTs as hekate `pinmux_config_uart`, and of the
    /// detect lines and charge enables.
    fn setup(&self) {
        for idx in [1, 2] {
            let base = 0xE4 + 0x10 * idx;
            self.pinmux.w32(base, 0);
            self.pinmux.w32(base + 4, PM_INPUT_ENABLE | PM_TRISTATE);
            self.pinmux.w32(base + 8, 0);
            self.pinmux.w32(base + 12, PM_INPUT_ENABLE | PM_TRISTATE);
        }
        self.pinmux.w32(PM_GPIO_PE6, PM_INPUT_ENABLE | PM_TRISTATE);
        self.pinmux.w32(PM_GPIO_PH6, PM_INPUT_ENABLE | PM_TRISTATE);
        self.input(PORT_E, 1 << 6);
        self.input(PORT_H, 1 << 6);
        // Charge enable high, as the loader leaves it with the rail on.
        self.pinmux.w32(PM_SPDIF_IN, PM_PULL_DOWN | 1);
        self.output(PORT_CC, 1 << 3, true);
        self.pinmux.w32(PM_GPIO_PK3, PM_DRIVE_4X | PM_PULL_DOWN | 2);
        self.output(PORT_K, 1 << 3, true);
    }

    /// Asleep: UART lines hi-Z (the Joy-Cons are unpowered, nothing may
    /// drive into them) and charge enables low, as hekate `jc_deinit`.
    fn park(&self) {
        for idx in [1, 2] {
            let base = 0xE4 + 0x10 * idx;
            for off in [0, 4, 8, 12] {
                self.pinmux.w32(base + off, PM_INPUT_ENABLE | PM_TRISTATE);
            }
        }
        self.output(PORT_CC, 1 << 3, false);
        self.output(PORT_K, 1 << 3, false);
    }

    /// Joy-Cons attached (right, left): the TX pads turned into inputs
    /// let the detect lines they share a net with be read (low: there).
    fn detect(&self) -> (bool, bool) {
        self.pinmux.w32(PM_UART2_TX, PM_INPUT_ENABLE);
        self.pinmux.w32(PM_UART3_TX, PM_INPUT_ENABLE);
        self.input(PORT_G, 1 << 0);
        self.input(PORT_D, 1 << 1);
        time::sleep_us(20);
        // The first read unlatches the pad buffers.
        let _ = self.read(PORT_H, 1 << 6);
        let _ = self.read(PORT_E, 1 << 6);
        let r = !self.read(PORT_H, 1 << 6);
        let l = !self.read(PORT_E, 1 << 6);
        self.pinmux.w32(PM_UART2_TX, 0);
        self.pinmux.w32(PM_UART3_TX, 0);
        self.set(GPIO_CNF, PORT_G, 1 << 0, false);
        self.set(GPIO_CNF, PORT_D, 1 << 1, false);
        time::sleep_us(20);
        (r, l)
    }
}

struct Uart {
    m: Mmio,
    dev: u64,
}

impl Uart {
    fn tx_idle(&self) {
        let end = time::now_ns() + 5_000_000;
        while self.m.r32(LSR) & LSR_TMTY == 0 && time::now_ns() < end {}
    }

    /// hekate `uart_init`, manual RTS.
    fn init(&self, baud: u64) -> Result<(), i64> {
        self.tx_idle();
        libnx::clk::uart_baud(self.dev, baud)?;
        let m = &self.m;
        m.w32(IER, 0);
        m.w32(LCR, LCR_DLAB | LCR_8N1);
        m.w32(THR, 1);
        m.w32(IER, 0);
        m.w32(LCR, (if baud > 1_000_000 { LCR_STOP } else { 0 }) | LCR_8N1);
        let _ = m.r32(SPR);
        m.w32(FCR, FCR_EN);
        let _ = m.r32(SPR);
        time::sleep_us(20);
        m.w32(MCR, 0);
        time::sleep_us(96);
        m.w32(FCR, FCR_EN | FCR_TX_CLR | FCR_RX_CLR);
        m.w32(MCR, MCR_RTS);
        time::sleep_us(3 * (baud + 999_999) / baud);
        self.tx_idle();
        self.drain();
        Ok(())
    }

    fn invert(&self) {
        self.m.modify(IRDA_CSR, 0, INVERT_TXD | INVERT_RTS);
        let _ = self.m.r32(SPR);
    }

    fn drain(&self) {
        for _ in 0..64 {
            if self.m.r32(LSR) & LSR_RDR == 0 {
                break;
            }
            let _ = self.m.r32(THR);
        }
    }

    fn send(&self, buf: &[u8]) {
        for &b in buf {
            let end = time::now_ns() + 2_000_000;
            while self.m.r32(LSR) & LSR_THRE == 0 && time::now_ns() < end {}
            self.m.w32(THR, b as u32);
        }
        self.tx_idle();
    }

    /// hekate `uart_recv`: RTS is released while reading, until a gap.
    /// A reply may pause after its first bytes: the gap is longer until
    /// the 24 bytes we parse are in.
    fn recv(&self, buf: &mut [u8]) -> usize {
        self.m.modify(MCR, MCR_RTS, 0);
        let mut n = 0;
        let mut end = time::now_ns() + RX_GAP_NS;
        while n < buf.len() {
            if self.m.r32(LSR) & LSR_RDR == 0 {
                if time::now_ns() > end {
                    break;
                }
                continue;
            }
            buf[n] = self.m.r32(THR) as u8;
            n += 1;
            end = time::now_ns() + if n < 24 { RX_PAUSE_NS } else { RX_GAP_NS };
        }
        self.m.modify(MCR, 0, MCR_RTS);
        n
    }
}

/// A packet from a Joy-Con, whole.
struct Rx {
    buf: [u8; 128],
    len: usize,
}

impl Rx {
    fn read(u: &Uart) -> Option<Self> {
        let mut r = Rx {
            buf: [0; 128],
            len: 0,
        };
        r.len = u.recv(&mut r.buf);
        // A reply cut in two by an earlier read starts mid-packet.
        let at = r.buf[..r.len].windows(3).position(|w| w == MAGIC_RX)?;
        r.buf.copy_within(at..r.len, 0);
        r.len -= at;
        (r.len >= 8).then_some(r)
    }

    fn cmd(&self) -> u8 {
        self.buf[5]
    }

    /// Reply to a wired command: its subcommand's status, if whole.
    fn status(&self, cmd: u8, sub: u8) -> Option<u8> {
        (self.len >= 12 && self.cmd() == cmd && self.buf[6] == sub).then(|| self.buf[9])
    }

    /// First bytes on the console, for bring-up.
    fn dump(&self, side: Side, what: &str) {
        let n = self.len.min(24);
        println!(
            "[joycon] {} {}: {} bytes {:02x?}",
            side.name(),
            what,
            self.len,
            &self.buf[..n]
        );
    }

    fn payload(&self) -> &[u8] {
        &self.buf[12.min(self.len)..self.len]
    }
}

/// An input report (or subcommand reply) in a wired HID packet.
fn report(rx: &Rx) -> Option<jc::Report> {
    if rx.cmd() != WIRED_HID {
        return None;
    }
    jc::parse(rx.payload())
}

struct Jc {
    side: Side,
    uart: Uart,
    /// Rail line state seen, and for how many samples in a row.
    present: bool,
    seen: bool,
    streak: u8,
    ready: bool,
    kind: u8,
    pkt: u8,
    /// Next poll, last valid reply, next initialization attempt.
    next_poll: u64,
    last_rx: u64,
    retry_at: u64,
    fails: u32,
    awaiting: Option<u64>,
    /// This side's buttons and stick.
    pad: jc::Pad,
    announced: bool,
    /// Last battery level read (0..4, `joycon::BATTERY_UNKNOWN`).
    battery: u64,
    polls: u32,
    replies: u32,
    latency_us: u64,
}

impl Jc {
    fn new(side: Side, m: Mmio) -> Self {
        Jc {
            side,
            uart: Uart { m, dev: side.clk() },
            present: false,
            seen: false,
            streak: 0,
            ready: false,
            kind: 0,
            pkt: 0,
            next_poll: 0,
            last_rx: 0,
            retry_at: 0,
            fails: 0,
            awaiting: None,
            pad: side.pad(),
            announced: false,
            battery: proto::BATTERY_UNKNOWN,
            polls: 0,
            replies: 0,
            latency_us: 0,
        }
    }

    /// A HID output report in a wired packet (hekate `_jc_send_hid_output_rpt`).
    fn send_hid(&mut self, report: &[u8]) {
        let mut b = [0u8; 40];
        let n = report.len();
        b[..3].copy_from_slice(&[0x19, 0x01, 0x03]);
        b[3] = (7 + n) as u8;
        b[5] = WIRED_HID;
        b[7] = n as u8;
        b[12..12 + n].copy_from_slice(report);
        b[12 + 1] = self.pkt & 0xF;
        self.pkt = self.pkt.wrapping_add(1);
        self.uart.send(&b[..12 + n]);
    }

    /// A HID subcommand: report 1, packet id, muted rumble, subcommand.
    fn subcmd(&mut self, sub: u8, data: u8, pad_to: usize) {
        let mut r = [0u8; 16];
        r[0] = HID_OUTPUT;
        r[2..10].copy_from_slice(&RUMBLE_MUTE);
        r[10] = sub;
        r[11] = data;
        self.send_hid(&r[..pad_to]);
    }

    fn rumble(&mut self, motors: &[u8; 8]) {
        let mut r = [0u8; 10];
        r[0] = HID_RUMBLE;
        r[2..].copy_from_slice(motors);
        self.send_hid(&r);
    }

    /// Wait `ms`, then read a packet.
    fn exchange(&self, ms: u64) -> Option<Rx> {
        time::sleep_ms(ms);
        Rx::read(&self.uart)
    }

    /// hekate `_jc_conn_init`: handshake at 1 Mbaud, controller info,
    /// 3 Mbaud, HID connection, 15 ms reports. Err: what failed.
    fn init(&mut self) -> Result<(), &'static str> {
        self.uart.init(clk::BAUD_1M).map_err(|_| "uart clock")?;
        self.uart.invert();
        let mut ok = false;
        for _ in 0..10 {
            self.uart.send(&WAKE);
            let _ = Rx::read(&self.uart);
            self.uart.send(&HANDSHAKE);
            if self.exchange(4).is_some_and(|r| r.cmd() == WIRED_HANDSHAKE) {
                ok = true;
                break;
            }
        }
        if !ok {
            return Err("no handshake");
        }
        self.uart.send(&GET_INFO);
        let info = self.exchange(2).ok_or("no controller info")?;
        if info.status(WIRED_INIT_REPLY, CMD_GET_INFO) != Some(0) || info.payload().len() < 7 {
            return Err("bad controller info");
        }
        self.kind = info.payload()[0];
        if self.kind & ID_HORI != 0 || self.kind & (ID_L | ID_R) == 0 {
            return Err("not a Joy-Con");
        }
        if self.kind & (ID_L | ID_R) != if self.side == Side::Left { ID_L } else { ID_R } {
            return Err("Joy-Con on the wrong rail");
        }
        let p = info.payload();
        println!(
            "[joycon] {}: type {:#04x}, address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.side.name(),
            self.kind,
            p[6],
            p[5],
            p[4],
            p[3],
            p[2],
            p[1]
        );
        self.uart.send(&SWITCH_BRATE);
        let fast = self
            .exchange(2)
            .is_some_and(|r| r.status(WIRED_INIT_REPLY, CMD_SET_BRATE) == Some(0));
        if fast {
            self.uart.init(clk::BAUD_3M).map_err(|_| "uart clock")?;
            self.uart.invert();
            // Not connected yet (or still from an earlier boot stage).
            let mut off = false;
            for _ in 0..10 {
                self.uart.send(&HID_DISCONNECT);
                off = self
                    .exchange(5)
                    .is_some_and(|r| r.status(WIRED_INIT_REPLY, CMD_HID_DISC) == Some(0xF));
                if off {
                    break;
                }
            }
            if !off {
                return Err("HID still connected");
            }
        }
        self.uart.send(&HID_CONNECT);
        if self
            .exchange(2)
            .and_then(|r| r.status(WIRED_INIT_REPLY, CMD_HID_CONN))
            != Some(0)
        {
            return Err("no HID connection");
        }
        self.uart.send(&SET_HID_RATE);
        let mut rate = false;
        for i in 0..25 {
            let Some(r) = self.exchange(4) else { continue };
            r.dump(self.side, "rate");
            let _ = i;
            if r.status(WIRED_INIT_REPLY, CMD_SET_HIDRATE).is_some() {
                rate = true;
                break;
            }
        }
        if !rate {
            return Err("no HID rate");
        }
        // Rumble on and a short buzz as the sign of life, then the
        // charger held at supply only.
        self.subcmd(SUBCMD_RUMBLE_CTL, 1, 16);
        let _ = self.exchange(15);
        self.rumble(&RUMBLE_INIT);
        time::sleep_ms(15);
        self.rumble(&RUMBLE_MUTE);
        time::sleep_ms(15);
        self.subcmd(SUBCMD_CHARGE_SET, CHARGE_SUPPLY, 12);
        let _ = self.exchange(15);
        self.ready = true;
        Ok(())
    }

    fn poll(&mut self) {
        self.uart.send(&PAD_STATUS);
        self.awaiting = Some(time::now_ns());
        self.polls += 1;
    }

    /// Back to nothing held and a centred stick.
    fn release(&mut self, out: &mut Vec<[u64; input::WORDS]>) {
        self.pad.release(out);
        self.battery = proto::BATTERY_UNKNOWN;
    }

    /// Detached and idle, as at boot: the next samples attach it again.
    fn reset(&mut self, out: &mut Vec<[u64; input::WORDS]>) {
        self.release(out);
        self.present = false;
        self.seen = false;
        self.streak = 0;
        self.ready = false;
        self.awaiting = None;
        self.retry_at = 0;
        self.fails = 0;
    }

    fn state(&self) -> u64 {
        match (self.present, self.ready) {
            (false, _) => proto::ST_ABSENT,
            (true, false) => proto::ST_ATTACHED,
            (true, true) => proto::ST_READY,
        }
    }

    fn apply(&mut self, r: &jc::Report, out: &mut Vec<[u64; input::WORDS]>) {
        self.pad.apply(r, "joycon", self.side.name(), out);
    }
}

fn set_rail(side: Side, on: bool) -> Result<(), i64> {
    FAN.call(&Msg::from_words(fan::RAIL, &[side.rail(), on as u64]))
        .map(|_| ())
}

fn flush(out: &mut Vec<[u64; input::WORDS]>) {
    for e in out.drain(..) {
        let _ = INPUT.call_timeout(&Msg::from_words(input::INJECT, &e), 100_000_000);
    }
}

/// Sleep: pads and polling stopped, rails released, then the UARTs'
/// clocks gated (their registers must not be touched after).
fn suspend(jcs: &mut [Jc; 2], pads: &Pads, out: &mut Vec<[u64; input::WORDS]>) {
    for jc in jcs.iter_mut() {
        jc.reset(out);
    }
    flush(out);
    for jc in jcs.iter() {
        if let Err(e) = set_rail(jc.side, false) {
            println!("[joycon] {}: 5V rail release: error {}", jc.side.name(), e);
        }
    }
    pads.park();
    for dev in UARTS {
        let _ = libnx::clk::disable(dev);
    }
}

/// Wake: clocks, pads and rails back; detection and handshakes follow
/// from the main loop. Err: the clock that would not start (still asleep).
fn resume(jcs: &[Jc; 2], pads: &Pads) -> Result<(), i64> {
    for dev in UARTS {
        libnx::clk::enable(dev)?;
    }
    pads.setup();
    for jc in jcs {
        if let Err(e) = set_rail(jc.side, true) {
            println!("[joycon] {}: 5V rail: error {}", jc.side.name(), e);
        }
    }
    Ok(())
}

/// One request; `suspended` follows SUSPEND and RESUME (both idempotent).
fn handle(
    label: u32,
    jcs: &mut [Jc; 2],
    pads: &Pads,
    out: &mut Vec<[u64; input::WORDS]>,
    suspended: &mut bool,
) -> Msg {
    match label {
        proto::STATUS => {
            let (r, l) = (&jcs[0], &jcs[1]);
            Msg::from_words(0, &[l.state(), r.state(), l.battery, r.battery])
        }
        SUSPEND => {
            if !*suspended {
                suspend(jcs, pads, out);
                *suspended = true;
            }
            Msg::new(0, 0, 0)
        }
        RESUME if !*suspended => Msg::new(0, 0, 0),
        RESUME => match resume(jcs, pads) {
            Ok(()) => {
                *suspended = false;
                Msg::new(0, 0, 0)
            }
            Err(e) => Msg::error(e),
        },
        _ => Msg::error(-libnx::abi::err::EINVAL),
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let map = |k, base| Mmio::map(drv::dev_slot(k), base, 0x1000);
    let (Ok(uarts), Ok(pinmux), Ok(gpio), Ok(misc)) = (
        map(0, UART_PAGE),
        map(1, PINMUX),
        map(2, GPIO),
        map(3, APB_MISC),
    ) else {
        println!("[joycon] no register access");
        return 1;
    };
    // Rail pins and UART lines as hekate and the loader set them on an
    // Erista; other chips are not checked.
    if (misc.r32(GP_HIDREV) >> 4) & 0xF >= 2 {
        println!("[joycon] not an Erista, the Joy-Con rails are unchecked: off");
        return 0;
    }
    for dev in [clk::UARTB, clk::UARTC] {
        if let Err(e) = libnx::clk::enable(dev) {
            println!("[joycon] UART clock {}: error {}", dev, e);
            return 1;
        }
    }
    let pads = Pads { gpio, pinmux };
    pads.setup();
    let mut jcs = [
        Jc::new(Side::Right, uarts.offset(0x40)),
        Jc::new(Side::Left, uarts.offset(0x200)),
    ];
    for jc in &jcs {
        if let Err(e) = set_rail(jc.side, true) {
            println!("[joycon] {}: 5V rail: error {}", jc.side.name(), e);
        }
    }
    println!("[joycon] rails on, waiting for Joy-Cons");
    let ep = drv::publish("joycon").ok();

    let mut out: Vec<[u64; input::WORDS]> = Vec::new();
    let mut rail_at = time::now_ns() + RAIL_REFRESH_NS;
    let mut raw_last = (false, false);
    let mut suspended = false;
    loop {
        // Asleep: blocked until a request, no wakeups.
        while let (true, Some(ep)) = (suspended, ep) {
            let Ok(rx) = ipc::recv(ep) else {
                time::sleep_ms(10);
                continue;
            };
            let reply = handle(rx.msg.label(), &mut jcs, &pads, &mut out, &mut suspended);
            let _ = ipc::reply_to(rx.token, &reply);
            if !suspended {
                rail_at = time::now_ns() + RAIL_REFRESH_NS;
                raw_last = (false, false);
            }
        }
        let tick = time::now_ns();
        // Attach and detach, after a few samples in agreement.
        let (r, l) = pads.detect();
        if (r, l) != raw_last {
            println!("[joycon] rail lines: right {}, left {}", r, l);
            raw_last = (r, l);
        }
        for (jc, line) in jcs.iter_mut().zip([r, l]) {
            jc.streak = if line == jc.seen {
                jc.streak.saturating_add(1)
            } else {
                1
            };
            jc.seen = line;
            if jc.streak < DEBOUNCE || line == jc.present {
                continue;
            }
            jc.present = line;
            if line {
                println!("[joycon] {}: attached", jc.side.name());
                jc.retry_at = 0;
                jc.fails = 0;
                jc.pad.retrace();
            } else {
                println!("[joycon] {}: detached", jc.side.name());
                jc.ready = false;
                jc.awaiting = None;
                jc.release(&mut out);
            }
        }
        if tick >= rail_at {
            // A restarted `fan` forgets us.
            for jc in &jcs {
                let _ = set_rail(jc.side, true);
            }
            rail_at = tick + RAIL_REFRESH_NS;
        }

        for jc in jcs.iter_mut().filter(|j| j.present) {
            if !jc.ready {
                if tick < jc.retry_at {
                    continue;
                }
                match jc.init() {
                    Ok(()) => {
                        jc.last_rx = time::now_ns();
                        jc.next_poll = 0;
                        jc.polls = 0;
                        jc.replies = 0;
                        jc.latency_us = 0;
                        jc.announced = false;
                        println!(
                            "[joycon] {}: ready, 3 Mbaud, polled every 15 ms",
                            jc.side.name()
                        );
                    }
                    Err(why) => {
                        jc.fails += 1;
                        if jc.fails < 10 || jc.fails % 30 == 0 {
                            println!("[joycon] {}: {} ({} tries)", jc.side.name(), why, jc.fails);
                        }
                        jc.retry_at = time::now_ns() + RETRY_NS;
                    }
                }
                continue;
            }
            if jc.awaiting.is_none() && tick >= jc.next_poll {
                jc.poll();
                jc.next_poll = tick + POLL_NS;
            }
        }

        // Replies, tried every millisecond until each came or the poll
        // period is nearly over.
        let end = time::now_ns() + REPLY_NS;
        while jcs.iter().any(|j| j.awaiting.is_some()) && time::now_ns() < end {
            time::sleep_ms(1);
            for jc in jcs.iter_mut() {
                let Some(sent) = jc.awaiting else { continue };
                if !jc.present {
                    jc.awaiting = None;
                    continue;
                }
                let got = Rx::read(&jc.uart).and_then(|rx| report(&rx));
                if let Some(rep) = got {
                    let now = time::now_ns();
                    jc.latency_us += (now - sent) / 1000;
                    jc.replies += 1;
                    jc.last_rx = now;
                    jc.awaiting = None;
                    jc.battery = (rep.battery as u64 >> 1).min(4);
                    if !jc.announced {
                        jc.announced = true;
                        println!(
                            "[joycon] {}: battery {}/4{}",
                            jc.side.name(),
                            rep.battery >> 1,
                            if rep.battery & 1 != 0 {
                                ", charging"
                            } else {
                                ""
                            }
                        );
                    }
                    jc.apply(&rep, &mut out);
                }
            }
        }
        for jc in jcs.iter_mut() {
            // No reply this period: the silence timer counts it.
            jc.awaiting = None;
            if jc.ready && jc.polls >= STATS_POLLS {
                println!(
                    "[joycon] {}: {} replies to {} polls, {} us on average",
                    jc.side.name(),
                    jc.replies,
                    jc.polls,
                    jc.latency_us / jc.replies.max(1) as u64
                );
                jc.polls = 0;
                jc.replies = 0;
                jc.latency_us = 0;
            }
            if jc.ready && time::now_ns() - jc.last_rx > SILENT_NS {
                println!("[joycon] {}: silent, initializing again", jc.side.name());
                jc.ready = false;
                jc.release(&mut out);
                jc.retry_at = 0;
            }
        }
        flush(&mut out);

        // Until the next poll, STATUS requests (the screen's controller
        // list) are answered.
        let next = tick + POLL_NS;
        loop {
            let now = time::now_ns();
            if now >= next {
                break;
            }
            let Some(ep) = ep else {
                time::sleep_us((next - now) / 1000);
                break;
            };
            let Ok(rx) = ipc::recv_timeout(ep, next - now) else {
                continue;
            };
            let reply = handle(rx.msg.label(), &mut jcs, &pads, &mut out, &mut suspended);
            let _ = ipc::reply_to(rx.token, &reply);
            if suspended {
                break;
            }
        }
    }
}

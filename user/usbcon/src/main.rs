//! usbcon: the USB-C port as a composite device for a PC: a serial
//! console (CDC-ACM) and a network link (CDC-ECM, `usb0`) the PC may
//! share its connection over. Erista's OTG controller in device mode:
//! bytes typed on the PC go to the console as input, all console output
//! goes back to the PC. One thread: console output is pulled only when
//! the IN endpoint is free, so the console's ring is the only buffer.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

mod udc;

use libnx::abi::err::{EAGAIN, EINVAL, ENETDOWN, ETIMEDOUT};
use libnx::abi::proto::{clk, console as con, eth, usbcon as proto};
use libnx::abi::rights::{DUP, GRANT, SEND, SIGNAL};
use libnx::abi::slots;
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::{ns, println, sys, time};

use udc::{bit, Dir, EpType, Udc};

const USB_BASE: u64 = 0x7D00_0000;
const APB_MISC: u64 = 0x7000_0000;
const HIDREV: usize = 0x804;
const FUSE_PAGE: u64 = 0x7000_F000;
/// FUSE_USB_CALIB and FUSE_USB_CALIB_EXT (fuse registers at +0x800).
const FUSE_USB_CALIB: usize = 0x800 + 0x1F0;
const FUSE_USB_CALIB_EXT: usize = 0x800 + 0x350;

/// Notification bits: controller interrupt, console output pending.
const EV_IRQ: u64 = 1;
const EV_TX: u64 = 2;
/// Output held after the host opens the terminal: its tty echoes what
/// arrives before it is switched to raw, and the echo would be typed.
const HOLD_NS: u64 = 100_000_000;

/// DMA block after the descriptors: control, receive, transmit buffers.
const EP0_BUF: usize = 0x400;
const EP0_LEN: usize = 0x200;
const RX_BUF: usize = 0x800;
const RX_LEN: usize = 512;
const TX_BUF: usize = 0x1000;
const TX_LEN: usize = 0x1000;
/// Network function: receive, transmit, notification buffers.
const NET_RX_BUF: usize = 0x2000;
const NET_RX_LEN: usize = 0x800;
const NET_TX_BUF: usize = 0x2800;
const NET_NOTE_BUF: usize = 0x3000;

/// Network function (ECM): interfaces 2 (control) and 3 (data).
const NET_IFACE: u8 = 2;
const NET_DATA: u16 = 3;
const MTU: u64 = 1500;
/// Largest frame, header included.
const FRAME: usize = 1514;
/// Frames held each way.
const NET_QUEUE: usize = 32;
/// Locally administered: the Switch's end of the link, and the PC's
/// (the ECM descriptor gives it to the host, string 4).
const NET_MAC: [u8; 6] = [0x02, 0x4E, 0x58, 0x4B, 0x00, 0x01];
const HOST_MAC: &str = "024E584B0002";

/// pid.codes test IDs: private use only.
const VID: u16 = 0x1209;
const PID: u16 = 0x0001;

#[rustfmt::skip]
const DEVICE: [u8; 18] = [
    18, 1, 0x00, 0x02, // USB 2.0
    0xEF, 0x02, 0x01, 64, // functions in interface associations, EP0 max packet 64
    VID as u8, (VID >> 8) as u8, PID as u8, (PID >> 8) as u8,
    0x00, 0x02, 1, 2, 3, 1, // release 2.0, strings, 1 configuration
];
const QUALIFIER: [u8; 10] = [10, 6, 0x00, 0x02, 0xEF, 0x02, 0x01, 64, 1, 0];

/// Configuration: the console (CDC-ACM control interface, whose
/// notification endpoint is never used, and data interface) and the
/// network (CDC-ECM control interface, data interface whose bulk pair
/// is alternate setting 1); sizes per bus speed.
#[rustfmt::skip]
fn config(hs: bool) -> Vec<u8> {
    let [lo, hi] = if hs { 512u16 } else { 64 }.to_le_bytes();
    let interval = if hs { 9 } else { 32 }; // 32 ms
    let [flo, fhi] = (FRAME as u16).to_le_bytes();
    let mut d = alloc::vec![
        9, 2, 0, 0, 4, 1, 0, 0xC0, 1, // length below, 4 interfaces, self-powered
        8, 11, 0, 2, 0x02, 0x02, 0, 0, // association: interfaces 0-1, ACM
        9, 4, 0, 0, 1, 0x02, 0x02, 0, 0, // communication: ACM, no AT commands
        5, 0x24, 0x00, 0x10, 0x01, // header: CDC 1.10
        5, 0x24, 0x01, 0x00, 1, // call management: data on interface 1
        4, 0x24, 0x02, 0x02, // ACM: line coding, control line state
        5, 0x24, 0x06, 0, 1, // union: 0 controls 1
        7, 5, 0x82, 3, 16, 0, interval, // notifications, interrupt IN 2
        9, 4, 1, 0, 2, 0x0A, 0, 0, 0, // data interface
        7, 5, 0x01, 2, lo, hi, 0, // bulk OUT 1
        7, 5, 0x81, 2, lo, hi, 0, // bulk IN 1
        8, 11, 2, 2, 0x02, 0x06, 0, 0, // association: interfaces 2-3, ECM
        9, 4, 2, 0, 1, 0x02, 0x06, 0, 0, // communication: ECM
        5, 0x24, 0x00, 0x10, 0x01, // header: CDC 1.10
        5, 0x24, 0x06, 2, 3, // union: 2 controls 3
        // Ethernet: host MAC in string 4, no statistics, frames of
        // 1514 bytes, no multicast or power filters.
        13, 0x24, 0x0F, 4, 0, 0, 0, 0, flo, fhi, 0, 0, 0,
        7, 5, 0x84, 3, 16, 0, interval, // link notifications, interrupt IN 4
        9, 4, 3, 0, 0, 0x0A, 0, 0, 0, // data interface, off
        9, 4, 3, 1, 2, 0x0A, 0, 0, 0, // data interface, on
        7, 5, 0x03, 2, lo, hi, 0, // bulk OUT 3
        7, 5, 0x83, 2, lo, hi, 0, // bulk IN 3
    ];
    let n = d.len() as u16;
    d[2..4].copy_from_slice(&n.to_le_bytes());
    d
}

/// MAC as the `eth` protocol's word: byte 0 lowest.
fn mac_word() -> u64 {
    let mut b = [0u8; 8];
    b[..6].copy_from_slice(&NET_MAC);
    u64::from_le_bytes(b)
}

/// String descriptor `index` in `out`; `None`: no such string.
fn string(index: u8, out: &mut [u8; 32]) -> Option<usize> {
    let s = match index {
        0 => {
            out[..4].copy_from_slice(&[4, 3, 0x09, 0x04]); // US English
            return Some(4);
        }
        1 => "nxk",
        2 => "nxk console",
        3 => "0001",
        4 => HOST_MAC,
        _ => return None,
    };
    let n = 2 + 2 * s.len();
    out[..2].copy_from_slice(&[n as u8, 3]);
    for (i, b) in s.bytes().enumerate() {
        out[2 + 2 * i..4 + 2 * i].copy_from_slice(&[b, 0]);
    }
    Some(n)
}

/// Control transfer in progress on EP0.
#[derive(Clone, Copy, PartialEq)]
enum Ep0 {
    Idle,
    /// IN data stage; the status stage (OUT) follows.
    DataIn,
    StatusOut,
    /// Status stage (IN), then the address from SET_ADDRESS if any.
    StatusIn(Option<u8>),
    /// OUT data stage of SET_LINE_CODING.
    LineCoding,
}

/// Traffic counters (`proto::STATS`).
#[derive(Default)]
struct Stats {
    rx_bytes: u64,
    tx_bytes: u64,
    rx_xfers: u64,
    tx_xfers: u64,
    resets: u32,
    errors: u32,
}

struct Dev {
    udc: Udc,
    term: u32,
    /// Terminal open on the host (configured, DTR up): output flows,
    /// from `hold_until` on.
    online: bool,
    hold_until: u64,
    /// Last byte of a transfer that was a whole number of packets, sent
    /// first in the next one.
    carry: Option<u8>,
    stats: Stats,
    hs: bool,
    ep0: Ep0,
    config: u8,
    dtr: bool,
    line: [u8; 7],
    /// Bytes of the IN transfer in flight (0: none).
    tx_len: usize,
    /// A host reset the bus once: a PC sees the Switch.
    seen: bool,
    /// Network function on: the host chose the data interface's bulk pair.
    net: bool,
    /// Frames from the host not yet taken, frames for it not yet sent,
    /// bytes of the one in flight (0: none).
    net_rx: VecDeque<Vec<u8>>,
    net_tx: VecDeque<Vec<u8>>,
    net_tx_len: usize,
    /// `RECV` callers waiting: reply token, deadline (0: none).
    net_readers: VecDeque<(u64, u64)>,
    /// The link went down with nobody waiting: the next `RECV` hears it.
    net_down_untold: bool,
}

impl Dev {
    /// Fatal status: on the console and the screen. Connection changes
    /// only log: the home screen shows them as statistics.
    fn fatal(&self, text: &str) {
        drv::report("usbcon", text);
    }

    fn max_packet(&self) -> usize {
        if self.hs {
            512
        } else {
            64
        }
    }

    fn set_online(&mut self) {
        let online = self.config != 0 && self.dtr;
        if self.online != online {
            self.online = online;
            self.hold_until = time::now_ns() + HOLD_NS;
            println!(
                "[usbcon] {}",
                if online {
                    "terminal open on the host"
                } else {
                    "terminal closed"
                }
            );
        }
    }

    fn state(&self) -> u64 {
        let bit = |on: bool, b: u64| if on { b } else { 0 };
        bit(self.seen, proto::ST_HOST)
            | bit(self.config != 0, proto::ST_CONFIGURED)
            | bit(self.hs, proto::ST_HIGH_SPEED)
            | bit(self.online, proto::ST_TERMINAL)
            | bit(self.net, proto::ST_NET)
    }

    fn interrupt(&mut self) {
        let st = self.udc.take_status();
        if st & udc::STS_URI != 0 {
            self.bus_reset();
        }
        if st & udc::STS_UEI != 0 {
            self.stats.errors += 1;
            if self.stats.errors == 1 {
                println!("[usbcon] USB transfer error (UEI)");
            }
        }
        if st & udc::STS_PCI != 0 {
            self.hs = self.udc.high_speed();
        }
        // Completions first: a finished status stage precedes a new SETUP.
        let done = self.udc.take_complete();
        if done & (bit(0, Dir::Out) | bit(0, Dir::In)) != 0 {
            self.ep0_done(done);
        }
        if done & bit(1, Dir::Out) != 0 {
            self.rx_done();
        }
        if done & bit(1, Dir::In) != 0 && self.tx_len != 0 {
            self.stats.tx_bytes += self.tx_len as u64;
            self.stats.tx_xfers += 1;
            self.tx_len = 0;
        }
        if done & bit(3, Dir::Out) != 0 {
            self.net_rx_done();
        }
        if done & bit(3, Dir::In) != 0 {
            self.net_tx_len = 0;
        }
        while let Some(p) = self.udc.take_setup() {
            self.setup(p);
        }
    }

    fn bus_reset(&mut self) {
        self.udc.bus_reset();
        self.ep0 = Ep0::Idle;
        self.stats.resets += 1;
        if !self.seen {
            self.seen = true;
            println!("[usbcon] host detected (bus reset)");
        } else if self.config != 0 {
            println!("[usbcon] bus reset by the host");
        }
        self.config = 0;
        self.dtr = false;
        self.tx_len = 0;
        self.set_online();
        self.set_net(false);
    }

    fn configure(&mut self, value: u8) {
        self.set_net(false);
        self.udc
            .flush(bit(1, Dir::Out) | bit(1, Dir::In) | bit(2, Dir::In) | bit(4, Dir::In));
        self.tx_len = 0;
        self.carry = None;
        self.config = value;
        if value != 0 {
            let mps = self.max_packet() as u32;
            self.udc.enable(1, Dir::Out, EpType::Bulk, mps);
            self.udc.enable(1, Dir::In, EpType::Bulk, mps);
            self.udc.enable(2, Dir::In, EpType::Interrupt, 16);
            self.udc.enable(4, Dir::In, EpType::Interrupt, 16);
            self.udc.start(1, Dir::Out, RX_BUF, RX_LEN, false);
            println!(
                "[usbcon] configured by the host ({} speed)",
                if self.hs { "high" } else { "full" }
            );
        } else {
            self.udc.disable_data_eps();
            self.dtr = false;
        }
        self.set_online();
    }

    /// Answer with `data` (at most `max` bytes asked by the host).
    fn data_in(&mut self, data: &[u8], max: usize) {
        let n = data.len().min(max).min(EP0_LEN);
        self.udc.buf(EP0_BUF, n).copy_from_slice(&data[..n]);
        // Short of the request on a packet boundary: a zero-length
        // packet tells the host the data ends.
        let zlp = n > 0 && n < max && n.is_multiple_of(64);
        self.udc.start(0, Dir::In, EP0_BUF, n, zlp);
        self.ep0 = Ep0::DataIn;
    }

    fn status_in(&mut self, addr: Option<u8>) {
        self.udc.start(0, Dir::In, EP0_BUF, 0, false);
        self.ep0 = Ep0::StatusIn(addr);
    }

    fn stall(&mut self) {
        self.udc.stall(0, Dir::In, true);
        self.ep0 = Ep0::Idle;
    }

    fn ep0_done(&mut self, done: u32) {
        let out = done & bit(0, Dir::Out) != 0;
        let inn = done & bit(0, Dir::In) != 0;
        match self.ep0 {
            Ep0::DataIn if inn => {
                self.udc.start(0, Dir::Out, EP0_BUF, 0, false);
                self.ep0 = Ep0::StatusOut;
            }
            Ep0::StatusOut if out => self.ep0 = Ep0::Idle,
            Ep0::StatusIn(addr) if inn => {
                // The new address counts once the status stage is over.
                if let Some(a) = addr {
                    self.udc.set_address(a);
                }
                self.ep0 = Ep0::Idle;
            }
            Ep0::LineCoding if out => {
                if self.udc.finished(0, Dir::Out) == Some(Ok(0)) {
                    self.line.copy_from_slice(self.udc.buf(EP0_BUF, 7));
                }
                self.status_in(None);
            }
            _ => {}
        }
    }

    fn setup(&mut self, p: [u8; 8]) {
        let (kind, req) = (p[0], p[1]);
        let value = u16::from_le_bytes([p[2], p[3]]);
        let index = u16::from_le_bytes([p[4], p[5]]);
        let max = u16::from_le_bytes([p[6], p[7]]) as usize;
        // A SETUP ends whatever EP0 was doing.
        self.udc.flush(bit(0, Dir::Out) | bit(0, Dir::In));
        self.ep0 = Ep0::Idle;
        let ep = (index & 0xF) as usize;
        let dir = if index & 0x80 != 0 { Dir::In } else { Dir::Out };
        let data_ep =
            (matches!(ep, 1 | 3) || (matches!(ep, 2 | 4) && dir == Dir::In)) && self.config != 0;
        match (kind, req) {
            // GET_DESCRIPTOR
            (0x80, 6) => match (value >> 8) as u8 {
                1 => self.data_in(&DEVICE, max),
                2 => self.data_in(&config(self.hs), max),
                3 => {
                    let mut s = [0u8; 32];
                    match string(value as u8, &mut s) {
                        Some(n) => self.data_in(&s[..n], max),
                        None => self.stall(),
                    }
                }
                6 => self.data_in(&QUALIFIER, max),
                _ => self.stall(),
            },
            (0x00, 5) => self.status_in(Some(value as u8 & 0x7F)), // SET_ADDRESS
            (0x00, 9) if value <= 1 => {
                self.status_in(None);
                self.configure(value as u8);
            }
            (0x80, 8) => self.data_in(&[self.config], max),
            (0x80, 0) => self.data_in(&[1, 0], max), // self-powered
            (0x81, 0) => self.data_in(&[0, 0], max),
            (0x82, 0) if ep == 0 || data_ep => {
                let halted = ep != 0 && self.udc.stalled(ep, dir);
                self.data_in(&[halted as u8, 0], max);
            }
            // GET_INTERFACE, SET_INTERFACE: the network's data interface
            // has its bulk pair in alternate setting 1.
            (0x81, 10) => {
                let alt = (index == NET_DATA && self.net) as u8;
                self.data_in(&[alt], max)
            }
            (0x01, 11) if index == NET_DATA && value <= 1 && self.config != 0 => {
                self.status_in(None);
                self.set_net(value == 1);
            }
            (0x01, 11) if value == 0 => self.status_in(None),
            // CDC-ECM: SET_ETHERNET_PACKET_FILTER (all frames go anyway).
            (0x21, 0x43) if index == NET_IFACE as u16 => self.status_in(None),
            // CLEAR_FEATURE, SET_FEATURE (ENDPOINT_HALT)
            (0x02, 1 | 3) if value == 0 && (ep == 0 || data_ep) => {
                if ep != 0 {
                    self.udc.stall(ep, dir, req == 3);
                }
                self.status_in(None);
            }
            // CDC: SET_LINE_CODING, GET_LINE_CODING, SET_CONTROL_LINE_STATE,
            // SEND_BREAK.
            (0x21, 0x20) if max == 7 => {
                self.udc.start(0, Dir::Out, EP0_BUF, 7, false);
                self.ep0 = Ep0::LineCoding;
            }
            (0xA1, 0x21) => {
                let line = self.line;
                self.data_in(&line, max);
            }
            (0x21, 0x22) => {
                self.status_in(None);
                self.dtr = value & 1 != 0;
                self.set_online();
            }
            (0x21, 0x23) => self.status_in(None),
            _ => self.stall(),
        }
    }

    /// Typed bytes: rearm first (the host keeps sending), then hand them
    /// to the console.
    fn rx_done(&mut self) {
        let n = match self.udc.finished(1, Dir::Out) {
            None => return, // stale completion: still receiving
            Some(Ok(left)) => RX_LEN - left,
            Some(Err(_)) => 0,
        };
        let mut data = [0u8; RX_LEN];
        data[..n].copy_from_slice(self.udc.buf(RX_BUF, n));
        if self.config != 0 {
            self.udc.start(1, Dir::Out, RX_BUF, RX_LEN, false);
        }
        self.stats.rx_bytes += n as u64;
        self.stats.rx_xfers += 1;
        for chunk in data[..n].chunks(ipc::MAX_BYTES) {
            let _ = ipc::call(self.term, &ipc::pack(con::INPUT, chunk));
        }
    }

    /// Next IN transfer, pulled from the console, if the endpoint is
    /// free and a terminal reads.
    /// ns until held output may go (0: none held).
    fn hold_left(&self) -> u64 {
        match self.hold_until.checked_sub(time::now_ns()) {
            Some(left) if self.online => left.max(1),
            _ => 0,
        }
    }

    fn send(&mut self) {
        if !self.online || self.tx_len != 0 || self.hold_left() != 0 {
            return;
        }
        let buf = self.udc.buf(TX_BUF, TX_LEN);
        let mut n = 0;
        if let Some(b) = self.carry.take() {
            buf[0] = b;
            n = 1;
        }
        let ask = Msg::from_words(con::OUTPUT, &[(TX_LEN - n) as u64]);
        if let Ok(m) = ipc::call(self.term, &ask) {
            if m.label() == 0 {
                n += ipc::unpack(&m, &mut buf[n..]);
            }
        }
        // Never a whole number of packets: the host sees the end of
        // each transfer without a zero-length packet.
        if n > 1 && n.is_multiple_of(self.max_packet()) {
            n -= 1;
            self.carry = Some(buf[n]);
        }
        if n > 0 {
            self.udc.start(1, Dir::In, TX_BUF, n, false);
            self.tx_len = n;
        }
    }

    /// Network function on (fresh endpoints, carrier to the host) or off.
    fn set_net(&mut self, on: bool) {
        let was = self.net;
        self.udc.flush(bit(3, Dir::Out) | bit(3, Dir::In));
        self.net_tx_len = 0;
        self.net_tx.clear();
        self.net_rx.clear();
        self.net = on;
        if on {
            let mps = self.max_packet() as u32;
            self.udc.enable(3, Dir::Out, EpType::Bulk, mps);
            self.udc.enable(3, Dir::In, EpType::Bulk, mps);
            self.udc.start(3, Dir::Out, NET_RX_BUF, NET_RX_LEN, false);
            // NETWORK_CONNECTION, connected: the host's carrier.
            let note = [0xA1, 0x00, 1, 0, NET_IFACE, 0, 0, 0];
            self.udc.buf(NET_NOTE_BUF, 8).copy_from_slice(&note);
            self.udc.start(4, Dir::In, NET_NOTE_BUF, 8, false);
            println!("[usbcon] network on (usb0)");
        } else {
            self.udc.disable(3);
            if was {
                println!("[usbcon] network off");
            }
            // Readers hear the loss; `net` starts its DHCP over.
            self.net_down_untold = self.net_readers.is_empty();
            for (token, _) in self.net_readers.drain(..) {
                let _ = ipc::reply_to(token, &Msg::error(-ENETDOWN));
            }
        }
    }

    /// A frame from the host: to a waiting reader, else queued.
    fn net_rx_done(&mut self) {
        let n = match self.udc.finished(3, Dir::Out) {
            None => return,
            Some(Ok(left)) => NET_RX_LEN - left,
            Some(Err(_)) => 0,
        };
        let frame = self.udc.buf(NET_RX_BUF, n).to_vec();
        if self.net {
            self.udc.start(3, Dir::Out, NET_RX_BUF, NET_RX_LEN, false);
        }
        if n < 14 {
            return;
        }
        match self.net_readers.pop_front() {
            Some((token, _)) => {
                let _ = ipc::reply_to(token, &ipc::pack(0, &frame));
            }
            None => {
                if self.net_rx.len() == NET_QUEUE {
                    self.net_rx.pop_front();
                }
                self.net_rx.push_back(frame);
            }
        }
    }

    /// Next frame to the host, if the endpoint is free.
    fn net_send(&mut self) {
        if !self.net || self.net_tx_len != 0 {
            return;
        }
        let Some(f) = self.net_tx.pop_front() else {
            return;
        };
        let mut n = f.len();
        self.udc.buf(NET_TX_BUF, n).copy_from_slice(&f);
        // Never a whole number of packets, so no zero-length packet: one
        // byte of padding, which the host's IP layer drops.
        if n.is_multiple_of(self.max_packet()) {
            self.udc.buf(NET_TX_BUF + n, 1)[0] = 0;
            n += 1;
        }
        self.udc.start(3, Dir::In, NET_TX_BUF, n, false);
        self.net_tx_len = n;
    }

    /// `eth` request on "usb0"; `None`: the reply waits for a frame.
    fn link(&mut self, rx: &ipc::Received, buf: &mut [u8]) -> Option<Msg> {
        Some(match rx.msg.label() {
            eth::INFO => Msg::from_words(0, &[mac_word(), MTU, self.net as u64]),
            eth::SEND => {
                let n = ipc::unpack(&rx.msg, buf);
                if !self.net {
                    Msg::error(-ENETDOWN)
                } else if !(14..=FRAME).contains(&n) {
                    Msg::error(-EINVAL)
                } else if self.net_tx.len() >= NET_QUEUE {
                    Msg::error(-EAGAIN)
                } else {
                    self.net_tx.push_back(buf[..n].to_vec());
                    Msg::new(0, 0, 0)
                }
            }
            eth::RECV => match self.net_rx.pop_front() {
                Some(f) => ipc::pack(0, &f),
                None if self.net_down_untold => {
                    self.net_down_untold = false;
                    Msg::error(-ENETDOWN)
                }
                None => {
                    let t = rx.msg.mr[0];
                    let deadline = if t == 0 { 0 } else { time::now_ns() + t };
                    self.net_readers.push_back((rx.token, deadline));
                    return None;
                }
            },
            _ => Msg::error(-EINVAL),
        })
    }

    /// `RECV` callers past their deadline.
    fn net_expire(&mut self) {
        let now = time::now_ns();
        self.net_readers.retain(|&(token, deadline)| {
            let expired = deadline != 0 && now >= deadline;
            if expired {
                let _ = ipc::reply_to(token, &Msg::error(-ETIMEDOUT));
            }
            !expired
        });
    }

    /// ns until the nearest `RECV` deadline (0: none).
    fn net_left(&self) -> u64 {
        let now = time::now_ns();
        self.net_readers
            .iter()
            .filter(|r| r.1 != 0)
            .map(|r| r.1.saturating_sub(now).max(1))
            .min()
            .unwrap_or(0)
    }

    fn stats_reply(&self) -> Msg {
        let s = &self.stats;
        Msg::from_words(
            0,
            &[
                self.state(),
                s.rx_bytes,
                s.tx_bytes,
                s.rx_xfers,
                s.tx_xfers,
                s.resets as u64 | (s.errors as u64) << 32,
            ],
        )
    }
}

libnx::entry!(main);

fn main() -> i32 {
    // Resources: registers, APB_MISC, fuses, IRQ (init's order).
    let misc = Mmio::map(drv::dev_slot(1), APB_MISC, 0x1000).expect("usbcon: APB_MISC");
    let mut dev = Dev {
        udc: {
            let regs = Mmio::map(drv::dev_slot(0), USB_BASE, 0x2000).expect("usbcon: registers");
            Udc::new(regs).expect("usbcon: DMA memory")
        },
        term: slots::TERMINAL,
        online: false,
        hold_until: 0,
        carry: None,
        stats: Stats::default(),
        hs: true,
        ep0: Ep0::Idle,
        config: 0,
        dtr: false,
        line: [0x00, 0xC2, 0x01, 0x00, 0, 0, 8], // 115200 8N1
        tx_len: 0,
        seen: false,
        net: false,
        net_rx: VecDeque::new(),
        net_tx: VecDeque::new(),
        net_tx_len: 0,
        net_readers: VecDeque::new(),
        net_down_untold: false,
    };
    // T210B01 (Mariko) has no such controller: its OTG port is XUSB's.
    if (misc.r32(HIDREV) >> 4) & 0xF >= 2 {
        dev.fatal("Mariko SoC: no USB2 OTG device controller, stopping");
        return 0;
    }
    let _ = libnx::clk::enable(clk::USBD);
    if !libnx::clk::ready(clk::USBD) {
        dev.fatal("USB PLLs or controller clock not ready, stopping");
        return 0;
    }
    // Fuse reads hang the bus unless the fuse block is clocked.
    let trims = if libnx::clk::ready(clk::FUSE) {
        Mmio::map(drv::dev_slot(2), FUSE_PAGE, 0x1000)
            .ok()
            .map(|f| (f.r32(FUSE_USB_CALIB), f.r32(FUSE_USB_CALIB_EXT)))
    } else {
        None
    };
    if trims.is_none() {
        dev.fatal("fuses unreadable: PHY left without its trims");
    }
    let trk = |on| {
        if on {
            libnx::clk::enable(clk::USB2_TRK).is_ok()
        } else {
            libnx::clk::disable(clk::USB2_TRK).is_ok()
        }
    };
    if !dev.udc.phy_init(trims, trk) {
        dev.fatal("USB2 tracking clock refused, stopping");
        return 0;
    }
    if !dev.udc.reset_device_mode() {
        dev.fatal("controller reset timed out, stopping");
        return 0;
    }
    if dev.udc.endpoints() < udc::NUM_EPS as u32 {
        println!(
            "[usbcon] controller reports {} endpoints",
            dev.udc.endpoints()
        );
    }
    // One notification wakes the service loop: controller interrupt and
    // console output pending (the console signals a copy of it).
    let irq = Irq::new(drv::dev_slot(3)).expect("usbcon: IRQ");
    let notif = irq.notification();
    sys::notify_bind(notif).expect("usbcon: notification");
    let signal = sys::handle_dup(notif, SIGNAL | GRANT, 0).expect("usbcon: notification");
    ipc::set_send_handle(0, signal);
    let attach = Msg::from_words(con::ATTACH, &[EV_TX]).with_handles(1);
    if ipc::call_checked(dev.term, &attach).is_err() {
        dev.fatal("console refused the terminal, stopping");
        return 0;
    }
    let _ = sys::handle_close(signal);
    let ep = sys::endpoint_create().expect("usbcon: endpoint");
    publish(ep, "usbcon", proto::BADGE_CTRL).expect("usbcon: publish");
    publish(ep, "usb0", proto::BADGE_LINK).expect("usbcon: publish usb0");
    if !dev
        .udc
        .attach(udc::STS_UI | udc::STS_UEI | udc::STS_PCI | udc::STS_URI)
    {
        dev.fatal("controller did not start, stopping");
        return 0;
    }
    println!("[usbcon] USB-C console ready, waiting for a PC (tools/usbterm.py)");
    // Requests and events; a timeout ends the output hold or a wait.
    let mut reply: Option<Msg> = None;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let wait = match (dev.hold_left(), dev.net_left()) {
            (0, n) | (n, 0) => n,
            (h, n) => h.min(n),
        };
        let rx = match reply.take() {
            Some(m) => ipc::reply_recv_timeout(ep, &m, wait),
            None => ipc::recv_timeout(ep, wait),
        };
        match rx {
            Ok(rx) if rx.msg.is_notify() => {
                if rx.msg.mr[0] & EV_IRQ != 0 {
                    dev.interrupt();
                    irq.ack();
                }
            }
            Ok(rx) => {
                reply = match (rx.badge, rx.msg.label()) {
                    (proto::BADGE_LINK, _) => dev.link(&rx, &mut buf),
                    (_, proto::STATS) => Some(dev.stats_reply()),
                    _ => Some(Msg::error(-EINVAL)),
                }
            }
            Err(_) => {}
        }
        dev.net_expire();
        dev.send();
        dev.net_send();
    }
}

/// Register `ep` under a name with its own badge: one service, two
/// protocols whose labels would otherwise collide.
fn publish(ep: u32, name: &str, badge: u64) -> Result<(), i64> {
    let h = sys::handle_dup(ep, SEND | GRANT | DUP, badge)?;
    let r = ns::register(name, h);
    let _ = sys::handle_close(h);
    r
}

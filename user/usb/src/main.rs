//! usb: xHCI host; HID keyboard to "input", CDC-ECM net as "eth0".
//! Standard xHCI only; controller touched once clk reports it ready.

#![no_std]
#![no_main]

extern crate alloc;

mod xhci;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{self, EAGAIN, EINVAL, ENODEV, ETIMEDOUT};
use libnx::abi::proto::{clk, eth, input};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::{ns, println, sys, time};

use xhci::{completion, dci, trb_ptr, trb_type, Device, EpSpec, Xhci, CC_SHORT, EV_TRANSFER};

const XHCI_BASE: u64 = 0x7009_0000;
const FRAME: usize = 2048;
const RX_BUFS: usize = 16;
const TX_BUFS: usize = 16;

struct Keyboard {
    dev: Device,
    dci: usize,
    buf: (*mut u8, u64),
    last: [u8; 8],
}

struct Net {
    dev: Device,
    rx_dci: usize,
    tx_dci: usize,
    mac: [u8; 6],
    rx: Vec<(*mut u8, u64, u64)>, // tampon, adresse physique, TRB en cours
    tx: Vec<(*mut u8, u64)>,
    tx_next: usize,
    tx_busy: usize,
    frames: VecDeque<Vec<u8>>,
}

/// Character produced by a key (US layout).
fn hid_char(code: u8, shift: bool) -> u32 {
    let c = match code {
        0x04..=0x1D => {
            let c = (b'a' + code - 0x04) as char;
            if shift {
                c.to_ascii_uppercase()
            } else {
                c
            }
        }
        0x1E..=0x27 => {
            let (n, s) = (b"1234567890", b"!@#$%^&*()");
            (if shift { s } else { n })[(code - 0x1E) as usize] as char
        }
        0x28 => '\n',
        0x2A => '\x08',
        0x2B => '\t',
        0x2C => ' ',
        0x2D..=0x38 => {
            let (n, s) = (b"-=[]\\#;'`,./", b"_+{}|~:\"~<>?");
            (if shift { s } else { n })[(code - 0x2D) as usize] as char
        }
        _ => return 0,
    };
    c as u32
}

fn desc(
    hc: &mut Xhci,
    dev: &mut Device,
    kind: u16,
    index: u16,
    lang: u16,
    out: &mut [u8],
) -> Result<usize, i64> {
    hc.control(dev, 0x80, 6, kind << 8 | index, lang, out)
}

/// Endpoint parsed from a configuration descriptor.
#[derive(Clone, Copy)]
struct Ep {
    iface: u8,
    alt: u8,
    address: u8,
    attr: u8,
    max_packet: u16,
    interval: u8,
}

struct Iface {
    number: u8,
    alt: u8,
    class: (u8, u8, u8),
}

struct Config {
    value: u8,
    ifaces: Vec<Iface>,
    eps: Vec<Ep>,
    mac_string: u8,
}

fn parse_config(d: &[u8]) -> Config {
    let mut c = Config {
        value: d[5],
        ifaces: Vec::new(),
        eps: Vec::new(),
        mac_string: 0,
    };
    let (mut cur, mut alt) = (0u8, 0u8);
    let mut p = 0;
    while p + 2 <= d.len() {
        let (len, kind) = (d[p] as usize, d[p + 1]);
        if len < 2 || p + len > d.len() {
            break;
        }
        let x = &d[p..p + len];
        match kind {
            4 if len >= 9 => {
                cur = x[2];
                alt = x[3];
                c.ifaces.push(Iface {
                    number: x[2],
                    alt: x[3],
                    class: (x[5], x[6], x[7]),
                });
            }
            5 if len >= 7 => c.eps.push(Ep {
                iface: cur,
                alt,
                address: x[2],
                attr: x[3] & 3,
                max_packet: u16::from_le_bytes([x[4], x[5]]) & 0x7FF,
                interval: x[6],
            }),
            0x24 if len >= 4 && x[2] == 0x0F => c.mac_string = x[3], // CDC Ethernet
            _ => {}
        }
        p += len;
    }
    c
}

fn set_config(hc: &mut Xhci, dev: &mut Device, value: u8) -> Result<(), i64> {
    hc.control(dev, 0x00, 9, value as u16, 0, &mut [])
        .map(|_| ())
}

fn setup_keyboard(hc: &mut Xhci, mut dev: Device, c: &Config, iface: u8) -> Result<Keyboard, i64> {
    let ep = *c
        .eps
        .iter()
        .find(|e| e.iface == iface && e.attr == 3 && e.address & 0x80 != 0)
        .ok_or(-ENODEV)?;
    set_config(hc, &mut dev, c.value)?;
    hc.configure(
        &mut dev,
        &[EpSpec {
            address: ep.address,
            kind: 7,
            max_packet: ep.max_packet,
            interval: ep.interval,
        }],
    )?;
    let _ = hc.control(&mut dev, 0x21, 0x0B, 0, iface as u16, &mut []); // SET_PROTOCOL(boot)
    let _ = hc.control(&mut dev, 0x21, 0x0A, 0, iface as u16, &mut []); // SET_IDLE(0)
    let buf = hc.dma.alloc(64, 64)?;
    let d = dci(ep.address);
    hc.queue(&mut dev, d, buf.1, 8)?;
    Ok(Keyboard {
        dev,
        dci: d,
        buf,
        last: [0; 8],
    })
}

fn setup_net(hc: &mut Xhci, mut dev: Device, c: &Config) -> Result<Net, i64> {
    let data = c
        .ifaces
        .iter()
        .find(|i| {
            i.class.0 == 0x0A
                && c.eps
                    .iter()
                    .filter(|e| e.iface == i.number && e.alt == i.alt && e.attr == 2)
                    .count()
                    == 2
        })
        .ok_or(-ENODEV)?;
    let (num, alt) = (data.number, data.alt);
    let bulk_in = *c
        .eps
        .iter()
        .find(|e| e.iface == num && e.alt == alt && e.attr == 2 && e.address & 0x80 != 0)
        .ok_or(-ENODEV)?;
    let bulk_out = *c
        .eps
        .iter()
        .find(|e| e.iface == num && e.alt == alt && e.attr == 2 && e.address & 0x80 == 0)
        .ok_or(-ENODEV)?;
    set_config(hc, &mut dev, c.value)?;
    hc.configure(
        &mut dev,
        &[
            EpSpec {
                address: bulk_in.address,
                kind: 6,
                max_packet: bulk_in.max_packet,
                interval: 0,
            },
            EpSpec {
                address: bulk_out.address,
                kind: 2,
                max_packet: bulk_out.max_packet,
                interval: 0,
            },
        ],
    )?;
    hc.control(&mut dev, 0x01, 11, alt as u16, num as u16, &mut [])?; // SET_INTERFACE
                                                                      // MAC address: 12-hex-digit string (UTF-16).
    let mut mac = [0x02, 0, 0, 0, 0, 1];
    if c.mac_string != 0 {
        let mut s = [0u8; 64];
        if desc(hc, &mut dev, 3, c.mac_string as u16, 0x0409, &mut s).is_ok() {
            let hex: Vec<u8> = s[2..(s[0] as usize).min(64)]
                .iter()
                .step_by(2)
                .copied()
                .collect();
            if hex.len() >= 12 {
                for k in 0..6 {
                    let h = core::str::from_utf8(&hex[k * 2..k * 2 + 2])
                        .ok()
                        .and_then(|v| u8::from_str_radix(v, 16).ok());
                    if let Some(b) = h {
                        mac[k] = b;
                    }
                }
            }
        }
    }
    // The host is the other link end: our address must differ.
    mac[5] ^= 1;
    let (rx_dci, tx_dci) = (dci(bulk_in.address), dci(bulk_out.address));
    let mut rx = Vec::new();
    for _ in 0..RX_BUFS {
        let (va, pa) = hc.dma.alloc(FRAME, 64)?;
        let at = hc.queue(&mut dev, rx_dci, pa, FRAME as u32)?;
        rx.push((va, pa, at));
    }
    let mut tx = Vec::new();
    for _ in 0..TX_BUFS {
        tx.push(hc.dma.alloc(FRAME, 64)?);
    }
    Ok(Net {
        dev,
        rx_dci,
        tx_dci,
        mac,
        rx,
        tx,
        tx_next: 0,
        tx_busy: 0,
        frames: VecDeque::new(),
    })
}

/// Handle controller events.
fn events(hc: &mut Xhci, kbd: &mut Option<Keyboard>, net: &mut Option<Net>, input: u32) {
    hc.collect();
    while let Some(t) = hc.pending.pop_front() {
        if trb_type(&t) != EV_TRANSFER {
            continue;
        }
        let (slot, d) = ((t[3] >> 24) as u8, ((t[3] >> 16) & 0x1F) as usize);
        let ok = matches!(completion(&t), 1 | CC_SHORT);
        let residual = (t[2] & 0xFF_FFFF) as usize;
        if let Some(k) = kbd.as_mut().filter(|k| k.dev.slot == slot && k.dci == d) {
            if ok {
                let mut r = [0u8; 8];
                unsafe { core::ptr::copy_nonoverlapping(k.buf.0, r.as_mut_ptr(), 8) };
                let shift = r[0] & 0x22 != 0;
                let send = |code: u8, down: bool| {
                    if input != 0 {
                        let mut m = Msg::new(input::INJECT, 5, 0);
                        m.mr[..5].copy_from_slice(&[
                            input::KIND_KEY,
                            code as u64,
                            down as u64,
                            hid_char(code, shift) as u64,
                            0,
                        ]);
                        let _ = ipc::call(input, &m);
                    }
                };
                for &c in r[2..]
                    .iter()
                    .filter(|&&c| c > 3 && !k.last[2..].contains(&c))
                {
                    send(c, true);
                }
                for &c in k.last[2..]
                    .iter()
                    .filter(|&&c| c > 3 && !r[2..].contains(&c))
                {
                    send(c, false);
                }
                k.last = r;
            }
            let pa = k.buf.1;
            let _ = hc.queue(&mut k.dev, k.dci, pa, 8);
            continue;
        }
        if let Some(n) = net.as_mut().filter(|n| n.dev.slot == slot) {
            if d == n.rx_dci {
                if let Some(i) = n.rx.iter().position(|b| b.2 == trb_ptr(&t)) {
                    let (va, pa, _) = n.rx[i];
                    let len = FRAME.saturating_sub(residual);
                    if ok && len >= 14 && n.frames.len() < 64 {
                        let mut f = alloc::vec![0u8; len];
                        unsafe { core::ptr::copy_nonoverlapping(va, f.as_mut_ptr(), len) };
                        n.frames.push_back(f);
                    }
                    if let Ok(at) = hc.queue(&mut n.dev, n.rx_dci, pa, FRAME as u32) {
                        n.rx[i].2 = at;
                    }
                }
            } else if d == n.tx_dci {
                n.tx_busy = n.tx_busy.saturating_sub(1);
            }
        }
    }
}

/// Ask clk to enable the controller, then check it is ready
/// (`clk::ST_READY`). Error: what is missing.
fn controller_ready() -> Result<(), String> {
    let _ = libnx::clk::enable(clk::XUSB_HOST);
    let (st, _) =
        libnx::clk::status(clk::XUSB_HOST).map_err(|e| alloc::format!("clk: {}", err::name(e)))?;
    let missing: Vec<&str> = [
        (clk::ST_POWERED, "partition off"),
        (clk::ST_CLOCK, "clock off"),
        (clk::ST_OUT_OF_RESET, "in reset"),
    ]
    .iter()
    .filter(|b| st & b.0 == 0)
    .map(|b| b.1)
    .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing.join(", "))
    }
}

libnx::entry!(main);

fn main() -> i32 {
    if let Err(why) = controller_ready() {
        println!("[usb] xHCI controller unpowered ({})", why);
        return 0;
    }
    let regs = Mmio::map(drv::dev_slot(0), XHCI_BASE, 0x4000).expect("usb: registers");
    let irq = Irq::new(drv::dev_slot(1)).expect("usb: interrupt");
    let mut hc = match Xhci::new(regs, irq) {
        Ok(h) => h,
        Err(e) => {
            println!("[usb] xHCI controller missing ({})", err::name(e));
            return 0;
        }
    };
    let (mut kbd, mut net): (Option<Keyboard>, Option<Net>) = (None, None);
    for port in 1..=hc.ports {
        let Some(speed) = hc.reset_port(port) else {
            continue;
        };
        let mut step = "address";
        let r = (|| -> Result<(), i64> {
            let mut dev = hc.address(port, speed)?;
            let mut d = [0u8; 18];
            step = "descriptor (8 bytes)";
            desc(&mut hc, &mut dev, 1, 0, 0, &mut d[..8])?;
            let mps0 = if d[7] == 9 { 512 } else { d[7] as u16 };
            step = "EP0 packet size";
            hc.set_ep0_packet(&mut dev, mps0)?;
            step = "device descriptor";
            desc(&mut hc, &mut dev, 1, 0, 0, &mut d)?;
            let vid_pid = (
                u16::from_le_bytes([d[8], d[9]]),
                u16::from_le_bytes([d[10], d[11]]),
            );
            step = "configuration";
            for ci in 0..d[17] {
                let mut hdr = [0u8; 9];
                desc(&mut hc, &mut dev, 2, ci as u16, 0, &mut hdr)?;
                let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
                let mut full = alloc::vec![0u8; total.min(4096)];
                desc(&mut hc, &mut dev, 2, ci as u16, 0, &mut full)?;
                let c = parse_config(&full);
                if let Some(i) = c.ifaces.iter().find(|i| i.class == (3, 1, 1)) {
                    if kbd.is_none() {
                        let n = i.number;
                        kbd = Some(setup_keyboard(&mut hc, dev, &c, n)?);
                        println!(
                            "[usb] port {}: HID keyboard {:04x}:{:04x}",
                            port, vid_pid.0, vid_pid.1
                        );
                        return Ok(());
                    }
                }
                if c.ifaces.iter().any(|i| i.class.0 == 2 && i.class.1 == 6) && net.is_none() {
                    step = "CDC-ECM setup";
                    let n = setup_net(&mut hc, dev, &c)?;
                    let m = n.mac;
                    println!(
                        "[usb] port {}: CDC-ECM network {:04x}:{:04x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        port, vid_pid.0, vid_pid.1, m[0], m[1], m[2], m[3], m[4], m[5]
                    );
                    net = Some(n);
                    return Ok(());
                }
            }
            println!(
                "[usb] port {}: unsupported device {:04x}:{:04x}",
                port, vid_pid.0, vid_pid.1
            );
            Ok(())
        })();
        if let Err(e) = r {
            println!(
                "[usb] port {}: failed at step {} ({})",
                port,
                step,
                err::name(e)
            );
        }
    }
    let input = if kbd.is_some() {
        ns::wait_for_timeout("input", 3_000_000_000).unwrap_or(0)
    } else {
        0
    };
    if kbd.is_none() && net.is_none() {
        println!("[usb] no usable device");
    }
    let ep = if net.is_some() {
        drv::publish("eth0").expect("usb: publish")
    } else {
        sys::endpoint_create().expect("usb: endpoint")
    };
    let _ = sys::notify_bind(hc.irq.notification());

    // Waiting frame readers: (token, deadline).
    let mut readers: VecDeque<(u64, u64)> = VecDeque::new();
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let mut rx = ipc::recv_timeout(ep, 50_000_000);
    // (First round bounded: events arrived during enumeration.)
    loop {
        match &rx {
            Ok(r) if r.msg.is_notify() => {
                hc.irq.ack();
            }
            Ok(r) => {
                let reply = match (r.msg.label(), net.as_mut()) {
                    (eth::INFO, Some(n)) => {
                        let mut m = Msg::new(0, 2, 0);
                        let mut mac = [0u8; 8];
                        mac[..6].copy_from_slice(&n.mac);
                        m.mr[0] = u64::from_le_bytes(mac);
                        m.mr[1] = 1500;
                        Some(m)
                    }
                    (eth::SEND, Some(n)) => {
                        let len = ipc::unpack(&r.msg, &mut buf);
                        if !(14..=FRAME).contains(&len) {
                            Some(Msg::error(-EINVAL))
                        } else if n.tx_busy >= TX_BUFS {
                            Some(Msg::error(-EAGAIN))
                        } else {
                            let (va, pa) = n.tx[n.tx_next];
                            n.tx_next = (n.tx_next + 1) % TX_BUFS;
                            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), va, len) };
                            n.tx_busy += 1;
                            let d = n.tx_dci;
                            Some(match hc.queue(&mut n.dev, d, pa, len as u32) {
                                Ok(_) => Msg::new(0, 0, 0),
                                Err(e) => Msg::error(e),
                            })
                        }
                    }
                    (eth::RECV, Some(_)) => {
                        let t = r.msg.mr[0];
                        readers.push_back((r.token, if t == 0 { 0 } else { time::now_ns() + t }));
                        None
                    }
                    _ => Some(Msg::error(-EINVAL)),
                };
                if let Some(m) = reply {
                    let _ = ipc::reply(&m);
                }
            }
            Err(e) if *e == -ETIMEDOUT => {}
            Err(_) => time::sleep_ms(1),
        }
        events(&mut hc, &mut kbd, &mut net, input);
        if let Some(n) = net.as_mut() {
            while !readers.is_empty() {
                let Some(f) = n.frames.pop_front() else { break };
                let (tok, _) = readers.pop_front().unwrap();
                let _ = ipc::reply_to(tok, &ipc::pack(0, &f));
            }
        }
        let now = time::now_ns();
        readers.retain(|&(tok, d)| {
            if d != 0 && now >= d {
                let _ = ipc::reply_to(tok, &Msg::error(-ETIMEDOUT));
                false
            } else {
                true
            }
        });
        // IRQ- or message-driven: only wake for the earliest deadline.
        let wait = match readers.iter().filter(|r| r.1 != 0).map(|r| r.1).min() {
            Some(d) => d.saturating_sub(time::now_ns()).max(1),
            None => 0,
        };
        rx = ipc::recv_timeout(ep, wait);
    }
}

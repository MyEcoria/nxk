//! wlan: Wi-Fi station. One endpoint under two names: "wlan0" carries
//! the link (`eth` protocol), "wifi" the station control (`wifi`).
//! Hardware is a BCM4356 behind the Tegra PCIe root complex (`pcie`);
//! without one (emulator) a simulated adapter keeps the protocol, the
//! link and the `wifi` program exercised.

#![no_std]
#![no_main]

extern crate alloc;

mod chip;
mod pcie;
mod sim;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{self, EINVAL, ENOSYS, ETIMEDOUT};
use libnx::abi::proto::{eth, wifi};
use libnx::abi::rights::{DUP, GRANT, SEND};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::{ns, println, sys, time};

/// Station address while the real one is out of reach: the console
/// keeps it in CAL0 on the eMMC, which nxk cannot read yet. Locally
/// administered, so it collides with no real card.
const MAC: [u8; 6] = [0x02, 0x4E, 0x58, 0x4B, 0x57, 0x01];
const MTU: u64 = 1500;
/// Scan time when the caller asks for no particular one.
const SCAN_TIMEOUT: u64 = 3_000_000_000;

/// The root complex, kept for the interrupts it reports. The chip
/// behind it is identified at startup and logged.
struct Hw {
    rc: pcie::Pcie,
}

enum Backend {
    /// Chip identified on the bus. Its firmware and the ring protocol
    /// that carries frames are not implemented yet: the link stays
    /// down, only STATUS answers.
    Chip(Hw),
    Sim(sim::Sim),
}

impl Backend {
    fn state(&self) -> u64 {
        match self {
            Backend::Chip(_) => wifi::ST_DOWN,
            Backend::Sim(s) if s.joined.is_some() => wifi::ST_CONNECTED,
            Backend::Sim(_) => wifi::ST_IDLE,
        }
    }

    fn scan(&self, _timeout: u64) -> Result<(usize, Vec<u8>), i64> {
        match self {
            Backend::Sim(s) => Ok((sim::APS.len(), s.scan())),
            Backend::Chip(_) => Err(-ENOSYS),
        }
    }

    /// Channel, signal and SSID of the network in use.
    fn joined(&self) -> (u64, u64, &[u8]) {
        match self {
            Backend::Sim(s) => match s.joined {
                Some(i) => (
                    sim::APS[i].channel as u64,
                    sim::APS[i].signal as u64,
                    sim::APS[i].ssid.as_bytes(),
                ),
                None => (0, 0, &[]),
            },
            Backend::Chip(_) => (0, 0, &[]),
        }
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

/// Bring the PCIe root complex up and identify what sits behind it.
/// A failure means no adapter, never a hang: `clk` reports the clocks
/// and the power partition ready before a register is touched.
/// Bring-up result: the chip and its interrupt, or the line describing
/// where it stopped (reported by `wifi`, since hardware runs have no
/// UART to read the boot log from).
fn hardware() -> Result<(Hw, Irq, String), String> {
    let map = |slot, base, len| {
        Mmio::map(drv::dev_slot(slot), base, len)
            .map_err(|e| format!("register page {} ({})", slot, err::name(e)))
    };
    let regs = map(0, pcie::PCIE_BLOCK, pcie::PCIE_BLOCK_LEN)?;
    let cfg = map(1, pcie::CFG_WINDOW, pcie::CFG_WINDOW_LEN)?;
    let padctl = map(2, pcie::PADCTL, 0x1000)?;
    let misc = map(3, pcie::APB_MISC, 0x1000)?;
    let pinmux = map(4, pcie::PINMUX, 0x1000)?;
    let gpio = map(5, pcie::GPIO, 0x1000)?;
    let rc = pcie::Pcie::new(regs, cfg, padctl, misc, pinmux, gpio);

    let ep = match rc.bring_up() {
        Ok(ep) => ep,
        Err(f) => {
            rc.module_off();
            let why = match f.value {
                Some(v) => format!("{}: {} (read {:#010x})", f.step, err::name(f.err), v),
                None => format!("{}: {}", f.step, err::name(f.err)),
            };
            println!("[wlan] no PCIe device, {}", why);
            return Err(why);
        }
    };
    println!(
        "[wlan] PCIe {:04x}:{:04x} on port {}, BAR0 {:#x}",
        ep.vendor, ep.device, ep.port, ep.bars[0]
    );
    if ep.vendor != chip::VENDOR_BROADCOM {
        return Err(format!(
            "endpoint {:04x}:{:04x} is not Broadcom",
            ep.vendor, ep.device
        ));
    }
    let bar0 = map(6, ep.bars[0], chip::BAR0_WINDOW_LEN)?;
    let c = chip::identify(&bar0, &|addr| rc.ep_cfg_write(chip::CFG_BAR0_WINDOW, addr));
    println!(
        "[wlan] BCM{:x} revision {}, firmware {}",
        c.id,
        c.rev,
        if c.firmware { "present" } else { "missing" }
    );
    if c.id != chip::CHIP_BCM4356 || ep.device != chip::DEVICE_BCM4356 {
        println!(
            "[wlan] unexpected adapter, expected BCM{:x}",
            chip::CHIP_BCM4356
        );
    }
    // Errors reported by the AFI are worth seeing during bring-up.
    let irq = Irq::new(drv::dev_slot(8)).map_err(|e| format!("interrupt ({})", err::name(e)))?;
    let what = format!(
        "BCM{:x} rev {} on PCIe port {}, firmware {}, no data path yet",
        c.id,
        c.rev,
        ep.port,
        if c.firmware { "present" } else { "missing" }
    );
    Ok((Hw { rc }, irq, what))
}

libnx::entry!(main);

fn main() -> i32 {
    let (mut backend, irq, diag) = match hardware() {
        Ok((hw, irq, what)) => (Backend::Chip(hw), Some(irq), what),
        Err(why) => {
            println!("[wlan] simulated adapter, {}", why);
            (
                Backend::Sim(sim::Sim::default()),
                None,
                format!("simulated adapter, {}", why),
            )
        }
    };

    let ep = sys::endpoint_create().expect("wlan: endpoint");
    publish(ep, "wifi", wifi::BADGE_CTRL).expect("wlan: publish");
    // "wlan0" appears only once associated: a station has no carrier
    // before that, and `net` waits for the name rather than polling a
    // link that would answer nothing.
    let mut link_up = false;
    if let Some(i) = irq.as_ref() {
        let _ = sys::notify_bind(i.notification());
    }
    println!("[wlan] Wi-Fi service ready");

    // Frame readers waiting on the link: (reply token, deadline).
    let mut readers: VecDeque<(u64, u64)> = VecDeque::new();
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        // Wake only for the earliest reader deadline: messages and the
        // interrupt wake us on their own.
        let wait = match readers.iter().filter(|r| r.1 != 0).map(|r| r.1).min() {
            Some(d) => d.saturating_sub(time::now_ns()).max(1),
            None => 0,
        };
        match ipc::recv_timeout(ep, wait) {
            Ok(rx) if rx.msg.is_notify() => {
                if let (Some(i), Backend::Chip(hw)) = (irq.as_ref(), &backend) {
                    let code = hw.rc.interrupt();
                    if code != 0 && code != pcie::AFI_INTR_LEGACY {
                        println!("[wlan] PCIe error, AFI code {}", code);
                    }
                    i.ack();
                }
            }
            Ok(rx) => {
                let reply = match (rx.badge, rx.msg.label()) {
                    (wifi::BADGE_LINK, eth::INFO) => {
                        let mut m = Msg::new(0, 2, 0);
                        m.mr[0] = mac_word();
                        m.mr[1] = MTU;
                        Some(m)
                    }
                    // Simulated link: frames are accepted and dropped.
                    (wifi::BADGE_LINK, eth::SEND) => Some(Msg::new(0, 0, 0)),
                    (wifi::BADGE_LINK, eth::RECV) => {
                        let t = rx.msg.mr[0];
                        readers.push_back((rx.token, if t == 0 { 0 } else { time::now_ns() + t }));
                        None
                    }
                    (wifi::BADGE_CTRL, wifi::SCAN) => {
                        let timeout = if rx.msg.mr[0] == 0 {
                            SCAN_TIMEOUT
                        } else {
                            rx.msg.mr[0]
                        };
                        Some(match backend.scan(timeout) {
                            Ok((n, records)) => ipc::pack_at(0, &[n as u64], &records),
                            Err(e) => Msg::error(e),
                        })
                    }
                    (wifi::BADGE_CTRL, wifi::CONNECT) => {
                        // Word 0 is the timeout: the bytes start after it.
                        let n = ipc::unpack_at(&rx.msg, 1, &mut buf);
                        let cut = buf[..n].iter().position(|&b| b == 0).unwrap_or(n);
                        let (ssid, psk) = buf[..n].split_at(cut);
                        let psk = if psk.is_empty() { psk } else { &psk[1..] };
                        Some(match &mut backend {
                            Backend::Sim(s) => match s.connect(ssid, psk) {
                                Ok(()) => {
                                    if !link_up {
                                        match publish(ep, "wlan0", wifi::BADGE_LINK) {
                                            Ok(()) => link_up = true,
                                            Err(e) => println!(
                                                "[wlan] cannot publish the link ({})",
                                                err::name(e)
                                            ),
                                        }
                                    }
                                    Msg::new(0, 0, 0)
                                }
                                Err(e) => Msg::error(e),
                            },
                            Backend::Chip(_) => Msg::error(-ENOSYS),
                        })
                    }
                    (wifi::BADGE_CTRL, wifi::DIAG) => Some(ipc::pack(0, diag.as_bytes())),
                    (wifi::BADGE_CTRL, wifi::DISCONNECT) => {
                        if let Backend::Sim(s) = &mut backend {
                            s.disconnect();
                        }
                        Some(Msg::new(0, 0, 0))
                    }
                    (wifi::BADGE_CTRL, wifi::STATUS) => {
                        let state = backend.state();
                        let (channel, signal, ssid) = backend.joined();
                        Some(ipc::pack_at(0, &[state, channel, signal, mac_word()], ssid))
                    }
                    _ => Some(Msg::error(-EINVAL)),
                };
                if let Some(m) = reply {
                    let _ = ipc::reply(&m);
                }
            }
            Err(e) if e == -ETIMEDOUT => {}
            Err(_) => time::sleep_ms(1),
        }
        let now = time::now_ns();
        readers.retain(|&(token, deadline)| {
            if deadline != 0 && now >= deadline {
                let _ = ipc::reply_to(token, &Msg::error(-ETIMEDOUT));
                false
            } else {
                true
            }
        });
    }
}

/// MAC in a message word, low six bytes first.
fn mac_word() -> u64 {
    let mut b = [0u8; 8];
    b[..6].copy_from_slice(&MAC);
    u64::from_le_bytes(b)
}

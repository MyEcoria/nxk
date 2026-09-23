//! wlan: Wi-Fi station. One endpoint under two names: "wlan0" carries
//! the link (`eth` protocol), "wifi" the station control (`wifi`).
//! Hardware is a BCM4356 behind the Tegra PCIe root complex (`pcie`);
//! without one (emulator) a simulated adapter keeps the protocol, the
//! link and the `wifi` program exercised.

#![no_std]
#![no_main]

extern crate alloc;

mod chip;
mod fw;
mod msgbuf;
mod pcie;
mod sim;
mod wl;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{self, EINVAL, EIO, ENOENT, ENOSYS, ETIMEDOUT};
use libnx::abi::proto::{eth, wifi};
use libnx::abi::rights::{DUP, GRANT, SEND};
use libnx::drv::{self, Irq, Mmio};
use libnx::ipc::{self, Msg};
use libnx::{fs, ns, println, sys, time};

/// Station address while the real one is out of reach: the console
/// keeps it in CAL0 on the eMMC, which nxk cannot read yet. Locally
/// administered, so it collides with no real card.
const MAC: [u8; 6] = [0x02, 0x4E, 0x58, 0x4B, 0x57, 0x01];
const MTU: u64 = 1500;
/// Scan time when the caller asks for no particular one.
const SCAN_TIMEOUT: u64 = 3_000_000_000;
/// Join time likewise: scan, authentication and the key handshake.
const CONNECT_TIMEOUT: u64 = 15_000_000_000;
/// While joined, firmware events are looked at this often at least.
const HOUSEKEEPING_NS: u64 = 500_000_000;

/// The root complex, the chip's BAR0 and TCM, and the rings to its
/// firmware when it runs.
struct Hw {
    rc: pcie::Pcie,
    bar0: Mmio,
    tcm: Mmio,
    mb: Option<msgbuf::Msgbuf>,
    /// Networks of the last scan, with their security: a join needs it.
    seen: Vec<(Vec<u8>, u8)>,
    joined: Option<Joined>,
}

/// The network in use: SSID, channel, signal (-dBm).
struct Joined {
    ssid: Vec<u8>,
    channel: u64,
    signal: u64,
}

impl Hw {
    /// A firmware call, with the backplane view it goes through.
    fn dev<R>(&mut self, f: impl FnOnce(&mut wl::Dev) -> R) -> Option<R> {
        let rc = &self.rc;
        let window = |a| rc.ep_cfg_write(chip::CFG_BAR0_WINDOW, a);
        let bp = chip::Backplane::new(&self.bar0, &window);
        let mb = self.mb.as_mut()?;
        Some(f(&mut wl::Dev {
            mb,
            bp: &bp,
            tcm: &self.tcm,
        }))
    }

    /// Every network in range, remembered with its security.
    fn scan(&mut self, timeout: u64) -> Result<Vec<wl::Bss>, i64> {
        match self.dev(|d| d.scan(timeout)) {
            Some(Ok(found)) => {
                self.seen = found.iter().map(|b| (b.ssid.clone(), b.security)).collect();
                Ok(found)
            }
            Some(Err(e)) => {
                println!("[wlan] scan: {}", e);
                Err(-EIO)
            }
            None => Err(-ENOSYS),
        }
    }

    /// Join by name, secured as the last scan saw it (one is run if the
    /// network was not in it).
    fn connect(&mut self, ssid: &[u8], psk: &[u8], timeout: u64) -> Result<(), i64> {
        let known = |hw: &Hw| hw.seen.iter().find(|s| s.0 == ssid).map(|s| s.1);
        let security = match known(self) {
            Some(s) => s,
            None => {
                self.scan(SCAN_TIMEOUT)?;
                known(self).ok_or(-ENOENT)?
            }
        };
        self.joined = None;
        match self.dev(|d| d.connect(ssid, psk, security, timeout)) {
            Some(Ok(())) => {
                println!("[wlan] joined {}", String::from_utf8_lossy(ssid));
                self.joined = Some(Joined {
                    ssid: ssid.to_vec(),
                    channel: 0,
                    signal: 0,
                });
                self.refresh();
                Ok(())
            }
            Some(Err((e, why))) => {
                println!("[wlan] join {}", why);
                Err(e)
            }
            None => Err(-ENOSYS),
        }
    }

    fn disconnect(&mut self) {
        if self.joined.take().is_some() {
            if let Some(Err(e)) = self.dev(|d| d.disconnect()) {
                println!("[wlan] leave: {}", e);
            }
        }
    }

    /// Channel and signal of the network in use, fresh.
    fn refresh(&mut self) {
        if self.joined.is_none() {
            return;
        }
        if let Some(Ok((channel, dbm))) = self.dev(|d| d.link()) {
            if let Some(j) = self.joined.as_mut() {
                j.channel = channel as u64;
                j.signal = dbm.unsigned_abs() as u64;
            }
        }
    }

    /// Firmware events that came on their own: a lost link ends the join.
    fn housekeeping(&mut self) {
        if self.joined.is_none() {
            return;
        }
        match self.dev(|d| d.housekeeping()) {
            Some(Ok(false)) => {
                println!("[wlan] link lost");
                self.joined = None;
            }
            Some(Err(e)) => println!("[wlan] events: {}", e),
            _ => {}
        }
    }
}

enum Backend {
    /// Chip on the bus; with its firmware running it scans and joins.
    /// The frames of the link are not carried yet.
    Chip(alloc::boxed::Box<Hw>),
    Sim(sim::Sim),
}

impl Backend {
    fn state(&self) -> u64 {
        match self {
            Backend::Chip(hw) if hw.joined.is_some() => wifi::ST_CONNECTED,
            Backend::Chip(hw) if hw.mb.is_some() => wifi::ST_IDLE,
            Backend::Chip(_) => wifi::ST_DOWN,
            Backend::Sim(s) if s.joined.is_some() => wifi::ST_CONNECTED,
            Backend::Sim(_) => wifi::ST_IDLE,
        }
    }

    fn scan(&mut self, timeout: u64) -> Result<(usize, Vec<u8>), i64> {
        match self {
            Backend::Sim(s) => Ok((sim::APS.len(), s.scan())),
            Backend::Chip(hw) => {
                let mut found = hw.scan(timeout)?;
                found.sort_by_key(|b| core::cmp::Reverse(b.rssi));
                let (mut n, mut out) = (0, Vec::new());
                for b in &found {
                    // One reply: what does not fit is the weakest.
                    if out.len() + wifi::RECORD + b.ssid.len() > ipc::MAX_BYTES - 16 {
                        break;
                    }
                    out.push(b.ssid.len() as u8);
                    out.push(b.channel);
                    out.push(b.security);
                    out.push(b.rssi.unsigned_abs().min(255) as u8);
                    out.extend_from_slice(&b.bssid);
                    out.extend_from_slice(&b.ssid);
                    n += 1;
                }
                Ok((n, out))
            }
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
            Backend::Chip(hw) => match &hw.joined {
                Some(j) => (j.channel, j.signal, &j.ssid),
                None => (0, 0, &[]),
            },
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
    let pinmux = map(3, pcie::PINMUX, 0x1000)?;
    let gpio = map(4, pcie::GPIO, 0x1000)?;
    let rc = pcie::Pcie::new(regs, cfg, padctl, pinmux, gpio);

    let ep = match rc.bring_up() {
        Ok(ep) => ep,
        Err(f) => {
            rc.module_off();
            let mut why = match f.value {
                Some(v) => format!("{}: {} (read {:#010x})", f.step, err::name(f.err), v),
                None => format!("{}: {}", f.step, err::name(f.err)),
            };
            if !f.detail.is_empty() {
                why = format!("{} [{}]", why, f.detail);
            }
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
    let bar0 = map(5, ep.bars[0], chip::BAR0_WINDOW_LEN)?;
    let window = |addr| rc.ep_cfg_write(chip::CFG_BAR0_WINDOW, addr);
    let bp = chip::Backplane::new(&bar0, &window);
    let c = chip::identify(&bp);
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
    let irq = Irq::new(drv::dev_slot(7)).map_err(|e| format!("interrupt ({})", err::name(e)))?;
    let mut what = format!("BCM{:x} rev {} on PCIe port {}", c.id, c.rev, ep.port);
    if c.id == 0xFFFF {
        // All ones: the read never reached the chip. The AFI says why.
        let (code, addr) = rc.afi_error();
        what = format!(
            "{} [chip ID read all ones: AFI error {} at {:#x}, BAR0 {:#x}]",
            what, code, addr, ep.bars[0]
        );
    }
    let tcm = match ep.bars[2] {
        0 => bar0,
        a => map(5, a, chip::BAR2_LEN)?,
    };
    let mut mb = None;
    if c.id != 0xFFFF && !c.firmware {
        what.push_str(", firmware missing");
    } else if c.id != 0xFFFF {
        let (text, m) = firmware(&bp, &tcm);
        what.push_str(&text);
        mb = m;
    }
    Ok((
        Hw {
            rc,
            bar0,
            tcm,
            mb,
            seen: Vec::new(),
            joined: None,
        },
        irq,
        what,
    ))
}

/// Boot the chip's firmware from the card: how it went, for `wifi
/// status`, and the rings to it when it runs.
fn firmware(bp: &chip::Backplane, tcm: &Mmio) -> (String, Option<msgbuf::Msgbuf>) {
    let cores = chip::cores(bp);
    let ids: Vec<String> = cores
        .iter()
        .map(|c| format!("{:x}/{}", c.id, c.rev))
        .collect();
    println!("[wlan] {} cores: {}", cores.len(), ids.join(" "));
    let (image, text) = match (fs::read(chip::FW_PATH), fs::read(chip::NVRAM_PATH)) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => {
            return (format!(", firmware unreadable ({})", err::name(e)), None)
        }
    };
    let t0 = time::now_ns();
    match fw::boot(bp, tcm, &cores, &image, &fw::nvram(&text)) {
        Ok(up) => {
            let ms = (time::now_ns() - t0) / 1_000_000;
            println!(
                "[wlan] firmware up in {} ms, shared area {:#x}",
                ms, up.shared
            );
            let (talk, mb) = match chip::find(&cores, chip::CORE_PCIE2) {
                Some(pcie2) => control(bp, tcm, pcie2, &up),
                None => (String::from("no PCIe2 core"), None),
            };
            let text = format!(
                ", firmware up in {} ms (shared v{}, flags {:#x}); {}; console: {}",
                ms,
                up.flags & 0xFF,
                up.flags,
                talk,
                fw::console_tail(tcm, up.console, 200)
            );
            (text, mb)
        }
        Err(e) => {
            println!("[wlan] firmware boot failed: {}", e);
            let text = format!(", firmware boot failed: {} [cores {}]", e, ids.join(" "));
            (text, None)
        }
    }
}

/// Rings up, the firmware's version, then the radio set up for scans.
fn control(
    bp: &chip::Backplane,
    tcm: &Mmio,
    pcie2: chip::Core,
    up: &fw::Up,
) -> (String, Option<msgbuf::Msgbuf>) {
    let mut m = match msgbuf::Msgbuf::attach(bp, tcm, pcie2, up) {
        Ok(m) => m,
        Err(e) => return (format!("rings: {}", e), None),
    };
    let ver = match m.get_var(bp, tcm, "ver", 256) {
        Ok(v) => {
            let end = v
                .iter()
                .position(|&c| c == 0 || c == b'\n')
                .unwrap_or(v.len());
            String::from_utf8_lossy(&v[..end]).into_owned()
        }
        Err(e) => return (format!("ioctl ver: {}", e), None),
    };
    println!("[wlan] firmware: {}", ver);
    let mut dev = wl::Dev {
        mb: &mut m,
        bp,
        tcm,
    };
    let radio = match dev.init(&MAC) {
        Ok(()) => String::from("radio up"),
        Err(e) => format!("radio setup: {}", e),
    };
    println!("[wlan] {}", radio);
    (format!("firmware {}, {}", ver, radio), Some(m))
}

libnx::entry!(main);

fn main() -> i32 {
    let (mut backend, mut irq, mut diag) = match hardware() {
        Ok((hw, irq, what)) => (Backend::Chip(alloc::boxed::Box::new(hw)), Some(irq), what),
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
        let mut wait = match readers.iter().filter(|r| r.1 != 0).map(|r| r.1).min() {
            Some(d) => d.saturating_sub(time::now_ns()).max(1),
            None => 0,
        };
        // Joined: firmware events (a lost link) are looked at regularly.
        if let Backend::Chip(hw) = &mut backend {
            hw.housekeeping();
            if hw.joined.is_some() && (wait == 0 || wait > HOUSEKEEPING_NS) {
                wait = HOUSEKEEPING_NS;
            }
        }
        match ipc::recv_timeout(ep, wait) {
            Ok(rx) if rx.msg.is_notify() => {
                if let (Some(i), Backend::Chip(hw)) = (irq.as_ref(), &backend) {
                    let code = hw.rc.interrupt();
                    if code != 0
                        && code != pcie::AFI_INTR_LEGACY
                        && !pcie::AFI_INTR_BENIGN.contains(&code)
                    {
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
                            Backend::Chip(hw) => {
                                let t = match rx.msg.mr[0] {
                                    0 => CONNECT_TIMEOUT,
                                    t => t,
                                };
                                match hw.connect(ssid, psk, t) {
                                    Ok(()) => Msg::new(0, 0, 0),
                                    Err(e) => Msg::error(e),
                                }
                            }
                        })
                    }
                    (wifi::BADGE_CTRL, wifi::DIAG) => {
                        // The firmware's console as it is now: joins and
                        // failures show there first.
                        let live = match &backend {
                            Backend::Chip(hw) => match &hw.mb {
                                Some(m) => format!(
                                    "; console now: {}",
                                    fw::console_tail(&hw.tcm, m.console, 600)
                                ),
                                None => String::new(),
                            },
                            Backend::Sim(_) => String::new(),
                        };
                        Some(ipc::pack(0, format!("{}{}", diag, live).as_bytes()))
                    }
                    // Bring-up again, on demand: at boot the other
                    // services are still starting, which a single log
                    // line cannot tell apart from a dead register.
                    (wifi::BADGE_CTRL, wifi::RETRY) => {
                        if matches!(backend, Backend::Sim(_)) {
                            match hardware() {
                                Ok((hw, i, what)) => {
                                    let _ = sys::notify_bind(i.notification());
                                    backend = Backend::Chip(alloc::boxed::Box::new(hw));
                                    irq = Some(i);
                                    diag = what;
                                }
                                Err(why) => diag = format!("simulated adapter, {}", why),
                            }
                            println!("[wlan] retry: {}", diag);
                        }
                        Some(ipc::pack(0, diag.as_bytes()))
                    }
                    (wifi::BADGE_CTRL, wifi::DISCONNECT) => {
                        match &mut backend {
                            Backend::Sim(s) => s.disconnect(),
                            Backend::Chip(hw) => hw.disconnect(),
                        }
                        Some(Msg::new(0, 0, 0))
                    }
                    (wifi::BADGE_CTRL, wifi::STATUS) => {
                        if let Backend::Chip(hw) = &mut backend {
                            hw.refresh();
                        }
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

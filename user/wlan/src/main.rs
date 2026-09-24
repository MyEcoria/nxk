//! wlan: Wi-Fi station. One endpoint under two names: "wlan0" carries
//! the link (`eth` protocol), "wifi" the station control (`wifi`).
//! Hardware is a BCM4356 behind the Tegra PCIe root complex (`pcie`);
//! without one (emulator) a simulated adapter keeps the protocol, the
//! link and the `wifi` program exercised.

#![no_std]
#![no_main]

extern crate alloc;

mod chip;
mod crypto;
mod fw;
mod msgbuf;
mod pcie;
mod sim;
mod wl;
mod wpa;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{self, EINVAL, EIO, ENETDOWN, ENOENT, ENOSYS, ETIMEDOUT};
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
/// While joined, the rings are also polled this often: the chip's
/// interrupt drives them, this is the safety net should one be missed.
const POLL_NS: u64 = 500_000_000;
/// How long bring-up waits for `fs` (the card is mounted after `sd`
/// initializes it) before touching the hardware.
const FS_WAIT_NS: u64 = 30_000_000_000;
/// Frames received and not read by `net` yet; older ones are dropped.
const RX_QUEUE: usize = 256;
/// The firmware console is saved this often: once the chip is gone, it
/// is what it said last.
const CONSOLE_SAVE_NS: u64 = 2_000_000_000;
/// A running chip is checked this often (crash flags, bus, config
/// space), joined or not: a loss is caught and undone at once.
const CHECK_NS: u64 = 1_000_000_000;
/// First wait before a recovery or rejoin attempt, doubled after each
/// failure up to the maximum.
const RETRY_FIRST_NS: u64 = 2_000_000_000;
const RETRY_MAX_NS: u64 = 60_000_000_000;

/// The root complex, the chip's BAR0 and TCM, and the rings to its
/// firmware when it runs.
struct Hw {
    rc: pcie::Pcie,
    bar0: Mmio,
    tcm: Mmio,
    mb: Option<msgbuf::Msgbuf>,
    /// Networks of the last scan: a join needs their security.
    seen: Vec<wl::Bss>,
    joined: Option<Joined>,
    /// Frames for `wlan0` readers, oldest first.
    rx: VecDeque<Vec<u8>>,
    /// Chip interrupts taken, deep sleep requests among them, the last
    /// mailbox data, and PCIe errors (count, last AFI code): counted,
    /// not printed, since a failing chip raises them in bursts.
    irqs: u64,
    deep_sleeps: u64,
    mb_data: u32,
    afi_errors: (u64, u32),
    /// The chip was lost (`loss` says how): nothing goes to it until a
    /// recovery, its interrupt line left masked meanwhile.
    gone: bool,
    /// Root port the chip sits behind, and the BARs bring-up gave it: a
    /// recovery must find them again.
    port: usize,
    bars: [u64; 6],
    /// The firmware console as last saved, and when.
    console: (String, u64),
    /// When the running firmware came up, and when to check it next.
    up_at: u64,
    next_check: u64,
    /// Network and passphrase of the last join, kept to rejoin after a
    /// loss; `rejoin` while that is still to do.
    last: Option<(Vec<u8>, Vec<u8>)>,
    rejoin: bool,
    /// Chip losses, the report on the last one, and recoveries.
    losses: u32,
    loss: String,
    recoveries: u32,
    /// Next recovery or rejoin attempt, and the wait after a failure.
    retry_at: u64,
    backoff: u64,
}

/// The network in use: SSID, channel, signal (-dBm), and on a WPA2
/// network the supplicant that answers its rekeys.
struct Joined {
    ssid: Vec<u8>,
    channel: u64,
    signal: u64,
    sup: Option<wpa::Supplicant>,
}

impl Hw {
    /// A firmware call, with the backplane view it goes through.
    fn dev<R>(&mut self, f: impl FnOnce(&mut wl::Dev) -> R) -> Option<R> {
        if self.gone {
            return None;
        }
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
                self.seen = found.clone();
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
        let known = |hw: &Hw| {
            let b = hw.seen.iter().find(|b| b.ssid == ssid)?;
            Some((b.security, b.rsn.clone()))
        };
        let (security, rsn) = match known(self) {
            Some(k) => k,
            None => {
                self.scan(SCAN_TIMEOUT)?;
                known(self).ok_or(-ENOENT)?
            }
        };
        self.joined = None;
        match self.dev(|d| d.connect(ssid, psk, (security, &rsn), &MAC, timeout)) {
            Some(Ok(sup)) => {
                println!("[wlan] joined {}", String::from_utf8_lossy(ssid));
                self.last = Some((ssid.to_vec(), psk.to_vec()));
                self.rejoin = false;
                self.joined = Some(Joined {
                    ssid: ssid.to_vec(),
                    channel: 0,
                    signal: 0,
                    sup,
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
        self.last = None;
        self.rejoin = false;
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

    /// Firmware events and frames that came on their own: rekeys are
    /// answered, data frames queued for `wlan0`, a lost link ends the
    /// join.
    fn housekeeping(&mut self) {
        let Some(mut j) = self.joined.take() else {
            // Not joined, events still come and their buffers must go
            // back to the firmware; frames have no reader.
            let mut rx = VecDeque::new();
            if let Some(Err(e)) = self.dev(|d| d.housekeeping(None, &mut rx)) {
                self.ring_error(&e, false);
            }
            return;
        };
        let mut rx = core::mem::take(&mut self.rx);
        let r = self.dev(|d| d.housekeeping(j.sup.as_mut(), &mut rx));
        while rx.len() > RX_QUEUE {
            rx.pop_front();
        }
        self.rx = rx;
        match r {
            Some(Ok(false)) => {
                println!("[wlan] link lost");
                return;
            }
            Some(Err(e)) => {
                println!("[wlan] link lost: {}", e);
                self.ring_error(&e, true);
                return;
            }
            _ => {}
        }
        self.joined = Some(j);
    }

    /// A ring error: a chip that stopped answering is lost, anything
    /// else only ended the join.
    fn ring_error(&mut self, e: &str, joined: bool) {
        if !self.dev(|d| d.alive()).unwrap_or(true) {
            self.lost(&format!("rings: {}", e), joined);
        }
    }

    /// Periodic look at a running chip, joined or not: the flags its
    /// firmware sets when it dies, a bus reading all ones, a config
    /// space back to reset values. The console is saved meanwhile.
    fn check(&mut self, now: u64) {
        if self.gone || now < self.next_check {
            return;
        }
        self.next_check = now + CHECK_NS;
        let Some((shared, console)) = self.mb.as_ref().map(|m| (m.shared, m.console)) else {
            return;
        };
        let flags = self.tcm.r32(shared as usize);
        let why = if flags == u32::MAX {
            Some(String::from("chip reads all ones"))
        } else if flags & (fw::SHARED_ASSERT | fw::SHARED_TRAP) != 0 {
            Some(fw::crash(&self.tcm, shared, flags))
        } else if !self.rc.ep_decodes() {
            Some(String::from("chip reset itself (memory decoding off)"))
        } else {
            None
        };
        match why {
            Some(why) => {
                let joined = self.joined.take().is_some();
                self.lost(&why, joined);
            }
            None if now > self.console.1 + CONSOLE_SAVE_NS => {
                self.console = (fw::console_tail(&self.tcm, console, 600), now);
            }
            None => {}
        }
    }

    /// The chip stopped: a report of what was seen (printed, and kept for
    /// `wifi status`), the link down, a recovery due shortly.
    fn lost(&mut self, why: &str, joined: bool) {
        let now = time::now_ns();
        self.gone = true;
        self.joined = None;
        self.losses += 1;
        self.rejoin |= joined && self.last.is_some();
        self.loss = format!(
            "{} at {} ms, {} s after the firmware came up, {}; module {}; link: {}; console before: {}",
            why,
            now / 1_000_000,
            now.saturating_sub(self.up_at) / 1_000_000_000,
            if joined { "joined" } else { "not joined" },
            self.rc.module_pins(),
            self.rc.link_state(self.port, false),
            self.console.0
        );
        println!("[wlan] chip lost: {}", self.loss);
        self.backoff = RETRY_FIRST_NS;
        self.retry_at = now + self.backoff;
    }

    /// The chip back after a loss, as `restart wlan` did by hand: module
    /// reset, link, endpoint, firmware. The old rings are freed only once
    /// the module is held in reset: nothing may DMA into them after.
    fn recover(&mut self) -> Result<String, String> {
        let ep = self.rc.restart(self.port);
        self.mb = None;
        let ep = ep.map_err(|f| format!("{}: {}", f.step, err::name(f.err)))?;
        if ep.bars[0] != self.bars[0] || ep.bars[2] != self.bars[2] {
            return Err(format!(
                "BARs moved to {:#x} and {:#x}",
                ep.bars[0], ep.bars[2]
            ));
        }
        let (text, mb) = {
            let rc = &self.rc;
            let window = |a| rc.ep_cfg_write(chip::CFG_BAR0_WINDOW, a);
            let bp = chip::Backplane::new(&self.bar0, &window);
            let c = chip::identify(&bp);
            if c.id != chip::CHIP_BCM4356 || !c.firmware {
                return Err(format!("chip reads as BCM{:x} after the reset", c.id));
            }
            let (text, mb) = firmware(&bp, &self.tcm);
            (
                format!(
                    "BCM{:x} rev {} on PCIe port {}{}",
                    c.id, c.rev, self.port, text
                ),
                mb,
            )
        };
        self.mb = Some(mb.ok_or_else(|| text.clone())?);
        let now = time::now_ns();
        self.gone = false;
        self.up_at = now;
        self.next_check = now + CHECK_NS;
        self.console = (String::new(), 0);
        Ok(text)
    }

    /// A recovery or rejoin attempt, if one is due; a failure doubles the
    /// wait before the next. The bring-up text when the chip came back
    /// (its interrupt line to re-arm).
    fn retry(&mut self, now: u64) -> Option<String> {
        if !(self.gone || self.rejoin) || now < self.retry_at {
            return None;
        }
        let mut back = None;
        let failed = if self.gone {
            match self.recover() {
                Ok(text) => {
                    self.recoveries += 1;
                    println!("[wlan] chip back after loss {}", self.losses);
                    back = Some(text);
                    None
                }
                Err(e) => Some(format!("recovery failed: {}", e)),
            }
        } else {
            None
        };
        let failed = match (failed, self.rejoin, self.last.clone()) {
            (None, true, Some((ssid, psk))) => self
                .connect(&ssid, &psk, CONNECT_TIMEOUT)
                .err()
                .map(|e| format!("rejoin: {}", err::name(e))),
            (f, _, _) => f,
        };
        if let Some(why) = failed {
            self.backoff = (self.backoff * 2).clamp(RETRY_FIRST_NS, RETRY_MAX_NS);
            self.retry_at = time::now_ns() + self.backoff;
            println!(
                "[wlan] {}; next try in {} s",
                why,
                self.backoff / 1_000_000_000
            );
        }
        back
    }

    /// The chip's interrupt (`brcmf_pcie_isr_thread`): causes cleared so
    /// INTx drops before the line is re-armed, mailbox data answered;
    /// the rings are drained by the housekeeping that follows. False when
    /// the line must stay masked: a chip gone from the bus holds it up.
    fn interrupt(&mut self) -> bool {
        if self.gone {
            return false;
        }
        let rc = &self.rc;
        let window = |a| rc.ep_cfg_write(chip::CFG_BAR0_WINDOW, a);
        let bp = chip::Backplane::new(&self.bar0, &window);
        let Some(mb) = self.mb.as_mut() else {
            return true;
        };
        let (status, data) = mb.irq_take(&bp, &self.tcm);
        let shared = mb.shared;
        if status == u32::MAX {
            let joined = self.joined.is_some();
            self.lost("chip reads all ones (interrupt)", joined);
            return false;
        }
        if status != 0 {
            self.irqs += 1;
        }
        if data != 0 {
            self.mb_data = data;
        }
        // Deep sleep requests are counted, not granted (Linux acks them):
        // the ring indices live in the chip's RAM, which a sleeping chip
        // may not serve. None seen so far.
        if data & msgbuf::D2H_DS_ENTER_REQ != 0 {
            self.deep_sleeps += 1;
        }
        if data & msgbuf::D2H_FWHALT != 0 {
            let flags = self.tcm.r32(shared as usize);
            let why = format!("firmware halted: {}", fw::crash(&self.tcm, shared, flags));
            let joined = self.joined.is_some();
            self.lost(&why, joined);
        }
        true
    }

    /// A frame from `wlan0`; dropped while not joined, as a cable would.
    fn send(&mut self, frame: &[u8]) -> Result<(), i64> {
        if self.joined.is_none() {
            return Ok(());
        }
        self.dev(|d| d.send(frame)).unwrap_or(Err(-ENOSYS))
    }
}

enum Backend {
    /// Chip on the bus; with its firmware running it scans, joins (open
    /// and WPA2) and carries the frames of `wlan0`.
    Chip(alloc::boxed::Box<Hw>),
    Sim(sim::Sim),
}

impl Backend {
    fn state(&self) -> u64 {
        match self {
            Backend::Chip(hw) if hw.gone => wifi::ST_DOWN,
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
    // The firmware is on the card, and the PCIe bring-up is kept off the
    // card's own start: at boot `sd` initializes it and `fs` mounts it.
    let _ = ns::wait_for_timeout("fs", FS_WAIT_NS);
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
            rx: VecDeque::new(),
            irqs: 0,
            deep_sleeps: 0,
            mb_data: 0,
            afi_errors: (0, 0),
            gone: false,
            port: ep.port,
            bars: ep.bars,
            console: (String::new(), 0),
            up_at: time::now_ns(),
            next_check: 0,
            last: None,
            rejoin: false,
            losses: 0,
            loss: String::new(),
            recoveries: 0,
            retry_at: 0,
            backoff: 0,
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
    // Features the firmware offers (brcmfmac `brcmf_feat_attach`).
    if let Ok(c) = m.get_var(bp, tcm, "cap", 512) {
        let end = c.iter().position(|&x| x == 0).unwrap_or(c.len());
        println!(
            "[wlan] firmware features: {}",
            String::from_utf8_lossy(&c[..end])
        );
    }
    let mut dev = wl::Dev {
        mb: &mut m,
        bp,
        tcm,
    };
    let radio = match dev.init(&MAC) {
        Ok(()) => String::from("radio up"),
        Err(e) => format!("radio setup: {}", e),
    };
    m.irq_enable(bp);
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
    let publish_link = |link_up: &mut bool| {
        if !*link_up {
            match publish(ep, "wlan0", wifi::BADGE_LINK) {
                Ok(()) => *link_up = true,
                Err(e) => println!("[wlan] cannot publish the link ({})", err::name(e)),
            }
        }
    };
    if let Some(i) = irq.as_ref() {
        let _ = sys::notify_bind(i.notification());
    }
    println!("[wlan] Wi-Fi service ready");

    // Frame readers waiting on the link: (reply token, deadline). A link
    // that went down with no reader waiting is told to the next one.
    let mut readers: VecDeque<(u64, u64)> = VecDeque::new();
    let mut down_untold = false;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let linked = backend.state() == wifi::ST_CONNECTED;
        // Wake only for the earliest reader deadline: messages and the
        // interrupt wake us on their own.
        let mut wait = match readers.iter().filter(|r| r.1 != 0).map(|r| r.1).min() {
            Some(d) => d.saturating_sub(time::now_ns()).max(1),
            None => 0,
        };
        // A lost chip is brought back, a lost join made again; a running
        // chip is checked; joined, the rings are drained (after an
        // interrupt, or by the safety-net poll), frames go to readers.
        if let Backend::Chip(hw) = &mut backend {
            let now = time::now_ns();
            if let Some(text) = hw.retry(now) {
                diag = text;
                if let Some(i) = irq.as_ref() {
                    i.ack();
                }
            }
            hw.check(now);
            hw.housekeeping();
            while !readers.is_empty() {
                let Some(f) = hw.rx.pop_front() else { break };
                let (token, _) = readers.pop_front().unwrap();
                let _ = ipc::reply_to(token, &ipc::pack(0, &f));
            }
            if hw.joined.is_some() && (wait == 0 || wait > POLL_NS) {
                wait = POLL_NS;
            }
            let next = if hw.gone || hw.rejoin {
                hw.retry_at
            } else if hw.mb.is_some() {
                hw.next_check
            } else {
                0
            };
            if next != 0 {
                let d = next.saturating_sub(time::now_ns()).max(1);
                if wait == 0 || d < wait {
                    wait = d;
                }
            }
        }
        match ipc::recv_timeout(ep, wait) {
            Ok(rx) if rx.msg.is_notify() => {
                if let (Some(i), Backend::Chip(hw)) = (irq.as_ref(), &mut backend) {
                    let code = hw.rc.interrupt();
                    if code != 0
                        && code != pcie::AFI_INTR_LEGACY
                        && !pcie::AFI_INTR_BENIGN.contains(&code)
                    {
                        hw.afi_errors = (hw.afi_errors.0 + 1, code);
                    }
                    if hw.interrupt() {
                        i.ack();
                    }
                }
            }
            Ok(rx) => {
                let reply = match (rx.badge, rx.msg.label()) {
                    (wifi::BADGE_LINK, eth::INFO) => {
                        let mut m = Msg::new(0, 3, 0);
                        m.mr[0] = mac_word();
                        m.mr[1] = MTU;
                        m.mr[2] = (backend.state() == wifi::ST_CONNECTED) as u64;
                        Some(m)
                    }
                    (wifi::BADGE_LINK, eth::SEND) => {
                        let n = ipc::unpack(&rx.msg, &mut buf);
                        Some(match &mut backend {
                            // Simulated link: frames are accepted and dropped.
                            Backend::Sim(_) => Msg::new(0, 0, 0),
                            Backend::Chip(_) if !(14..=14 + MTU as usize).contains(&n) => {
                                Msg::error(-EINVAL)
                            }
                            Backend::Chip(hw) => match hw.send(&buf[..n]) {
                                Ok(()) => Msg::new(0, 0, 0),
                                Err(e) => Msg::error(e),
                            },
                        })
                    }
                    (wifi::BADGE_LINK, eth::RECV) if down_untold => {
                        down_untold = false;
                        Some(Msg::error(-ENETDOWN))
                    }
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
                                    publish_link(&mut link_up);
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
                                    Ok(()) => {
                                        publish_link(&mut link_up);
                                        Msg::new(0, 0, 0)
                                    }
                                    Err(e) => Msg::error(e),
                                }
                            }
                        })
                    }
                    (wifi::BADGE_CTRL, wifi::DIAG) => {
                        // The firmware's console as it is now: joins and
                        // failures show there first.
                        let live = match &backend {
                            Backend::Chip(hw) if hw.gone => format!(
                                "; chip lost ({} so far, {} recovered, next try in {} s), interrupts {}, PCIe errors {} (last AFI code {}); {}",
                                hw.losses,
                                hw.recoveries,
                                hw.retry_at.saturating_sub(time::now_ns()) / 1_000_000_000,
                                hw.irqs,
                                hw.afi_errors.0,
                                hw.afi_errors.1,
                                hw.loss
                            ),
                            Backend::Chip(hw) => match &hw.mb {
                                Some(m) => format!(
                                    "; data: {}, interrupts {} (deep sleep asked {}, mailbox data {:#x}), PCIe errors {} (last AFI code {}){}; link: {}; console now: {}",
                                    m.stats(&hw.tcm),
                                    hw.irqs,
                                    hw.deep_sleeps,
                                    hw.mb_data,
                                    hw.afi_errors.0,
                                    hw.afi_errors.1,
                                    match hw.losses {
                                        0 => String::new(),
                                        n => format!(
                                            "; chip lost {} times, {} recovered, last: {}",
                                            n, hw.recoveries, hw.loss
                                        ),
                                    },
                                    hw.rc.link_state(hw.port, true),
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
                    // A chip goes through a recovery, as after a loss.
                    (wifi::BADGE_CTRL, wifi::RETRY) => {
                        match &mut backend {
                            Backend::Sim(_) => {
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
                            Backend::Chip(hw) => {
                                let joined = hw.joined.is_some();
                                hw.lost("recovery asked (wifi retry)", joined);
                                hw.retry_at = 0;
                                if let Some(text) = hw.retry(time::now_ns()) {
                                    diag = text;
                                    if let Some(i) = irq.as_ref() {
                                        i.ack();
                                    }
                                }
                            }
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
        // The link went down (chip lost, network lost, disconnect): the
        // readers hear it, and `net` starts its DHCP over.
        if linked && backend.state() != wifi::ST_CONNECTED {
            down_untold = readers.is_empty();
            for (token, _) in readers.drain(..) {
                let _ = ipc::reply_to(token, &Msg::error(-ENETDOWN));
            }
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

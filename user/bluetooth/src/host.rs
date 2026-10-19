//! The host side: inquiry and pairing (Secure Simple Pairing, Just
//! Works), connections, L2CAP signaling and basic-mode channels, and the
//! HID host on PSM 0x11 / 0x13 speaking the Joy-Con / Pro Controller
//! protocol (dekuNukem's Nintendo_Switch_Reverse_Engineering notes,
//! Linux `hid-nintendo.c`): subcommands, then full reports every 15 ms.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{bt, input};
use libnx::jc;
use libnx::{println, time};

use crate::bcm::{self, Chip};
use crate::hci::{self, le16, Hci, Packet, PB_CONT};
use crate::store::{addr_str, addr_word, Addr, Store};

/// General inquiry access code, 0x9E8B33.
const GIAC: [u8; 3] = [0x33, 0x8B, 0x9E];
/// Inquiry length, in 1.28 s units.
const INQUIRY_LEN: u8 = 4;
/// DM1, DH1, DM3, DH3, DM5, DH5.
const PACKET_TYPES: u16 = 0xCC18;
const IO_NO_INPUT_NO_OUTPUT: u8 = 0x03;
/// General bonding, no MITM protection (Just Works).
const AUTH_GENERAL_BONDING: u8 = 0x04;
const REASON_USER: u8 = 0x13;
const REASON_AUTH_FAILURE: u8 = 0x05;
const REASON_UNACCEPTABLE: u8 = 0x0F;
/// Link key types an unauthenticated (Just Works) pairing gives.
const LINK_KEY_TYPES: u8 = 9;

const CID_SIGNALING: u16 = 0x0001;
const CID_FIRST: u16 = 0x0040;
const SIG_REJECT: u8 = 0x01;
const SIG_CONN_REQ: u8 = 0x02;
const SIG_CONN_RSP: u8 = 0x03;
const SIG_CONF_REQ: u8 = 0x04;
const SIG_CONF_RSP: u8 = 0x05;
const SIG_DISC_REQ: u8 = 0x06;
const SIG_DISC_RSP: u8 = 0x07;
const SIG_ECHO_REQ: u8 = 0x08;
const SIG_ECHO_RSP: u8 = 0x09;
const SIG_INFO_REQ: u8 = 0x0A;
const SIG_INFO_RSP: u8 = 0x0B;
const INFO_FEATURES: u16 = 0x0002;
const INFO_FIXED: u16 = 0x0003;
const CONF_MTU: u8 = 0x01;
const MTU: u16 = 672;
pub const PSM_HID_CONTROL: u16 = 0x0011;
pub const PSM_HID_INTERRUPT: u16 = 0x0013;

/// HID transaction headers: DATA | input, DATA | output.
const HID_DATA_IN: u8 = 0xA1;
const HID_DATA_OUT: u8 = 0xA2;
const OUT_SUBCMD: u8 = 0x01;
const RUMBLE_OFF: [u8; 8] = [0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40];
const SUB_DEVICE_INFO: u8 = 0x02;
const SUB_REPORT_MODE: u8 = 0x03;
const SUB_PLAYER_LIGHTS: u8 = 0x30;
/// Setup: device info, full reports, player 1 light.
/// Setup, in the console's order (dekuNukem's notes, Linux
/// `hid-nintendo`): device info, shipment state, SPI reads (serial,
/// colours, calibration), full reports, motion, vibration, lights.
const SETUP: [(u8, &[u8]); 11] = [
    (SUB_DEVICE_INFO, &[]),
    (SUB_SHIPMENT, &[0x00]),
    (SUB_SPI_READ, &[0x00, 0x60, 0x00, 0x00, 0x10]),
    (SUB_SPI_READ, &[0x50, 0x60, 0x00, 0x00, 0x0D]),
    (SUB_REPORT_MODE, &[jc::REPORT_FULL]),
    (SUB_SPI_READ, &[0x3D, 0x60, 0x00, 0x00, 0x12]),
    (SUB_SPI_READ, &[0x10, 0x80, 0x00, 0x00, 0x18]),
    (SUB_IMU, &[0x01]),
    (SUB_VIBRATION, &[0x01]),
    (SUB_PLAYER_LIGHTS, &[0x01]),
    (SUB_HOME_LIGHT, &[0x01, 0xF0, 0xF0, 0x00]),
];
const SUB_SHIPMENT: u8 = 0x08;
const SUB_SPI_READ: u8 = 0x10;
const SUB_HOME_LIGHT: u8 = 0x38;
/// Motion sensors and vibration on, as Linux `hid-nintendo` sets them up.
const SUB_IMU: u8 = 0x40;
const SUB_VIBRATION: u8 = 0x48;
/// Sniff interval: 24 slots, 15 ms.
const SNIFF_SLOTS: u16 = 24;
const SUBCMD_RETRY_NS: u64 = 300_000_000;
const SUBCMD_TRIES: u8 = 3;
/// A connection that has not reached HID reports by then is dropped.
const SETUP_LIMIT_NS: u64 = 20_000_000_000;
/// A reconnecting controller opens its channels itself; if it has not
/// after this long, we do.
const OPEN_FALLBACK_NS: u64 = 1_500_000_000;
const INQUIRY_PAUSE_NS: u64 = 200_000_000;
/// Inquiry scan and page scan while pairing.
const SCAN_INQUIRY_PAGE: u8 = 0x03;
/// Connection attempts to one controller per pairing window.
const PAGE_TRIES: usize = 4;

struct Chan {
    psm: u16,
    local: u16,
    remote: u16,
    /// Our configuration accepted, theirs answered.
    conf_out: bool,
    conf_in: bool,
    open: bool,
}

struct Conn {
    handle: u16,
    addr: Addr,
    incoming: bool,
    name: String,
    kind: u8,
    encrypted: bool,
    /// A link key came during this connection (new pairing).
    new_key: bool,
    chans: Vec<Chan>,
    next_cid: u16,
    ident: u8,
    rx: Vec<u8>,
    in_flight: u16,
    since: u64,
    open_at: u64,
    /// HID: setup step, its last send and tries, report counter.
    step: usize,
    step_at: u64,
    tries: u8,
    counter: u8,
    ready: bool,
    pad: jc::Pad,
    battery: u8,
    reports: u64,
    /// Last output report sent.
    out_at: u64,
    /// A disconnect was asked: timers leave it alone.
    closing: bool,
}

impl Conn {
    fn new(handle: u16, addr: Addr, incoming: bool) -> Self {
        let now = time::now_ns();
        Conn {
            handle,
            addr,
            incoming,
            name: String::new(),
            kind: bt::KIND_UNKNOWN,
            encrypted: false,
            new_key: false,
            chans: Vec::new(),
            next_cid: CID_FIRST,
            ident: 0,
            rx: Vec::new(),
            in_flight: 0,
            since: now,
            open_at: 0,
            step: 0,
            step_at: 0,
            tries: 0,
            counter: 0,
            ready: false,
            pad: jc::Pad::pro(),
            battery: bt::BATTERY_UNKNOWN,
            reports: 0,
            out_at: 0,
            closing: false,
        }
    }

    fn ident(&mut self) -> u8 {
        self.ident = self.ident.wrapping_add(1).max(1);
        self.ident
    }

    fn label(&self) -> String {
        if self.name.is_empty() {
            addr_str(&self.addr)
        } else {
            self.name.clone()
        }
    }

    fn chan(&self, psm: u16) -> Option<&Chan> {
        self.chans.iter().find(|c| c.psm == psm)
    }
}

/// Kind from the name the controller gives.
fn kind_of(name: &str) -> u8 {
    if name.contains("Joy-Con (L)") {
        bt::KIND_JOYCON_L
    } else if name.contains("Joy-Con (R)") {
        bt::KIND_JOYCON_R
    } else if name.contains("Pro Controller") {
        bt::KIND_PRO
    } else {
        bt::KIND_UNKNOWN
    }
}

pub fn kind_name(k: u8) -> &'static str {
    match k {
        bt::KIND_JOYCON_L => "Joy-Con (L)",
        bt::KIND_JOYCON_R => "Joy-Con (R)",
        bt::KIND_PRO => "Pro Controller",
        _ => "controller",
    }
}

/// The buttons and sticks a kind owns; unknown ones get both halves.
fn pad_for(kind: u8, addr: &Addr) -> jc::Pad {
    let pad = match kind {
        bt::KIND_JOYCON_L => jc::Pad::left(),
        bt::KIND_JOYCON_R => jc::Pad::right(),
        _ => jc::Pad::pro(),
    };
    pad.source(input::SRC_BT | addr_word(addr))
}

/// Peripheral, joystick or gamepad (class of device), or a name that
/// says controller.
fn is_gamepad(class: u32, name: Option<&str>) -> bool {
    let major = (class >> 8) & 0x1F;
    let minor = (class >> 2) & 0x0F;
    major == 0x05 && (minor == 1 || minor == 2)
        || name.is_some_and(|n| n.contains("Joy-Con") || n.contains("Controller"))
}

/// The name in extended inquiry response data, if any.
fn eir_name(mut eir: &[u8]) -> Option<String> {
    while eir.len() >= 2 && eir[0] != 0 {
        let len = eir[0] as usize;
        if eir.len() < 1 + len {
            break;
        }
        if eir[1] == 0x08 || eir[1] == 0x09 {
            return Some(String::from_utf8_lossy(&eir[2..1 + len]).into_owned());
        }
        eir = &eir[1 + len..];
    }
    None
}

pub struct Pairing {
    pub until: u64,
    tried: Vec<Addr>,
    /// Devices the inquiry found, logged once each.
    seen: Vec<Addr>,
}

pub struct Host {
    pub hci: Hci,
    pub chip: Chip,
    pub store: Store,
    conns: Vec<Conn>,
    acl_free: u16,
    acl_queue: VecDeque<(u16, Vec<u8>)>,
    pub pairing: Option<Pairing>,
    inquiring: bool,
    inquiry_at: u64,
    connecting: Option<Addr>,
    /// Create Connection parameters waiting for a name request, and when
    /// it was sent.
    naming: Option<([u8; 13], u64)>,
    /// For `input`.
    pub events: Vec<[u64; input::WORDS]>,
    /// Paired during the current window: address, name.
    pub paired: Vec<(Addr, String)>,
    /// Hardware Error code: the core is dead until a power cycle.
    pub hw_error: Option<u8>,
}

impl Host {
    pub fn new(hci: Hci, chip: Chip, store: Store) -> Self {
        let acl_free = chip.acl_pkts.max(1);
        Host {
            hci,
            chip,
            store,
            conns: Vec::new(),
            acl_free,
            acl_queue: VecDeque::new(),
            pairing: None,
            inquiring: false,
            inquiry_at: 0,
            connecting: None,
            naming: None,
            events: Vec::new(),
            paired: Vec::new(),
            hw_error: None,
        }
    }

    /// Timers running: pairing, a controller being set up, data queued.
    pub fn busy(&self) -> bool {
        self.pairing.is_some() || !self.acl_queue.is_empty() || self.conns.iter().any(|c| !c.ready)
    }

    pub fn connected(&self) -> usize {
        self.conns.iter().filter(|c| c.ready).count()
    }

    /// Paired controllers and connected ones: address, kind, connected,
    /// battery, name.
    pub fn list(&self) -> Vec<(Addr, u8, bool, u8, String)> {
        let mut v: Vec<_> = self
            .store
            .pads
            .iter()
            .map(|p| {
                let c = self.conns.iter().find(|c| c.addr == p.addr && c.ready);
                (
                    p.addr,
                    c.map_or(p.kind, |c| c.kind),
                    c.is_some(),
                    c.map_or(bt::BATTERY_UNKNOWN, |c| c.battery),
                    p.name.clone(),
                )
            })
            .collect();
        for c in self.conns.iter().filter(|c| c.ready) {
            if self.store.find(&c.addr).is_none() {
                v.push((c.addr, c.kind, true, c.battery, c.label()));
            }
        }
        v
    }

    pub fn start_pairing(&mut self, window_ns: u64) {
        let until = time::now_ns() + window_ns;
        match self.pairing.as_mut() {
            Some(p) => p.until = until,
            None => {
                println!(
                    "[bt] pairing for {} s: hold a controller's sync button",
                    window_ns / 1_000_000_000
                );
                self.paired.clear();
                self.pairing = Some(Pairing {
                    until,
                    tried: Vec::new(),
                    seen: Vec::new(),
                });
                // Found and connectable too: in sync mode a controller also
                // looks for a console and connects to it (Horizon's way).
                self.hci
                    .command(hci::OP_WRITE_SCAN_ENABLE, &[SCAN_INQUIRY_PAGE]);
            }
        }
    }

    pub fn stop_pairing(&mut self) {
        if self.pairing.take().is_some() {
            println!("[bt] pairing over, {} paired", self.paired.len());
            self.hci
                .command(hci::OP_WRITE_SCAN_ENABLE, &[bcm::SCAN_PAGE]);
        }
        if self.inquiring {
            self.hci.command(hci::OP_INQUIRY_CANCEL, &[]);
            self.inquiring = false;
        }
    }

    pub fn forget(&mut self, a: Option<Addr>) -> usize {
        let n = self.store.forget(a.as_ref());
        let handles: Vec<u16> = self
            .conns
            .iter()
            .filter(|c| a.is_none_or(|a| a == c.addr))
            .map(|c| c.handle)
            .collect();
        for h in handles {
            self.disconnect(h, REASON_USER);
        }
        n
    }

    fn disconnect(&mut self, handle: u16, reason: u8) {
        if let Some(c) = self.conns.iter_mut().find(|c| c.handle == handle) {
            c.closing = true;
        }
        self.hci.command(
            hci::OP_DISCONNECT,
            &[handle as u8, (handle >> 8) as u8, reason],
        );
    }

    /// Received packets, then timers.
    pub fn poll(&mut self) {
        self.hci.pump();
        while let Some(p) = self.hci.queue.pop_front() {
            match p {
                Packet::Event(code, d) => self.event(code, &d),
                Packet::Acl(h, d) => self.acl_in(h, &d),
            }
        }
        self.tick();
    }

    fn tick(&mut self) {
        let now = time::now_ns();
        if let Some(p) = &self.pairing {
            if now >= p.until {
                self.stop_pairing();
            }
        }
        let setting_up = self.conns.iter().any(|c| !c.ready);
        if self.pairing.is_some()
            && !self.inquiring
            && self.connecting.is_none()
            && !setting_up
            && now >= self.inquiry_at
        {
            let mut p = [0u8; 5];
            p[..3].copy_from_slice(&GIAC);
            p[3] = INQUIRY_LEN;
            self.hci.command(hci::OP_INQUIRY, &p);
            self.inquiring = true;
        }
        let mut drop = Vec::new();
        for i in 0..self.conns.len() {
            let c = &self.conns[i];
            if c.closing {
                continue;
            }
            if !c.ready && now - c.since > SETUP_LIMIT_NS {
                println!(
                    "[bt] {}: no HID reports, disconnecting (channels {:?})",
                    c.label(),
                    c.chans
                        .iter()
                        .map(|ch| (ch.psm, ch.open))
                        .collect::<Vec<_>>()
                );
                drop.push(c.handle);
                continue;
            }
            if c.open_at != 0 && now >= c.open_at {
                self.conns[i].open_at = 0;
                if self.conns[i].chans.is_empty() {
                    self.open_chan(i, PSM_HID_CONTROL);
                }
            }
            let c = &self.conns[i];
            if !c.ready && c.step_at != 0 && now - c.step_at > SUBCMD_RETRY_NS {
                if c.tries >= SUBCMD_TRIES {
                    println!(
                        "[bt] {}: subcommand {:#04x} unanswered",
                        c.label(),
                        SETUP[c.step].0
                    );
                    self.next_step(i);
                } else {
                    self.send_step(i);
                }
            }
        }
        for h in drop {
            self.disconnect(h, REASON_USER);
        }
    }

    // --- HCI events ---

    fn event(&mut self, code: u8, p: &[u8]) {
        match code {
            hci::EV_CMD_COMPLETE if p.len() >= 3 => {
                self.hci.credit(p[0]);
                let op = le16(&p[1..3]);
                if op == hci::OP_INQUIRY_CANCEL {
                    self.inquiring = false;
                }
                if p.len() >= 4 && p[3] != 0 && op != hci::OP_INQUIRY_CANCEL {
                    println!("[bt] command {:#06x}: status {:#04x}", op, p[3]);
                }
            }
            hci::EV_CMD_STATUS if p.len() >= 4 => {
                self.hci.credit(p[1]);
                let op = le16(&p[2..4]);
                if p[0] != 0 {
                    println!("[bt] command {:#06x}: status {:#04x}", op, p[0]);
                    match op {
                        hci::OP_INQUIRY => {
                            self.inquiring = false;
                            self.inquiry_at = time::now_ns() + 1_000_000_000;
                        }
                        hci::OP_CREATE_CONN => self.connecting = None,
                        // Unknown handle: the link is gone already.
                        hci::OP_DISCONNECT if p[0] == 0x02 => {
                            let gone: Vec<u16> = self
                                .conns
                                .iter()
                                .filter(|c| c.closing)
                                .map(|c| c.handle)
                                .collect();
                            for h in gone {
                                self.gone(h, p[0]);
                            }
                        }
                        hci::OP_REMOTE_NAME => {
                            if let Some((n, _)) = self.naming {
                                self.named(&n[..6], p[0], "");
                            }
                        }
                        _ => {}
                    }
                }
            }
            hci::EV_INQUIRY_COMPLETE => {
                self.inquiring = false;
                self.inquiry_at = time::now_ns() + INQUIRY_PAUSE_NS;
            }
            hci::EV_INQUIRY_RESULT if !p.is_empty() => {
                let n = p[0] as usize;
                if p.len() > 14 * n {
                    for i in 0..n {
                        let a = 1 + 6 * i;
                        let class = &p[1 + 9 * n + 3 * i..];
                        let clock = le16(&p[1 + 12 * n + 2 * i..]);
                        self.found(&p[a..a + 6], p[1 + 6 * n + i], class, clock, None);
                    }
                }
            }
            hci::EV_INQUIRY_RESULT_RSSI if !p.is_empty() => {
                let n = p[0] as usize;
                if p.len() > 14 * n {
                    for i in 0..n {
                        let a = 1 + 6 * i;
                        let class = &p[1 + 8 * n + 3 * i..];
                        let clock = le16(&p[1 + 11 * n + 2 * i..]);
                        self.found(&p[a..a + 6], p[1 + 6 * n + i], class, clock, None);
                    }
                }
            }
            hci::EV_EXT_INQUIRY_RESULT if p.len() >= 15 => {
                let name = eir_name(&p[15..]);
                self.found(&p[1..7], p[7], &p[9..12], le16(&p[12..14]), name);
            }
            hci::EV_CONN_REQUEST if p.len() >= 10 => self.conn_request(p),
            hci::EV_CONN_COMPLETE if p.len() >= 11 => self.conn_complete(p),
            hci::EV_DISCONN_COMPLETE if p.len() >= 4 => {
                let h = le16(&p[1..3]) & 0xFFF;
                self.gone(h, p[3]);
            }
            hci::EV_AUTH_COMPLETE if p.len() >= 3 => {
                let h = le16(&p[1..3]) & 0xFFF;
                let Some(i) = self.conns.iter().position(|c| c.handle == h) else {
                    return;
                };
                if p[0] == 0 {
                    self.hci
                        .command(hci::OP_SET_ENCRYPTION, &[h as u8, (h >> 8) as u8, 1]);
                } else {
                    let c = &self.conns[i];
                    println!("[bt] {}: authentication failed ({:#04x})", c.label(), p[0]);
                    self.disconnect(h, REASON_AUTH_FAILURE);
                }
            }
            hci::EV_ENCRYPT_CHANGE if p.len() >= 4 => {
                let h = le16(&p[1..3]) & 0xFFF;
                let Some(i) = self.conns.iter().position(|c| c.handle == h) else {
                    return;
                };
                let c = &mut self.conns[i];
                c.encrypted = p[0] == 0 && p[3] != 0;
                if !c.encrypted {
                    println!("[bt] {}: encryption refused ({:#04x})", c.label(), p[0]);
                }
                if !c.incoming && c.chans.is_empty() {
                    self.open_chan(i, PSM_HID_CONTROL);
                }
            }
            hci::EV_REMOTE_NAME if p.len() >= 7 => {
                let raw = &p[7..];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                let name = String::from_utf8_lossy(&raw[..end]).into_owned();
                let a = p[1..7].to_vec();
                self.named(&a, p[0], &name);
                if p[0] != 0 {
                    return;
                }
                if let Some(c) = self.conns.iter_mut().find(|c| c.addr[..] == p[1..7]) {
                    if c.kind == bt::KIND_UNKNOWN {
                        c.kind = kind_of(&name);
                    }
                    c.name = name.chars().take(bt::MAX_NAME).collect();
                }
            }
            hci::EV_NUM_COMPLETED if !p.is_empty() => {
                let n = p[0] as usize;
                for i in 0..n {
                    let at = 1 + 4 * i;
                    if p.len() < at + 4 {
                        break;
                    }
                    let (h, k) = (le16(&p[at..]) & 0xFFF, le16(&p[at + 2..]));
                    if let Some(c) = self.conns.iter_mut().find(|c| c.handle == h) {
                        c.in_flight = c.in_flight.saturating_sub(k);
                    }
                    self.acl_free = (self.acl_free + k).min(self.chip.acl_pkts.max(1));
                }
                self.acl_flush();
            }
            hci::EV_PIN_REQUEST if p.len() >= 6 => {
                // Legacy pairing (third-party pads): the usual "0000".
                let mut r = [0u8; 23];
                r[..6].copy_from_slice(&p[..6]);
                r[6] = 4;
                r[7..11].copy_from_slice(b"0000");
                self.hci.command(hci::OP_PIN_REPLY, &r);
            }
            hci::EV_LINK_KEY_REQUEST if p.len() >= 6 => {
                let mut a = [0u8; 6];
                a.copy_from_slice(&p[..6]);
                match self.store.find(&a) {
                    Some(k) => {
                        let mut r = [0u8; 22];
                        r[..6].copy_from_slice(&a);
                        r[6..].copy_from_slice(&k.key);
                        self.hci.command(hci::OP_LINK_KEY_REPLY, &r);
                    }
                    None => self.hci.command(hci::OP_LINK_KEY_NEG_REPLY, &a),
                }
            }
            hci::EV_LINK_KEY_NOTIFY if p.len() >= 23 => {
                let mut a = [0u8; 6];
                a.copy_from_slice(&p[..6]);
                let mut key = [0u8; 16];
                key.copy_from_slice(&p[6..22]);
                if p[22] > LINK_KEY_TYPES {
                    println!("[bt] link key type {:#04x}", p[22]);
                }
                let (kind, name) = match self.conns.iter_mut().find(|c| c.addr == a) {
                    Some(c) => {
                        c.new_key = true;
                        (c.kind, c.label())
                    }
                    None => (bt::KIND_UNKNOWN, addr_str(&a)),
                };
                println!("[bt] paired {} ({})", name, addr_str(&a));
                self.store.remember(a, key, kind, &name);
            }
            hci::EV_IO_CAP_REQUEST if p.len() >= 6 => {
                let mut r = [0u8; 9];
                r[..6].copy_from_slice(&p[..6]);
                r[6] = IO_NO_INPUT_NO_OUTPUT;
                r[8] = AUTH_GENERAL_BONDING;
                self.hci.command(hci::OP_IO_CAP_REPLY, &r);
            }
            hci::EV_USER_CONFIRM if p.len() >= 6 => {
                self.hci.command(hci::OP_USER_CONFIRM_REPLY, &p[..6]);
            }
            hci::EV_SSP_COMPLETE if p.len() >= 7 && p[0] != 0 => {
                let mut a = [0u8; 6];
                a.copy_from_slice(&p[1..7]);
                println!("[bt] {}: pairing failed ({:#04x})", addr_str(&a), p[0]);
            }
            hci::EV_HW_ERROR => {
                let code = p.first().copied().unwrap_or(0);
                println!("[bt] controller hardware error {:#04x}, restarting", code);
                self.hw_error = Some(code);
            }
            hci::EV_ROLE_CHANGE | hci::EV_MODE_CHANGE | hci::EV_IO_CAP_RESPONSE => {}
            _ => {}
        }
    }

    fn found(&mut self, addr: &[u8], psrm: u8, class: &[u8], clock: u16, name: Option<String>) {
        let mut a = [0u8; 6];
        a.copy_from_slice(addr);
        let class = class[0] as u32 | (class[1] as u32) << 8 | (class[2] as u32) << 16;
        let Some(pairing) = self.pairing.as_mut() else {
            return;
        };
        if !pairing.seen.contains(&a) {
            pairing.seen.push(a);
            println!(
                "[bt] seen {} class {:#08x} page scan R{} {}",
                addr_str(&a),
                class,
                psrm,
                name.as_deref().unwrap_or("(no name in the inquiry)")
            );
        }
        if self.connecting.is_some()
            || pairing.tried.iter().filter(|t| **t == a).count() >= PAGE_TRIES
            || self.conns.iter().any(|c| c.addr == a)
            || !is_gamepad(class, name.as_deref())
        {
            return;
        }
        let attempt = pairing.tried.iter().filter(|t| **t == a).count();
        pairing.tried.push(a);
        println!(
            "[bt] found {} ({}, class {:#08x})",
            name.as_deref().unwrap_or("a controller"),
            addr_str(&a),
            class
        );
        if self.inquiring {
            self.hci.command(hci::OP_INQUIRY_CANCEL, &[]);
            self.inquiring = false;
        }
        let mut p = [0u8; 13];
        p[..6].copy_from_slice(&a);
        p[6..8].copy_from_slice(&PACKET_TYPES.to_le_bytes());
        // Every other attempt pages blind (R2, no clock offset): both
        // trains in full, for a controller that scans rarely.
        if attempt % 2 == 0 {
            p[8] = psrm;
            p[10..12].copy_from_slice(&(clock | 0x8000).to_le_bytes());
        } else {
            p[8] = 0x02;
        }
        // Allow_Role_Switch, as BlueZ sets it.
        p[12] = 1;
        // Its name first, as BlueZ resolves names before connecting: a
        // Pro Controller paged straight by Create Connection refuses with
        // 0x10 (Erista), the connection riding on the name page works.
        let mut r = [0u8; 10];
        r[..6].copy_from_slice(&a);
        r[6] = p[8];
        r[8..10].copy_from_slice(&p[10..12]);
        self.hci.command(hci::OP_REMOTE_NAME, &r);
        self.naming = Some((p, time::now_ns()));
        self.connecting = Some(a);
    }

    /// The name request before a connection is over: connect.
    fn named(&mut self, a: &[u8], status: u8, name: &str) {
        let Some((p, since)) = self.naming else {
            return;
        };
        if p[..6] != *a {
            return;
        }
        self.naming = None;
        println!(
            "[bt] name request to {}: status {:#04x} after {} ms{}{}",
            addr_str(&p[..6].try_into().unwrap_or([0; 6])),
            status,
            (time::now_ns() - since) / 1_000_000,
            if name.is_empty() { "" } else { ", name " },
            name
        );
        self.hci.command(hci::OP_CREATE_CONN, &p);
    }

    fn conn_request(&mut self, p: &[u8]) {
        let mut a = [0u8; 6];
        a.copy_from_slice(&p[..6]);
        let class = p[6] as u32 | (p[7] as u32) << 8 | (p[8] as u32) << 16;
        let known = self.store.find(&a).is_some();
        if p[9] == 1 && (known || self.pairing.is_some() && is_gamepad(class, None)) {
            let mut r = [0u8; 7];
            r[..6].copy_from_slice(&a);
            // Role 1: stay slave, as BlueZ does: a reconnecting controller
            // leads (role, authentication, encryption, channels).
            r[6] = 0x01;
            self.hci.command(hci::OP_ACCEPT_CONN, &r);
        } else {
            println!(
                "[bt] refused a connection from {} (class {:#08x})",
                addr_str(&a),
                class
            );
            let mut r = [0u8; 7];
            r[..6].copy_from_slice(&a);
            r[6] = REASON_UNACCEPTABLE;
            self.hci.command(hci::OP_REJECT_CONN, &r);
        }
    }

    fn conn_complete(&mut self, p: &[u8]) {
        let mut a = [0u8; 6];
        a.copy_from_slice(&p[3..9]);
        let ours = self.connecting == Some(a);
        if ours {
            self.connecting = None;
        }
        if p[9] != 1 {
            return;
        }
        if p[0] != 0 {
            println!("[bt] {}: connection failed ({:#04x})", addr_str(&a), p[0]);
            return;
        }
        let h = le16(&p[1..3]) & 0xFFF;
        let mut c = Conn::new(h, a, !ours);
        if let Some(k) = self.store.find(&a) {
            c.name = k.name.clone();
            c.kind = k.kind;
        }
        if c.kind == bt::KIND_UNKNOWN {
            c.kind = kind_of(&c.name);
        }
        if c.incoming {
            c.open_at = time::now_ns() + OPEN_FALLBACK_NS;
        }
        println!(
            "[bt] {} connected ({}), handle {:#05x}",
            c.label(),
            if c.incoming {
                "it reconnected"
            } else {
                "pairing"
            },
            h
        );
        if c.name.is_empty() || c.kind == bt::KIND_UNKNOWN {
            let mut r = [0u8; 10];
            r[..6].copy_from_slice(&a);
            r[6] = 0x01;
            self.hci.command(hci::OP_REMOTE_NAME, &r);
        }
        let incoming = c.incoming;
        self.conns.push(c);
        // Pairing: we authenticate. A reconnecting controller does it
        // itself; asking too collides with it.
        if !incoming {
            self.hci
                .command(hci::OP_AUTH_REQUESTED, &[h as u8, (h >> 8) as u8]);
        }
    }

    fn gone(&mut self, handle: u16, reason: u8) {
        let Some(i) = self.conns.iter().position(|c| c.handle == handle) else {
            return;
        };
        let mut c = self.conns.remove(i);
        c.pad.release(&mut self.events);
        self.acl_free = (self.acl_free + c.in_flight).min(self.chip.acl_pkts.max(1));
        self.acl_queue.retain(|(h, _)| *h != handle);
        println!("[bt] {} disconnected (reason {:#04x})", c.label(), reason);
    }

    // --- ACL and L2CAP ---

    fn acl_flush(&mut self) {
        while self.acl_free > 0 {
            let Some((h, d)) = self.acl_queue.pop_front() else {
                break;
            };
            self.acl_free -= 1;
            if let Some(c) = self.conns.iter_mut().find(|c| c.handle == h) {
                c.in_flight += 1;
            }
            self.hci.acl(h, &d);
        }
    }

    fn l2cap_out(&mut self, i: usize, cid: u16, payload: &[u8]) {
        let h = self.conns[i].handle;
        let mut d = Vec::with_capacity(4 + payload.len());
        d.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        d.extend_from_slice(&cid.to_le_bytes());
        d.extend_from_slice(payload);
        if d.len() > self.chip.acl_mtu as usize {
            return;
        }
        self.acl_queue.push_back((h, d));
        self.acl_flush();
    }

    fn signal(&mut self, i: usize, code: u8, ident: u8, data: &[u8]) {
        let mut p = Vec::with_capacity(4 + data.len());
        p.extend_from_slice(&[code, ident]);
        p.extend_from_slice(&(data.len() as u16).to_le_bytes());
        p.extend_from_slice(data);
        self.l2cap_out(i, CID_SIGNALING, &p);
    }

    fn acl_in(&mut self, hdr: u16, data: &[u8]) {
        let (h, pb) = (hdr & 0xFFF, hdr >> 12 & 3);
        let Some(i) = self.conns.iter().position(|c| c.handle == h) else {
            return;
        };
        let c = &mut self.conns[i];
        if pb == PB_CONT {
            c.rx.extend_from_slice(data);
        } else {
            c.rx.clear();
            c.rx.extend_from_slice(data);
        }
        if c.rx.len() > 8192 {
            c.rx.clear();
        }
        loop {
            let c = &mut self.conns[i];
            if c.rx.len() < 4 {
                break;
            }
            let len = le16(&c.rx) as usize;
            if c.rx.len() < 4 + len {
                break;
            }
            let frame: Vec<u8> = c.rx.drain(..4 + len).collect();
            let cid = le16(&frame[2..]);
            if cid == CID_SIGNALING {
                self.signaling(i, &frame[4..]);
            } else if let Some(ch) = self.conns[i].chans.iter().find(|ch| ch.local == cid) {
                if ch.psm == PSM_HID_INTERRUPT {
                    self.hid_in(i, &frame[4..]);
                }
            }
            if i >= self.conns.len() {
                break;
            }
        }
    }

    fn open_chan(&mut self, i: usize, psm: u16) {
        let c = &mut self.conns[i];
        let local = c.next_cid;
        c.next_cid += 1;
        c.chans.push(Chan {
            psm,
            local,
            remote: 0,
            conf_out: false,
            conf_in: false,
            open: false,
        });
        let id = c.ident();
        let mut d = [0u8; 4];
        d[..2].copy_from_slice(&psm.to_le_bytes());
        d[2..].copy_from_slice(&local.to_le_bytes());
        self.signal(i, SIG_CONN_REQ, id, &d);
    }

    fn configure(&mut self, i: usize, remote: u16) {
        let id = self.conns[i].ident();
        let mut d = [0u8; 8];
        d[..2].copy_from_slice(&remote.to_le_bytes());
        d[4] = CONF_MTU;
        d[5] = 2;
        d[6..].copy_from_slice(&MTU.to_le_bytes());
        self.signal(i, SIG_CONF_REQ, id, &d);
    }

    fn signaling(&mut self, i: usize, mut p: &[u8]) {
        while p.len() >= 4 {
            let (code, id, len) = (p[0], p[1], le16(&p[2..]) as usize);
            if p.len() < 4 + len {
                break;
            }
            let d = p[4..4 + len].to_vec();
            p = &p[4 + len..];
            self.sig_command(i, code, id, &d);
            if i >= self.conns.len() {
                return;
            }
        }
    }

    fn sig_command(&mut self, i: usize, code: u8, id: u8, d: &[u8]) {
        match code {
            SIG_CONN_REQ if d.len() >= 4 => {
                let (psm, remote) = (le16(d), le16(&d[2..]));
                let ok = psm == PSM_HID_CONTROL || psm == PSM_HID_INTERRUPT;
                let c = &mut self.conns[i];
                let local = if ok { c.next_cid } else { 0 };
                if ok {
                    c.next_cid += 1;
                    c.chans.retain(|ch| ch.psm != psm);
                    c.chans.push(Chan {
                        psm,
                        local,
                        remote,
                        conf_out: false,
                        conf_in: false,
                        open: false,
                    });
                }
                let mut r = [0u8; 8];
                r[..2].copy_from_slice(&local.to_le_bytes());
                r[2..4].copy_from_slice(&remote.to_le_bytes());
                // Result 2: PSM not supported.
                r[4] = if ok { 0 } else { 2 };
                self.signal(i, SIG_CONN_RSP, id, &r);
                if ok {
                    self.configure(i, remote);
                }
            }
            SIG_CONN_RSP if d.len() >= 8 => {
                let (remote, local, result) = (le16(d), le16(&d[2..]), le16(&d[4..]));
                let c = &mut self.conns[i];
                let Some(k) = c.chans.iter().position(|ch| ch.local == local) else {
                    return;
                };
                match result {
                    0 => {
                        c.chans[k].remote = remote;
                        self.configure(i, remote);
                    }
                    1 => {}
                    _ => {
                        println!(
                            "[bt] {}: PSM {:#06x} refused ({})",
                            c.label(),
                            c.chans[k].psm,
                            result
                        );
                        c.chans.remove(k);
                    }
                }
            }
            SIG_CONF_REQ if d.len() >= 4 => {
                let local = le16(d);
                let c = &mut self.conns[i];
                let Some(k) = c.chans.iter().position(|ch| ch.local == local) else {
                    let mut r = [0u8; 6];
                    r[..2].copy_from_slice(&0x0002u16.to_le_bytes());
                    r[2..4].copy_from_slice(&local.to_le_bytes());
                    self.signal(i, SIG_REJECT, id, &r);
                    return;
                };
                c.chans[k].conf_in = true;
                let remote = c.chans[k].remote;
                let mut r = [0u8; 6];
                r[..2].copy_from_slice(&remote.to_le_bytes());
                self.signal(i, SIG_CONF_RSP, id, &r);
                self.opened(i, k);
            }
            SIG_CONF_RSP if d.len() >= 6 => {
                let (local, result) = (le16(d), le16(&d[4..]));
                let c = &mut self.conns[i];
                let Some(k) = c.chans.iter().position(|ch| ch.local == local) else {
                    return;
                };
                if result != 0 {
                    println!("[bt] {}: configuration result {}", c.label(), result);
                }
                c.chans[k].conf_out = true;
                self.opened(i, k);
            }
            SIG_DISC_REQ if d.len() >= 4 => {
                let local = le16(d);
                self.conns[i].chans.retain(|ch| ch.local != local);
                self.signal(i, SIG_DISC_RSP, id, &d[..4]);
            }
            SIG_ECHO_REQ => self.signal(i, SIG_ECHO_RSP, id, d),
            SIG_INFO_REQ if d.len() >= 2 => {
                let t = le16(d);
                let mut r = Vec::from(&d[..2]);
                match t {
                    INFO_FEATURES => r.extend_from_slice(&[0, 0, 0, 0, 0, 0]),
                    INFO_FIXED => r.extend_from_slice(&[0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0]),
                    _ => r.extend_from_slice(&[1, 0]),
                }
                self.signal(i, SIG_INFO_RSP, id, &r);
            }
            SIG_REJECT | SIG_DISC_RSP | SIG_ECHO_RSP | SIG_INFO_RSP => {}
            _ => self.signal(i, SIG_REJECT, id, &[0, 0]),
        }
    }

    /// Both configurations done: the channel carries data.
    fn opened(&mut self, i: usize, k: usize) {
        let c = &mut self.conns[i];
        let ch = &mut c.chans[k];
        if ch.open || !ch.conf_in || !ch.conf_out {
            return;
        }
        ch.open = true;
        let psm = ch.psm;
        if psm == PSM_HID_CONTROL && c.chan(PSM_HID_INTERRUPT).is_none() && !c.incoming {
            self.open_chan(i, PSM_HID_INTERRUPT);
        } else if psm == PSM_HID_INTERRUPT {
            let c = &mut self.conns[i];
            c.open_at = 0;
            c.pad = pad_for(c.kind, &c.addr);
            c.step = 0;
            c.tries = 0;
            self.send_step(i);
        }
    }

    // --- HID ---

    fn subcmd(&mut self, i: usize, sub: u8, args: &[u8]) {
        let c = &mut self.conns[i];
        let Some(remote) = c
            .chan(PSM_HID_INTERRUPT)
            .filter(|ch| ch.open)
            .map(|ch| ch.remote)
        else {
            return;
        };
        let mut r = [0u8; 50];
        r[0] = HID_DATA_OUT;
        r[1] = OUT_SUBCMD;
        r[2] = c.counter & 0xF;
        c.counter = c.counter.wrapping_add(1);
        r[3..11].copy_from_slice(&RUMBLE_OFF);
        r[11] = sub;
        let n = args.len().min(r.len() - 12);
        r[12..12 + n].copy_from_slice(&args[..n]);
        c.out_at = time::now_ns();
        // The console sends 49-byte output reports.
        self.l2cap_out(i, remote, &r);
    }

    fn send_step(&mut self, i: usize) {
        let (sub, args) = SETUP[self.conns[i].step];
        let c = &mut self.conns[i];
        c.tries += 1;
        c.step_at = time::now_ns();
        self.subcmd(i, sub, args);
    }

    fn next_step(&mut self, i: usize) {
        let c = &mut self.conns[i];
        c.step += 1;
        c.tries = 0;
        c.step_at = 0;
        if c.step < SETUP.len() {
            self.send_step(i);
            return;
        }
        c.ready = true;
        // Sniff at 15 ms, as the console holds its controllers: a
        // third-party Pro Controller streams no report until then.
        let h = c.handle;
        let mut sn = [0u8; 10];
        sn[..2].copy_from_slice(&h.to_le_bytes());
        sn[2..4].copy_from_slice(&SNIFF_SLOTS.to_le_bytes());
        sn[4..6].copy_from_slice(&SNIFF_SLOTS.to_le_bytes());
        sn[6..8].copy_from_slice(&1u16.to_le_bytes());
        self.hci.command(hci::OP_SNIFF_MODE, &sn);
        let c = &mut self.conns[i];
        println!(
            "[bt] {}: ready, {}{}",
            c.label(),
            kind_name(c.kind),
            if c.encrypted { ", encrypted" } else { "" }
        );
        let (a, n, kind) = (c.addr, c.label(), c.kind);
        // Name and kind learnt since the link key was saved.
        if let Some(key) = self
            .store
            .find(&a)
            .filter(|p| p.kind != kind || p.name != n)
            .map(|p| p.key)
        {
            self.store.remember(a, key, kind, &n);
        }
        if c.new_key && self.pairing.is_some() {
            self.paired.push((a, n));
        }
    }

    fn hid_in(&mut self, i: usize, p: &[u8]) {
        if p.len() < 2 || p[0] != HID_DATA_IN {
            return;
        }
        let r = &p[1..];
        let c = &mut self.conns[i];
        if r[0] == jc::REPORT_SUBCMD_REPLY && r.len() >= 15 && !c.ready {
            let sub = r[14];
            if sub == SETUP[c.step].0 {
                if sub == SUB_DEVICE_INFO && r.len() >= 18 {
                    let k = match r[17] {
                        1 => bt::KIND_JOYCON_L,
                        2 => bt::KIND_JOYCON_R,
                        3 => bt::KIND_PRO,
                        _ => c.kind,
                    };
                    if k != c.kind {
                        c.pad.release(&mut self.events);
                        c.kind = k;
                        c.pad = pad_for(k, &c.addr);
                    }
                    println!(
                        "[bt] {}: {}, firmware {}.{:02x}",
                        c.label(),
                        kind_name(c.kind),
                        r[15],
                        r[16]
                    );
                }
                self.next_step(i);
            }
        }
        let c = &mut self.conns[i];
        let Some(rep) = jc::parse(r) else { return };
        c.reports += 1;
        // Some controllers report a level past full (8).
        let battery = (rep.battery >> 1).min(4);
        if c.battery != battery {
            if c.battery == bt::BATTERY_UNKNOWN {
                println!("[bt] {}: battery {}/4", c.label(), battery);
            }
            c.battery = battery;
        }
        if c.ready {
            let who = c.label();
            c.pad.apply(&rep, "bt", &who, &mut self.events);
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{}; {} paired, {} connected",
            self.chip.describe(),
            self.store.pads.len(),
            self.connected()
        )
    }
}

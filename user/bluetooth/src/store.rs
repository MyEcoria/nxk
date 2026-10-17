//! What survives a reboot, in `/config/bt.cfg` on the SD card: the
//! adapter's address and each paired controller's link key.
//!
//! ```text
//! addr 02:4E:58:12:34:56
//! pad 98:B6:E9:01:02:03 <link key, 32 hex digits> <kind> <name>
//! ```

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::bt;
use libnx::{fs, sys};

const DIR: &str = "/config";
const FILE: &str = "/config/bt.cfg";

/// A BD_ADDR in the order HCI carries it (least significant byte first).
pub type Addr = [u8; 6];

pub struct Paired {
    pub addr: Addr,
    pub key: [u8; 16],
    pub kind: u8,
    pub name: String,
}

#[derive(Default)]
pub struct Store {
    pub local: Option<Addr>,
    pub pads: Vec<Paired>,
    /// The file could not be read (no card): nothing is written either,
    /// so a card inserted later keeps its keys.
    pub unreadable: bool,
}

/// Printed most significant byte first, as Bluetooth addresses are.
pub fn addr_str(a: &Addr) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        a[5], a[4], a[3], a[2], a[1], a[0]
    )
}

fn parse_addr(s: &str) -> Option<Addr> {
    let mut a = [0u8; 6];
    let mut parts = s.split(':');
    for i in (0..6).rev() {
        a[i] = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(a)
}

/// The protocol's word for an address (`proto::bt`).
pub fn addr_word(a: &Addr) -> u64 {
    a.iter().rev().fold(0, |w, &b| w << 8 | b as u64)
}

pub fn word_addr(w: u64) -> Addr {
    let mut a = [0u8; 6];
    for (i, b) in a.iter_mut().enumerate() {
        *b = (w >> (8 * i)) as u8;
    }
    a
}

fn parse_key(s: &str) -> Option<[u8; 16]> {
    let mut k = [0u8; 16];
    if s.len() != 32 {
        return None;
    }
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

impl Store {
    pub fn load() -> Self {
        let text = match fs::read(FILE) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            Err(e) if e == -libnx::abi::err::ENOENT && fs::statfs().is_ok() => String::new(),
            Err(_) => {
                return Store {
                    unreadable: true,
                    ..Store::default()
                }
            }
        };
        let mut s = Store::default();
        for line in text.lines() {
            let mut w = line.split_whitespace();
            match w.next() {
                Some("addr") => s.local = w.next().and_then(parse_addr),
                Some("pad") => {
                    let (Some(addr), Some(key), Some(kind)) = (
                        w.next().and_then(parse_addr),
                        w.next().and_then(parse_key),
                        w.next().and_then(|k| k.parse().ok()),
                    ) else {
                        continue;
                    };
                    let name: Vec<&str> = w.collect();
                    s.pads.push(Paired {
                        addr,
                        key,
                        kind,
                        name: name.join(" "),
                    });
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        if self.unreadable {
            return;
        }
        let mut t = String::new();
        if let Some(a) = &self.local {
            t += &format!("addr {}\n", addr_str(a));
        }
        for p in &self.pads {
            let key: String = p.key.iter().map(|b| format!("{:02x}", b)).collect();
            t += &format!("pad {} {} {} {}\n", addr_str(&p.addr), key, p.kind, p.name);
        }
        let _ = fs::mkdir(DIR);
        if let Err(e) = fs::write(FILE, t.as_bytes()) {
            libnx::println!("[bt] cannot save {} ({})", FILE, libnx::abi::err::name(e));
        }
    }

    /// The adapter's address: the saved one, else a new random one
    /// (the console's own is in CAL0 on the eMMC, out of reach), saved.
    pub fn local_addr(&mut self) -> Addr {
        if let Some(a) = self.local {
            return a;
        }
        let mut r = [0u8; 3];
        sys::getrandom(&mut r);
        // Printed 02:4E:58:xx:xx:xx, like the Wi-Fi station's address.
        let a = [r[0], r[1], r[2], 0x58, 0x4E, 0x02];
        self.local = Some(a);
        self.save();
        a
    }

    pub fn find(&self, a: &Addr) -> Option<&Paired> {
        self.pads.iter().find(|p| &p.addr == a)
    }

    pub fn remember(&mut self, addr: Addr, key: [u8; 16], kind: u8, name: &str) {
        self.pads.retain(|p| p.addr != addr);
        let name: String = name
            .chars()
            .filter(|c| !c.is_control())
            .take(bt::MAX_NAME)
            .collect();
        self.pads.push(Paired {
            addr,
            key,
            kind,
            name,
        });
        self.save();
    }

    pub fn forget(&mut self, a: Option<&Addr>) -> usize {
        let n = self.pads.len();
        self.pads.retain(|p| a.is_some_and(|a| &p.addr != a));
        let gone = n - self.pads.len();
        if gone > 0 {
            self.save();
        }
        gone
    }
}

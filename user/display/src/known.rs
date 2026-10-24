//! The Wi-Fi network joined last, in `/config/wifi.cfg` on the SD card
//! (`ssid=` and `psk=` as hex, so any bytes fit on a line): `display`
//! joins it again at boot. The passphrase is in the clear on the card,
//! as the Bluetooth link keys are in `/config/bt.cfg`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::fs;

const DIR: &str = "/config";
const FILE: &str = "/config/wifi.cfg";

/// A network and its passphrase (empty: an open one).
#[derive(Clone, PartialEq)]
pub struct Known {
    pub ssid: Vec<u8>,
    pub psk: Vec<u8>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl Known {
    pub fn text(&self) -> String {
        format!("ssid={}\npsk={}\n", hex(&self.ssid), hex(&self.psk))
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (mut ssid, mut psk) = (None, Vec::new());
        for line in text.lines() {
            match line.split_once('=') {
                Some(("ssid", v)) => ssid = unhex(v.trim()),
                Some(("psk", v)) => psk = unhex(v.trim())?,
                _ => {}
            }
        }
        let ssid = ssid.filter(|s| !s.is_empty() && s.len() <= 32)?;
        if psk.len() > 64 {
            return None;
        }
        Some(Self { ssid, psk })
    }

    pub fn load() -> Option<Self> {
        Self::parse(&String::from_utf8_lossy(&fs::read(FILE).ok()?))
    }

    pub fn save(&self) -> Result<(), i64> {
        let _ = fs::mkdir(DIR);
        fs::write(FILE, self.text().as_bytes())
    }
}

/// The network is forgotten: nothing is joined at boot any more.
pub fn forget() {
    let _ = fs::remove_file(FILE);
}

//! Simulated adapter: what the emulator has instead of a PCIe chip.
//! Enough to exercise the control protocol, the link and the `wifi`
//! program; it carries no traffic.

use alloc::vec::Vec;

use libnx::abi::err::{EACCES, EINVAL, ENOENT, EOPNOTSUPP};
use libnx::abi::proto::wifi;

pub struct Ap {
    pub ssid: &'static str,
    pub channel: u8,
    pub security: u8,
    /// Signal strength in -dBm.
    pub signal: u8,
    pub bssid: [u8; 6],
    /// What a secured network wants (the join is refused otherwise).
    pub psk: &'static str,
}

const fn ap(ssid: &'static str, channel: u8, security: u8, signal: u8, n: u8) -> Ap {
    Ap {
        ssid,
        channel,
        security,
        signal,
        bssid: [0x02, 0x4E, 0x58, 0x4B, 0xA0, n],
        psk: if security == wifi::SEC_OPEN {
            ""
        } else {
            "nxkwifi2026"
        },
    }
}

/// Networks the simulated scan reports: enough to scroll, with a name
/// seen twice, a hidden one, and the kinds `wlan` does not join.
pub const APS: &[Ap] = &[
    ap("nxk-test", 6, wifi::SEC_WPA2, 52, 1),
    ap("nxk-open", 11, wifi::SEC_OPEN, 71, 2),
    ap("Freebox-8C1B", 1, wifi::SEC_WPA2, 58, 3),
    ap("nxk-test", 36, wifi::SEC_WPA2, 80, 4),
    ap("Livebox-4F2A", 11, wifi::SEC_WPA2, 64, 5),
    ap("Cafe Wi-Fi", 6, wifi::SEC_OPEN, 83, 6),
    ap("Old-Router-WEP", 3, wifi::SEC_WEP, 69, 7),
    ap("Legacy-WPA-TKIP", 9, wifi::SEC_WPA, 73, 8),
    ap("WPA3-Only-Net", 44, wifi::SEC_WPA3, 61, 9),
    ap("SFR_A7F0", 52, wifi::SEC_WPA2, 77, 10),
    ap("", 6, wifi::SEC_WPA2, 75, 11),
    ap("Neighbour 5G", 149, wifi::SEC_WPA2, 88, 12),
    ap("Guest", 1, wifi::SEC_WPA2, 91, 13),
    ap(
        "a-name-of-the-full-32-characters",
        13,
        wifi::SEC_WPA2,
        85,
        14,
    ),
    ap("Hotel Lobby", 6, wifi::SEC_OPEN, 79, 15),
    ap("Printer-Direct-3E", 4, wifi::SEC_WPA2, 86, 16),
];

/// Shortest WPA2 passphrase (802.11i).
const MIN_PSK: usize = 8;
/// A join takes this long: the chip's association and key handshake,
/// during which `wlan` answers nobody.
const JOIN_MS: u64 = 1500;

#[derive(Default)]
pub struct Sim {
    pub joined: Option<usize>,
    /// Flight mode.
    pub radio_off: bool,
}

impl Sim {
    pub fn scan(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for ap in APS {
            out.push(ap.ssid.len() as u8);
            out.push(ap.channel);
            out.push(ap.security);
            out.push(ap.signal);
            out.extend_from_slice(&ap.bssid);
            out.extend_from_slice(ap.ssid.as_bytes());
        }
        out
    }

    /// Join by name, with the passphrase a secured network needs.
    pub fn connect(&mut self, ssid: &[u8], psk: &[u8]) -> Result<(), i64> {
        let i = APS
            .iter()
            .position(|a| a.ssid.as_bytes() == ssid)
            .ok_or(-ENOENT)?;
        let secured = APS[i].security != wifi::SEC_OPEN;
        if secured && !(MIN_PSK..=64).contains(&psk.len()) {
            return Err(-EINVAL);
        }
        if !matches!(APS[i].security, wifi::SEC_OPEN | wifi::SEC_WPA2) {
            return Err(-EOPNOTSUPP);
        }
        // As the chip's join: the old network goes first.
        self.joined = None;
        libnx::time::sleep_ms(JOIN_MS);
        if secured && psk != APS[i].psk.as_bytes() {
            return Err(-EACCES);
        }
        self.joined = Some(i);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.joined = None;
    }

    pub fn set_radio(&mut self, on: bool) {
        self.radio_off = !on;
        if !on {
            self.joined = None;
        }
    }
}

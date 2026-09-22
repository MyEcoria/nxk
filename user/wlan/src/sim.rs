//! Simulated adapter: what the emulator has instead of a PCIe chip.
//! Enough to exercise the control protocol, the link and the `wifi`
//! program; it carries no traffic.

use alloc::vec::Vec;

use libnx::abi::err::{EACCES, ENOENT};
use libnx::abi::proto::wifi;

pub struct Ap {
    pub ssid: &'static str,
    pub channel: u8,
    pub security: u8,
    /// Signal strength in -dBm.
    pub signal: u8,
    pub bssid: [u8; 6],
}

/// Networks the simulated scan reports.
pub const APS: &[Ap] = &[
    Ap {
        ssid: "nxk-test",
        channel: 6,
        security: wifi::SEC_WPA2,
        signal: 52,
        bssid: [0x02, 0x4E, 0x58, 0x4B, 0xA0, 0x01],
    },
    Ap {
        ssid: "nxk-open",
        channel: 11,
        security: wifi::SEC_OPEN,
        signal: 71,
        bssid: [0x02, 0x4E, 0x58, 0x4B, 0xA0, 0x02],
    },
];

/// Shortest WPA2 passphrase (802.11i).
const MIN_PSK: usize = 8;

#[derive(Default)]
pub struct Sim {
    pub joined: Option<usize>,
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
        if APS[i].security != wifi::SEC_OPEN && psk.len() < MIN_PSK {
            return Err(-EACCES);
        }
        self.joined = Some(i);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.joined = None;
    }
}

//! Firmware interface of the FullMAC chip (Linux `brcmfmac`: `fwil.c`,
//! `fweh.c`, `cfg80211.c`): radio setup, events, scans.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::wifi;
use libnx::drv::Mmio;
use libnx::time;

use crate::chip::Backplane;
use crate::msgbuf::Msgbuf;

// Commands.
const WLC_UP: u32 = 2;
const WLC_SET_INFRA: u32 = 20;
const WLC_SET_PM: u32 = 86;

// Events (`brcmf_fweh_event_code`) and their status values.
pub const E_SET_SSID: u32 = 0;
pub const E_JOIN: u32 = 1;
pub const E_AUTH: u32 = 3;
pub const E_DEAUTH: u32 = 5;
pub const E_DEAUTH_IND: u32 = 6;
pub const E_ASSOC: u32 = 7;
pub const E_DISASSOC: u32 = 11;
pub const E_DISASSOC_IND: u32 = 12;
pub const E_LINK: u32 = 16;
pub const E_PSK_SUP: u32 = 46;
pub const E_IF: u32 = 54;
pub const E_ESCAN_RESULT: u32 = 69;
const STATUS_PARTIAL: u32 = 8;
/// `event_msgs`: one bit per event code.
const EVENT_MASK_LEN: usize = 16;

/// Broadcom event frames: Ethernet header with this ethertype, then the
/// vendor header and the big-endian event message.
const ETH_P_LINK_CTL: u16 = 0x886C;
const EVENT_MSG: usize = 14 + 10;
const EVENT_DATA: usize = EVENT_MSG + 48;

/// escan request (`brcmf_escan_params_le`): version 1, start.
const ESCAN_VERSION: u32 = 1;
const ESCAN_START: u16 = 1;
const BSSTYPE_ANY: u8 = 2;

// BSS record (`brcmf_bss_info_le`) fields.
const BSS_LENGTH: usize = 4;
const BSS_BSSID: usize = 8;
const BSS_CAPABILITY: usize = 16;
const BSS_SSID_LEN: usize = 18;
const BSS_SSID: usize = 19;
const BSS_CHANSPEC: usize = 72;
const BSS_RSSI: usize = 78;
const BSS_CTL_CH: usize = 88;
const BSS_IE_OFFSET: usize = 116;
const BSS_IE_LENGTH: usize = 120;
const CAP_PRIVACY: u16 = 1 << 4;

/// Chip, backplane and TCM together: what every firmware call needs.
pub struct Dev<'a> {
    pub mb: &'a mut Msgbuf,
    pub bp: &'a Backplane<'a>,
    pub tcm: &'a Mmio,
}

/// One firmware event.
pub struct Event {
    pub kind: u32,
    pub status: u32,
    pub reason: u32,
    pub data: Vec<u8>,
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Event from a frame the firmware sent (`brcmf_fweh_process_skb`).
fn parse_event(f: &[u8]) -> Option<Event> {
    if f.len() < EVENT_DATA || u16::from_be_bytes([f[12], f[13]]) != ETH_P_LINK_CTL {
        return None;
    }
    let len = be32(f, EVENT_MSG + 20) as usize;
    Some(Event {
        kind: be32(f, EVENT_MSG + 4),
        status: be32(f, EVENT_MSG + 8),
        reason: be32(f, EVENT_MSG + 12),
        data: f[EVENT_DATA..(EVENT_DATA + len).min(f.len())].to_vec(),
    })
}

/// A network seen by a scan.
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub security: u8,
    /// Signal in dBm (negative).
    pub rssi: i16,
}

/// Security from the information elements: RSN (WPA2, or WPA3 when SAE
/// is the only key management), WPA, or just the privacy bit (WEP).
fn security(cap: u16, ies: &[u8]) -> u8 {
    let (mut rsn, mut wpa, mut sae_only) = (false, false, false);
    let mut k = 0;
    while k + 2 <= ies.len() {
        let (id, len) = (ies[k], ies[k + 1] as usize);
        let body = &ies[(k + 2).min(ies.len())..(k + 2 + len).min(ies.len())];
        if id == 48 {
            rsn = true;
            // Version, group suite, pairwise count and suites, AKM count.
            let pairwise = if body.len() >= 8 {
                le16(body, 6) as usize
            } else {
                0
            };
            let akm = 8 + 4 * pairwise;
            if body.len() >= akm + 2 {
                let n = le16(body, akm) as usize;
                let suites = &body[(akm + 2).min(body.len())..(akm + 2 + 4 * n).min(body.len())];
                sae_only = n > 0 && suites.chunks(4).all(|s| s == [0x00, 0x0F, 0xAC, 8]);
            }
        } else if id == 221 && body.starts_with(&[0x00, 0x50, 0xF2, 0x01]) {
            wpa = true;
        }
        k += 2 + len;
    }
    match () {
        _ if rsn && sae_only => wifi::SEC_WPA3,
        _ if rsn => wifi::SEC_WPA2,
        _ if wpa => wifi::SEC_WPA,
        _ if cap & CAP_PRIVACY != 0 => wifi::SEC_WEP,
        _ => wifi::SEC_OPEN,
    }
}

/// BSS records of an escan result (`brcmf_escan_result_le`).
fn parse_bss(data: &[u8], out: &mut Vec<Bss>) {
    if data.len() < 12 {
        return;
    }
    let count = le16(data, 10) as usize;
    let mut at = 12;
    for _ in 0..count {
        if at + BSS_IE_LENGTH + 4 > data.len() {
            break;
        }
        let b = &data[at..];
        let len = le32(b, BSS_LENGTH) as usize;
        if len < BSS_IE_LENGTH + 4 || len > b.len() {
            break;
        }
        let ssid_len = (b[BSS_SSID_LEN] as usize).min(wifi::MAX_SSID);
        let ie_off = le16(b, BSS_IE_OFFSET) as usize;
        let ie_len = le32(b, BSS_IE_LENGTH) as usize;
        let ies = b.get(ie_off..ie_off + ie_len).unwrap_or(&[]);
        let channel = match b[BSS_CTL_CH] {
            0 => le16(b, BSS_CHANSPEC) as u8,
            c => c,
        };
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&b[BSS_BSSID..BSS_BSSID + 6]);
        let bss = Bss {
            bssid,
            ssid: b[BSS_SSID..BSS_SSID + ssid_len].to_vec(),
            channel,
            security: security(le16(b, BSS_CAPABILITY), ies),
            rssi: le16(b, BSS_RSSI) as i16,
        };
        // Several results per network: keep the strongest.
        match out.iter_mut().find(|o| o.bssid == bss.bssid) {
            Some(o) if o.rssi < bss.rssi => *o = bss,
            Some(_) => {}
            None => out.push(bss),
        }
        at += len;
    }
}

impl Dev<'_> {
    /// Radio ready for scans and joins (`brcmf_c_preinit_dcmds`,
    /// `brcmf_config_dongle`): our address, the events handled here,
    /// station mode, no power saving, interface up.
    pub fn init(&mut self, mac: &[u8; 6]) -> Result<(), String> {
        let (bp, tcm) = (self.bp, self.tcm);
        self.mb.set_var(bp, tcm, "cur_etheraddr", mac)?;
        let mut mask = [0u8; EVENT_MASK_LEN];
        for e in [
            E_SET_SSID,
            E_JOIN,
            E_AUTH,
            E_DEAUTH,
            E_DEAUTH_IND,
            E_ASSOC,
            E_DISASSOC,
            E_DISASSOC_IND,
            E_LINK,
            E_PSK_SUP,
            E_IF,
            E_ESCAN_RESULT,
        ] {
            mask[e as usize / 8] |= 1 << (e % 8);
        }
        self.mb.set_var(bp, tcm, "event_msgs", &mask)?;
        self.mb.cmd_int(bp, tcm, WLC_SET_INFRA, 1)?;
        self.mb.cmd_int(bp, tcm, WLC_SET_PM, 0)?;
        self.mb.cmd_int(bp, tcm, WLC_UP, 1)
    }

    /// Next firmware event, waiting at most until `deadline` (ns).
    pub fn event(&mut self, deadline: u64) -> Result<Option<Event>, String> {
        loop {
            while let Some(f) = self.mb.event() {
                if let Some(e) = parse_event(&f) {
                    return Ok(Some(e));
                }
            }
            if time::now_ns() > deadline {
                return Ok(None);
            }
            time::sleep_ms(2);
            self.mb.poll(self.bp, self.tcm)?;
        }
    }

    /// All networks in range: an escan on every channel, results
    /// gathered until the firmware reports the scan complete.
    pub fn scan(&mut self, timeout_ns: u64) -> Result<Vec<Bss>, String> {
        let mut req = Vec::with_capacity(72);
        req.extend_from_slice(&ESCAN_VERSION.to_le_bytes());
        req.extend_from_slice(&ESCAN_START.to_le_bytes());
        req.extend_from_slice(&0x4E58u16.to_le_bytes());
        req.extend_from_slice(&[0u8; 36]); // any SSID
        req.extend_from_slice(&[0xFF; 6]); // any BSSID
        req.extend_from_slice(&[BSSTYPE_ANY, 0]); // active scan
        for _ in 0..4 {
            req.extend_from_slice(&u32::MAX.to_le_bytes()); // firmware defaults
        }
        req.extend_from_slice(&0u32.to_le_bytes()); // all channels
        let (bp, tcm) = (self.bp, self.tcm);
        self.mb.set_var(bp, tcm, "escan", &req)?;

        let deadline = time::now_ns() + timeout_ns;
        let mut out = Vec::new();
        while let Some(e) = self.event(deadline)? {
            if e.kind != E_ESCAN_RESULT {
                continue;
            }
            parse_bss(&e.data, &mut out);
            if e.status != STATUS_PARTIAL {
                if e.status != 0 {
                    return Err(format!(
                        "scan ended with status {} (reason {})",
                        e.status, e.reason
                    ));
                }
                return Ok(out);
            }
        }
        Err(format!(
            "scan unfinished after {} ms",
            timeout_ns / 1_000_000
        ))
    }
}

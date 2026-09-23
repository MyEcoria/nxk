//! Firmware interface of the FullMAC chip (Linux `brcmfmac`: `fwil.c`,
//! `fweh.c`, `cfg80211.c`): radio setup, events, scans.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{EACCES, EINVAL, EIO, ENOENT, EOPNOTSUPP, ETIMEDOUT};
use libnx::abi::proto::wifi;
use libnx::drv::Mmio;
use libnx::time;

use crate::chip::Backplane;
use crate::msgbuf::Msgbuf;

// Commands.
const WLC_UP: u32 = 2;
const WLC_SET_INFRA: u32 = 20;
const WLC_GET_CHANNEL: u32 = 29;
const WLC_DISASSOC: u32 = 52;
const WLC_SET_PM: u32 = 86;
const WLC_GET_RSSI: u32 = 127;
const WLC_SET_WSEC_PMK: u32 = 268;

// Security settings (`brcmf_set_wpa_version`, `_set_wsec_mode`).
const WPA_AUTH_PSK: u32 = 0x0004;
const WPA2_AUTH_PSK: u32 = 0x0080;
const TKIP_ENABLED: u32 = 0x2;
const AES_ENABLED: u32 = 0x4;
/// `brcmf_wsec_pmk_le.flags`: the key is the passphrase, the firmware
/// derives the PMK itself.
const WSEC_PASSPHRASE: u16 = 1;
const PMK_LEN: usize = 2 + 2 + 65 + 1;
/// 802.11i passphrase lengths.
const PSK_MIN: usize = 8;
const PSK_MAX: usize = 63;
/// `cap` token of the firmware supplicant (brcmfmac `BRCMF_FEAT_FWSUP`).
const CAP_FWSUP: &[u8] = b"idsup";
/// Reason given when leaving (802.11: station leaving).
const REASON_LEAVING: u32 = 3;

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
/// `E_SET_SSID` statuses (`BRCMF_E_STATUS_*`).
const SET_SSID_FAILURES: [&str; 5] = ["", "failed", "timed out", "no such network", "aborted"];
/// `E_PSK_SUP`: firmware supplicant keyed, or given up.
const SUP_KEYED: u32 = 6;
const SUP_TIMEOUT: u32 = 7;
/// `E_LINK` flag: link up.
const LINK_UP: u16 = 1;
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
    pub flags: u16,
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
        flags: u16::from_be_bytes([f[EVENT_MSG + 2], f[EVENT_MSG + 3]]),
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

    /// Join `ssid` (`brcmf_cfg80211_connect` with the firmware
    /// supplicant): security set from what the network announced, the
    /// passphrase handed to the firmware, then its events followed until
    /// associated and keyed, or the reason it failed.
    pub fn connect(
        &mut self,
        ssid: &[u8],
        psk: &[u8],
        security: u8,
        timeout_ns: u64,
    ) -> Result<(), (i64, String)> {
        self.join(ssid, psk, security, timeout_ns)
            .map_err(|(e, why)| (e, format!("{}: {}", String::from_utf8_lossy(ssid), why)))
    }

    fn join(
        &mut self,
        ssid: &[u8],
        psk: &[u8],
        security: u8,
        timeout_ns: u64,
    ) -> Result<(), (i64, String)> {
        let io = |e: String| (-EIO, e);
        let (bp, tcm) = (self.bp, self.tcm);
        let secured = security != wifi::SEC_OPEN;
        let (wpa_auth, wsec) = match security {
            wifi::SEC_OPEN => (0, 0),
            wifi::SEC_WPA => (WPA_AUTH_PSK, TKIP_ENABLED | AES_ENABLED),
            wifi::SEC_WPA2 => (WPA2_AUTH_PSK, AES_ENABLED | TKIP_ENABLED),
            _ => {
                return Err((
                    -EINVAL,
                    String::from("only open, WPA and WPA2 personal networks"),
                ))
            }
        };
        if secured && !(PSK_MIN..=PSK_MAX).contains(&psk.len()) {
            let why = format!(
                "passphrase of {} characters, {} to {} needed",
                psk.len(),
                PSK_MIN,
                PSK_MAX
            );
            return Err((-EINVAL, why));
        }
        // The console's firmware has no supplicant: Horizon runs its own.
        if secured && !self.has_cap(CAP_FWSUP).map_err(io)? {
            let why = String::from("WPA/WPA2 need a host supplicant, this firmware has none");
            return Err((-EOPNOTSUPP, why));
        }
        self.mb
            .set_var(bp, tcm, "wpa_auth", &wpa_auth.to_le_bytes())
            .map_err(io)?;
        self.mb
            .set_var(bp, tcm, "auth", &0u32.to_le_bytes())
            .map_err(io)?;
        self.mb
            .set_var(bp, tcm, "wsec", &wsec.to_le_bytes())
            .map_err(io)?;
        // Only the firmware supplicant knows `sup_wpa` (brcmfmac sets it with
        // FWSUP alone): without one, even 0 is refused.
        if secured {
            self.mb
                .set_var(bp, tcm, "sup_wpa", &1u32.to_le_bytes())
                .map_err(io)?;
            let mut pmk = [0u8; PMK_LEN];
            pmk[..2].copy_from_slice(&(psk.len() as u16).to_le_bytes());
            pmk[2..4].copy_from_slice(&WSEC_PASSPHRASE.to_le_bytes());
            pmk[4..4 + psk.len()].copy_from_slice(psk);
            let r = self
                .mb
                .ioctl(bp, tcm, WLC_SET_WSEC_PMK, &pmk, 0)
                .map_err(io)?;
            if r.status != 0 {
                return Err(io(format!(
                    "passphrase refused by the firmware ({})",
                    r.status
                )));
            }
        }
        // Stale events (an earlier scan) must not decide this join.
        while self.mb.event().is_some() {}

        // `brcmf_ext_join_params_le`: SSID, join scan with firmware
        // defaults, any BSSID, no channel list.
        let mut join = Vec::with_capacity(68);
        join.extend_from_slice(&(ssid.len() as u32).to_le_bytes());
        let mut name = [0u8; 32];
        name[..ssid.len()].copy_from_slice(ssid);
        join.extend_from_slice(&name);
        join.extend_from_slice(&[0xFF, 0, 0, 0]);
        for _ in 0..4 {
            join.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        join.extend_from_slice(&[0xFF; 6]);
        join.extend_from_slice(&[0, 0]);
        join.extend_from_slice(&0u32.to_le_bytes());
        self.mb.set_var(bp, tcm, "join", &join).map_err(io)?;

        let deadline = time::now_ns() + timeout_ns;
        let (mut associated, mut keyed, mut link) = (false, !secured, false);
        while let Some(e) = self.event(deadline).map_err(io)? {
            match e.kind {
                E_SET_SSID if e.status == 0 => associated = true,
                E_SET_SSID => {
                    let why = SET_SSID_FAILURES
                        .get(e.status as usize)
                        .copied()
                        .unwrap_or("failed");
                    let errno = if e.status == 3 { -ENOENT } else { -EIO };
                    return Err((errno, format!("join {} (status {})", why, e.status)));
                }
                E_PSK_SUP if e.status == SUP_KEYED => keyed = true,
                E_PSK_SUP if e.status == SUP_TIMEOUT => {
                    let why = String::from("key handshake timed out: wrong passphrase?");
                    return Err((-EACCES, why));
                }
                E_LINK => link = e.flags & LINK_UP != 0,
                E_DEAUTH | E_DEAUTH_IND | E_DISASSOC | E_DISASSOC_IND => {
                    let why = format!("sent away by the access point (reason {})", e.reason);
                    return Err((-EACCES, why));
                }
                _ => {}
            }
            if associated && keyed && (link || secured) {
                return Ok(());
            }
        }
        let why = format!(
            "not joined after {} ms (associated {}, keyed {}, link {})",
            timeout_ns / 1_000_000,
            associated,
            keyed,
            link
        );
        Err((-ETIMEDOUT, why))
    }

    /// Whether the firmware's `cap` list names `feature` (`brcmf_feat_attach`).
    fn has_cap(&mut self, feature: &[u8]) -> Result<bool, String> {
        let caps = self.mb.get_var(self.bp, self.tcm, "cap", 512)?;
        Ok(caps.split(|&c| c == b' ' || c == 0).any(|w| w == feature))
    }

    /// Leave the network (`brcmf_link_down`).
    pub fn disconnect(&mut self) -> Result<(), String> {
        let mut scb = [0u8; 12];
        scb[..4].copy_from_slice(&REASON_LEAVING.to_le_bytes());
        self.mb
            .ioctl(self.bp, self.tcm, WLC_DISASSOC, &scb, 0)
            .map(|_| ())
    }

    /// Channel and signal (dBm) of the network in use.
    pub fn link(&mut self) -> Result<(u32, i32), String> {
        let (bp, tcm) = (self.bp, self.tcm);
        let ch = self.mb.ioctl(bp, tcm, WLC_GET_CHANNEL, &[0u8; 12], 12)?;
        let rssi = self.mb.ioctl(bp, tcm, WLC_GET_RSSI, &[0u8; 12], 12)?;
        let channel = if ch.data.len() >= 8 {
            le32(&ch.data, 4)
        } else {
            0
        };
        let dbm = if rssi.data.len() >= 4 {
            le32(&rssi.data, 0) as i32
        } else {
            0
        };
        Ok((channel, dbm))
    }

    /// Events that arrived on their own; false once the link is lost.
    pub fn housekeeping(&mut self) -> Result<bool, String> {
        self.mb.poll(self.bp, self.tcm)?;
        let mut up = true;
        while let Some(f) = self.mb.event() {
            if let Some(e) = parse_event(&f) {
                match e.kind {
                    E_LINK if e.flags & LINK_UP == 0 => up = false,
                    E_DEAUTH | E_DEAUTH_IND | E_DISASSOC | E_DISASSOC_IND => up = false,
                    _ => {}
                }
            }
        }
        Ok(up)
    }
}

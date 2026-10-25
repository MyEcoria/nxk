//! Firmware interface of the FullMAC chip (Linux `brcmfmac`: `fwil.c`,
//! `fweh.c`, `cfg80211.c`): radio setup, events, scans, joins with our
//! own WPA2 supplicant (`wpa`) and the keys it derives.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{EACCES, EAGAIN, EINVAL, EIO, ENOENT, EOPNOTSUPP, ETIMEDOUT};
use libnx::abi::proto::wifi;
use libnx::drv::Mmio;
use libnx::{println, sys, time};

use crate::chip::Backplane;
use crate::crypto;
use crate::msgbuf::Msgbuf;
use crate::wpa::{self, Supplicant};

// Commands.
const WLC_UP: u32 = 2;
const WLC_DOWN: u32 = 3;
const WLC_SET_INFRA: u32 = 20;
const WLC_GET_CHANNEL: u32 = 29;
const WLC_DISASSOC: u32 = 52;
const WLC_GET_PM: u32 = 85;
const WLC_SET_PM: u32 = 86;
const WLC_GET_RSSI: u32 = 127;

// Security settings (`brcmf_set_wpa_version`, `_set_wsec_mode`).
const WPA2_AUTH_PSK: u32 = 0x0080;
const AES_ENABLED: u32 = 0x4;
/// 802.11i passphrase lengths.
const PSK_MIN: usize = 8;
const PSK_MAX: usize = 63;

// `wsec_key` (`brcmf_wsec_key_le`, not packed: 164 bytes).
const KEY_SIZE: usize = 164;
const KEY_LEN: usize = 4;
const KEY_DATA: usize = 8;
const KEY_ALGO: usize = 112;
const KEY_FLAGS: usize = 116;
const KEY_IV_INIT: usize = 132;
const KEY_RXIV: usize = 140;
const KEY_EA: usize = 156;
const CRYPTO_ALGO_AES_CCM: u32 = 4;
/// Key flag of a group key (`BRCMF_PRIMARY_KEY`).
const PRIMARY_KEY: u32 = 1 << 1;
/// Room for the association request's elements (`WL_ASSOC_INFO_MAX`).
const ASSOC_INFO_MAX: usize = 512;
/// A handshake frame has left once the firmware reports it sent.
const TX_TIMEOUT_NS: u64 = 1_000_000_000;
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
/// An EAPOL frame received: how this firmware hands 802.1X to the host,
/// rather than on the data rings as Linux's firmware does.
pub const E_EAPOL_MSG: u32 = 25;
pub const E_IF: u32 = 54;
pub const E_ESCAN_RESULT: u32 = 69;
const STATUS_PARTIAL: u32 = 8;
/// `E_SET_SSID` statuses (`BRCMF_E_STATUS_*`).
const SET_SSID_FAILURES: [&str; 5] = ["", "failed", "timed out", "no such network", "aborted"];
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
const EID_RSN: u8 = 48;

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
    /// The station it concerns (for `E_EAPOL_MSG`, the sender).
    pub addr: [u8; 6],
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
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&f[EVENT_MSG + 24..EVENT_MSG + 30]);
    Some(Event {
        kind: be32(f, EVENT_MSG + 4),
        flags: u16::from_be_bytes([f[EVENT_MSG + 2], f[EVENT_MSG + 3]]),
        status: be32(f, EVENT_MSG + 8),
        reason: be32(f, EVENT_MSG + 12),
        addr,
        data: f[EVENT_DATA..(EVENT_DATA + len).min(f.len())].to_vec(),
    })
}

/// An `E_EAPOL_MSG` event (the EAPOL PDU alone) as the Ethernet frame
/// the supplicant takes: sender, ethertype, then the PDU.
fn eapol_frame(e: &Event) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + e.data.len());
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&e.addr);
    f.extend_from_slice(&wpa::ETH_P_PAE.to_be_bytes());
    f.extend_from_slice(&e.data);
    f
}

/// A network seen by a scan.
#[derive(Clone)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub security: u8,
    /// Signal in dBm (negative).
    pub rssi: i16,
    /// Its RSN element, empty without one.
    pub rsn: Vec<u8>,
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
            rsn: wpa::element(ies, EID_RSN).map_or(Vec::new(), <[u8]>::to_vec),
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
            E_EAPOL_MSG,
            E_IF,
            E_ESCAN_RESULT,
        ] {
            mask[e as usize / 8] |= 1 << (e % 8);
        }
        self.mb.set_var(bp, tcm, "event_msgs", &mask)?;
        self.mb.cmd_int(bp, tcm, WLC_SET_INFRA, 1)?;
        self.mb.cmd_int(bp, tcm, WLC_SET_PM, 0)?;
        self.mb.cmd_int(bp, tcm, WLC_UP, 1)?;
        // The regulatory domain decides which 5 GHz channels are scanned.
        if let Ok(c) = self.mb.get_var(bp, tcm, "country", 12) {
            let abbrev = |b: &[u8]| {
                String::from_utf8_lossy(&b[..b.iter().position(|&x| x == 0).unwrap_or(b.len())])
                    .into_owned()
            };
            if c.len() >= 12 {
                println!(
                    "[wlan] country {:?} (rev {}), code {:?}",
                    abbrev(&c[..4]),
                    le32(&c, 4) as i32,
                    abbrev(&c[8..12])
                );
            }
        }
        // And the channels the firmware offers (`brcmf_construct_chaninfo`).
        if let Ok(l) = self.mb.get_var(bp, tcm, "chanspecs", 1536) {
            if l.len() >= 4 {
                let n = (le32(&l, 0) as usize).min((l.len() - 4) / 4);
                let ch: Vec<(u16, u8)> = (0..n)
                    .map(|i| {
                        let c = le32(&l, 4 + 4 * i) as u16;
                        (c & 0xc000, c as u8)
                    })
                    .collect();
                let g5: Vec<u8> = ch.iter().filter(|c| c.0 == 0xc000).map(|c| c.1).collect();
                println!(
                    "[wlan] {} channels: {} on 2.4 GHz, 5 GHz: {:?}",
                    n,
                    ch.iter().filter(|c| c.0 == 0).count(),
                    g5
                );
            }
        }
        Ok(())
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
                                                  // nprobes, active and passive time per channel (ms), home time: with
                                                  // the firmware's defaults a 5 GHz network (the Freebox, channel 36) was
                                                  // never heard; 200 ms of listening finds it, a scan takes 5 s.
        for v in [3u32, 80, 200, u32::MAX] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&0u32.to_le_bytes()); // all channels
        let (bp, tcm) = (self.bp, self.tcm);
        self.mb.set_var(bp, tcm, "escan", &req)?;

        let start = time::now_ns();
        let deadline = start + timeout_ns;
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
                println!(
                    "[wlan] scan: {} networks, {} on 5 GHz, {} ms",
                    out.len(),
                    out.iter().filter(|b| b.channel > 14).count(),
                    (time::now_ns() - start) / 1_000_000
                );
                return Ok(out);
            }
        }
        Err(format!(
            "scan unfinished after {} ms",
            timeout_ns / 1_000_000
        ))
    }

    /// Join `ssid` (`brcmf_cfg80211_connect`): security set from what
    /// the network announced, then its events and key handshake followed
    /// until associated, linked and keyed, or the reason it failed. A
    /// WPA2 join returns the supplicant, which answers later rekeys.
    pub fn connect(
        &mut self,
        ssid: &[u8],
        psk: &[u8],
        ap: (u8, &[u8]),
        mac: &[u8; 6],
        timeout_ns: u64,
    ) -> Result<Option<Supplicant>, (i64, String)> {
        self.join(ssid, psk, ap, mac, timeout_ns)
            .map_err(|(e, why)| (e, format!("{}: {}", String::from_utf8_lossy(ssid), why)))
    }

    fn join(
        &mut self,
        ssid: &[u8],
        psk: &[u8],
        (security, rsn): (u8, &[u8]),
        mac: &[u8; 6],
        timeout_ns: u64,
    ) -> Result<Option<Supplicant>, (i64, String)> {
        let io = |e: String| (-EIO, e);
        let (bp, tcm) = (self.bp, self.tcm);
        let (wpa_auth, wsec) = match security {
            wifi::SEC_OPEN => (0, 0),
            wifi::SEC_WPA2 => (WPA2_AUTH_PSK, AES_ENABLED),
            wifi::SEC_WPA => {
                let why = String::from("WPA (TKIP) networks are not supported, WPA2 only");
                return Err((-EOPNOTSUPP, why));
            }
            _ => {
                let why = String::from("only open and WPA2 personal networks");
                return Err((-EINVAL, why));
            }
        };
        let mut sup = None;
        if security == wifi::SEC_WPA2 {
            let raw_key = crypto::pmk_from_hex(psk);
            if raw_key.is_none() && !(PSK_MIN..=PSK_MAX).contains(&psk.len()) {
                let why = format!(
                    "passphrase of {} characters, {} to {} needed",
                    psk.len(),
                    PSK_MIN,
                    PSK_MAX
                );
                return Err((-EINVAL, why));
            }
            wpa::check_rsn(rsn).map_err(|why| (-EOPNOTSUPP, String::from(why)))?;
            let pmk = raw_key.unwrap_or_else(|| crypto::psk(psk, ssid));
            sup = Some(Supplicant::new(pmk, *mac, sys::getrandom));
        }
        // Our RSN element for the association request, none for an open
        // network (brcmfmac `wpaie`); the firmware may build its own.
        let ie: &[u8] = if sup.is_some() { &wpa::RSN_IE } else { &[] };
        if let Err(e) = self.mb.set_var(bp, tcm, "wpaie", ie) {
            println!("[wlan] {}", e);
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
        // Stale events and frames (an earlier scan or join) must not
        // decide this one.
        self.mb.poll(bp, tcm).map_err(io)?;
        while self.mb.event().is_some() {}
        while self.mb.frame().is_some() {}

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
        let (mut associated, mut link, mut keyed) = (false, false, sup.is_none());
        let mut ie_read = false;
        loop {
            self.mb.poll(bp, tcm).map_err(io)?;
            let mut eapol = Vec::new();
            while let Some(f) = self.mb.event() {
                let Some(e) = parse_event(&f) else { continue };
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
                    E_LINK => link = e.flags & LINK_UP != 0,
                    E_DEAUTH | E_DEAUTH_IND | E_DISASSOC | E_DISASSOC_IND => {
                        let why = if sup.as_ref().is_some_and(|s| s.unconfirmed()) {
                            format!(
                                "key handshake refused, wrong passphrase? (reason {})",
                                e.reason
                            )
                        } else {
                            format!("sent away by the access point (reason {})", e.reason)
                        };
                        return Err((-EACCES, why));
                    }
                    E_EAPOL_MSG => eapol.push(eapol_frame(&e)),
                    _ => {}
                }
            }
            while let Some(f) = self.mb.frame() {
                if wpa::is_eapol(&f) {
                    eapol.push(f);
                }
            }
            for f in eapol {
                let Some(s) = sup.as_mut() else { continue };
                // Message 2 repeats the RSN element the association
                // request carried: the firmware's, if it built one.
                if !ie_read {
                    ie_read = true;
                    if let Some(ie) = self.assoc_rsn() {
                        if ie != wpa::RSN_IE {
                            println!("[wlan] association RSN element {:02x?}", ie);
                        }
                        s.set_ie(&ie);
                    }
                }
                match s.receive(&f) {
                    Ok(step) => keyed |= self.apply(s, step).map_err(io)?,
                    Err(why) => println!("[wlan] EAPOL-Key frame dropped: {}", why),
                }
            }
            if associated && link && keyed {
                // The firmware may turn power saving back on when it
                // associates: the access point would then hold what it has for
                // us until the next beacon (pings of 100-170 ms).
                let pm = self
                    .mb
                    .ioctl(bp, tcm, WLC_GET_PM, &[0u8; 4], 4)
                    .ok()
                    .filter(|r| r.data.len() >= 4)
                    .map(|r| le32(&r.data, 0));
                println!("[wlan] power save mode {:?} after the join", pm);
                if pm != Some(0) {
                    let _ = self.mb.cmd_int(bp, tcm, WLC_SET_PM, 0);
                }
                return Ok(sup);
            }
            if time::now_ns() > deadline {
                let handshake = match &sup {
                    Some(s) if s.unconfirmed() => ", no message 3: wrong passphrase?",
                    Some(s) if s.aa().is_none() => ", no key handshake started",
                    _ => "",
                };
                let why = format!(
                    "not joined after {} ms (associated {}, link {}, keyed {}{})",
                    timeout_ns / 1_000_000,
                    associated,
                    link,
                    keyed,
                    handshake
                );
                return Err((-ETIMEDOUT, why));
            }
            time::sleep_ms(2);
        }
    }

    /// Our RSN element as the association request carried it
    /// (`brcmf_get_assoc_ies`), when the firmware tells.
    fn assoc_rsn(&mut self) -> Option<Vec<u8>> {
        let (bp, tcm) = (self.bp, self.tcm);
        let info = self
            .mb
            .get_var(bp, tcm, "assoc_info", ASSOC_INFO_MAX)
            .ok()?;
        let len = le32(info.get(..4)?, 0) as usize;
        let ies = self
            .mb
            .get_var(bp, tcm, "assoc_req_ies", ASSOC_INFO_MAX)
            .ok()?;
        wpa::element(&ies[..len.min(ies.len())], EID_RSN).map(<[u8]>::to_vec)
    }

    /// A handshake step carried out: the reply on the air before its
    /// keys are in (`brcmf_netdev_wait_pend8021x`), so message 4 leaves
    /// in the clear. True once a group key is installed.
    fn apply(&mut self, s: &Supplicant, step: wpa::Step) -> Result<bool, String> {
        if let Some(reply) = &step.reply {
            let id = self.mb.send(self.bp, self.tcm, reply)?;
            if step.ptk.is_some() || step.gtk.is_some() {
                self.wait_sent(id)?;
            }
        }
        if let Some(tk) = step.ptk {
            let aa = s
                .aa()
                .ok_or_else(|| String::from("pairwise key without a peer"))?;
            self.add_key(0, &tk, Some(&aa), None)?;
        }
        match step.gtk {
            Some(g) => {
                self.add_key(g.index as u32, &g.key, None, Some(&g.rsc))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn wait_sent(&mut self, id: u32) -> Result<(), String> {
        let deadline = time::now_ns() + TX_TIMEOUT_NS;
        loop {
            if let Some(status) = self.mb.sent(id) {
                if status != 0 {
                    println!("[wlan] handshake frame sent with status {}", status);
                }
                return Ok(());
            }
            if time::now_ns() > deadline {
                return Err(format!(
                    "handshake frame not sent after {} ms",
                    TX_TIMEOUT_NS / 1_000_000
                ));
            }
            time::sleep_us(200);
            self.mb.poll(self.bp, self.tcm)?;
        }
    }

    /// A CCMP key to the firmware (`brcmf_cfg80211_add_key`): pairwise
    /// for the access point, or a group key by index with its receive
    /// counter.
    fn add_key(
        &mut self,
        index: u32,
        key: &[u8; 16],
        peer: Option<&[u8; 6]>,
        rsc: Option<&[u8; 6]>,
    ) -> Result<(), String> {
        let mut k = [0u8; KEY_SIZE];
        k[..4].copy_from_slice(&index.to_le_bytes());
        k[KEY_LEN..KEY_LEN + 4].copy_from_slice(&(key.len() as u32).to_le_bytes());
        k[KEY_DATA..KEY_DATA + key.len()].copy_from_slice(key);
        k[KEY_ALGO..KEY_ALGO + 4].copy_from_slice(&CRYPTO_ALGO_AES_CCM.to_le_bytes());
        match peer {
            Some(ea) => k[KEY_EA..KEY_EA + 6].copy_from_slice(ea),
            None => k[KEY_FLAGS..KEY_FLAGS + 4].copy_from_slice(&PRIMARY_KEY.to_le_bytes()),
        }
        if let Some(pn) = rsc {
            // Receive IV: bits 16-47 of the packet number, then 0-15.
            k[KEY_RXIV..KEY_RXIV + 4].copy_from_slice(&pn[2..6]);
            k[KEY_RXIV + 4..KEY_RXIV + 6].copy_from_slice(&pn[..2]);
            k[KEY_IV_INIT] = 1;
        }
        self.mb.set_var(self.bp, self.tcm, "wsec_key", &k)
    }

    /// The chip still answers on the bus (a register read not all ones).
    pub fn alive(&self) -> bool {
        self.mb.alive(self.bp)
    }

    /// A frame from `wlan0` onto the air: `EAGAIN` while every TX buffer
    /// is in flight, for the sender to retry.
    pub fn send(&mut self, frame: &[u8]) -> Result<(), i64> {
        let (bp, tcm) = (self.bp, self.tcm);
        let failed = |e: String| {
            println!("[wlan] send: {}", e);
            -EIO
        };
        self.mb.poll(bp, tcm).map_err(failed)?;
        if !self.mb.tx_free() {
            return Err(-EAGAIN);
        }
        self.mb.send(bp, tcm, frame).map(|_| ()).map_err(failed)
    }

    /// Radio on (`WLC_UP`) or off (`WLC_DOWN`, as `brcmf_cfg80211_down`):
    /// off, the chip neither scans nor joins.
    pub fn radio(&mut self, on: bool) -> Result<(), String> {
        let cmd = if on { WLC_UP } else { WLC_DOWN };
        self.mb.cmd_int(self.bp, self.tcm, cmd, 1)
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

    /// Events and frames that arrived on their own: key handshakes (group
    /// rekeys) answered, the other frames queued on `rx` for `wlan0`.
    /// False once the link is lost.
    pub fn housekeeping(
        &mut self,
        mut sup: Option<&mut Supplicant>,
        rx: &mut VecDeque<Vec<u8>>,
    ) -> Result<bool, String> {
        self.mb.poll(self.bp, self.tcm)?;
        let mut up = true;
        let mut eapol = Vec::new();
        while let Some(f) = self.mb.event() {
            if let Some(e) = parse_event(&f) {
                match e.kind {
                    E_LINK if e.flags & LINK_UP == 0 => up = false,
                    E_DEAUTH | E_DEAUTH_IND | E_DISASSOC | E_DISASSOC_IND => up = false,
                    E_EAPOL_MSG => eapol.push(eapol_frame(&e)),
                    _ => {}
                }
            }
        }
        while let Some(f) = self.mb.frame() {
            if wpa::is_eapol(&f) {
                eapol.push(f);
            } else {
                rx.push_back(f);
            }
        }
        for f in eapol {
            let Some(s) = sup.as_deref_mut() else {
                continue;
            };
            match s.receive(&f) {
                Ok(step) => {
                    if self.apply(s, step)? {
                        println!("[wlan] group key renewed");
                    }
                }
                Err(why) => println!("[wlan] EAPOL-Key frame dropped: {}", why),
            }
        }
        Ok(up)
    }
}

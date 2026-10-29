//! WPA2-Personal supplicant (802.11-2016 12.7.6, 12.7.7): the 4-way and
//! group key handshakes over EAPOL-Key frames, CCMP with a PSK only.
//! The console's firmware has no supplicant of its own (Horizon runs
//! one): frames go through the data rings and the keys derived here go
//! back to the firmware, which does the encryption. No I/O here.

use alloc::vec::Vec;

use crate::crypto;

pub const ETH_P_PAE: u16 = 0x888E;
const ETH_HLEN: usize = 14;
const EAPOL_KEY: u8 = 3;
/// Key descriptor type: RSN (IEEE 802.11).
const DESC_RSN: u8 = 2;

// Key information bits (big-endian on the wire).
const KI_VERSION: u16 = 0x0007;
/// Descriptor version 2: HMAC-SHA1-128 MIC, AES key wrap.
const VERSION_AES: u16 = 2;
const KI_PAIRWISE: u16 = 1 << 3;
const KI_INSTALL: u16 = 1 << 6;
const KI_ACK: u16 = 1 << 7;
const KI_MIC: u16 = 1 << 8;
const KI_SECURE: u16 = 1 << 9;
const KI_ENCRYPTED: u16 = 1 << 12;

// EAPOL-Key frame from the EAPOL header: version, type, body length,
// then the key descriptor.
const EAPOL_HDR: usize = 4;
const K_INFO: usize = EAPOL_HDR + 1;
const K_REPLAY: usize = EAPOL_HDR + 5;
const K_NONCE: usize = EAPOL_HDR + 13;
const K_RSC: usize = EAPOL_HDR + 61;
const K_MIC: usize = EAPOL_HDR + 77;
const K_DATA_LEN: usize = EAPOL_HDR + 93;
const K_DATA: usize = EAPOL_HDR + 95;
const MIC_LEN: usize = 16;

/// PTK for CCMP: KCK (MIC), KEK (key wrap), then TK.
const PTK_LEN: usize = 48;
const TK_LEN: usize = 16;

const EID_RSN: u8 = 48;
const EID_VENDOR: u8 = 221;
const OUI_IEEE: [u8; 3] = [0x00, 0x0F, 0xAC];
const SUITE_CCMP: [u8; 4] = [0x00, 0x0F, 0xAC, 4];
pub const SUITE_1X: [u8; 4] = [0x00, 0x0F, 0xAC, 1];
const SUITE_PSK: [u8; 4] = [0x00, 0x0F, 0xAC, 2];
pub const SUITE_1X_SHA256: [u8; 4] = [0x00, 0x0F, 0xAC, 5];
/// KDE data type of a group key.
const KDE_GTK: u8 = 1;
/// RSN capabilities: management frame protection required.
const RSN_MFPR: u16 = 1 << 6;

/// Our RSN element: version 1, CCMP group and pairwise, PSK, no
/// capabilities (no management frame protection).
pub const RSN_IE: [u8; 22] = [
    EID_RSN, 20, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 2,
    0, 0,
];

/// Our RSN element for WPA2-Enterprise (802.1X AKM `00-0F-AC:1`).
pub const RSN_IE_EAP: [u8; 22] = [
    EID_RSN, 20, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 4, 1, 0, 0x00, 0x0F, 0xAC, 1,
    0, 0,
];

/// A group key to install: key index, key, receive sequence counter.
#[derive(Clone, PartialEq, Debug)]
pub struct Gtk {
    pub index: u8,
    pub key: [u8; TK_LEN],
    pub rsc: [u8; 6],
}

/// What a frame calls for: a reply, then keys to install once it is
/// on the air (message 4 must leave unencrypted).
#[derive(Default)]
pub struct Step {
    pub reply: Option<Vec<u8>>,
    pub ptk: Option<[u8; TK_LEN]>,
    pub gtk: Option<Gtk>,
}

pub struct Supplicant {
    pmk: [u8; 32],
    spa: [u8; 6],
    /// The access point, from its first message 1.
    aa: Option<[u8; 6]>,
    /// Our RSN element as the association request carried it.
    ie: Vec<u8>,
    random: fn(&mut [u8]),
    anonce: [u8; 32],
    snonce: [u8; 32],
    /// PTK derived from message 1, not yet confirmed by message 3.
    tptk: Option<[u8; PTK_LEN]>,
    ptk: Option<[u8; PTK_LEN]>,
    /// Last replay counter of a frame whose MIC checked out.
    replay: Option<[u8; 8]>,
    gtk: Option<Gtk>,
}

fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// An EAPOL frame, whatever its type.
pub fn is_eapol(frame: &[u8]) -> bool {
    frame.len() >= ETH_HLEN && be16(frame, 12) == ETH_P_PAE
}

/// First element `id` among `ies`, header included.
pub fn element(ies: &[u8], id: u8) -> Option<&[u8]> {
    let mut k = 0;
    while k + 2 <= ies.len() {
        let end = k + 2 + ies[k + 1] as usize;
        if end > ies.len() {
            return None;
        }
        if ies[k] == id {
            return Some(&ies[k..end]);
        }
        k = end;
    }
    None
}

/// Whether we can join a network announcing `rsn` (its RSN element):
/// CCMP for both ciphers, PSK among the key managements, and management
/// frame protection not required.
pub fn check_rsn(rsn: &[u8]) -> Result<(), &'static str> {
    check_rsn_suite(
        rsn,
        &SUITE_PSK,
        "no PSK key management (WPA3-only or enterprise)",
    )
}

/// Whether we can join a WPA2-Enterprise network announcing `rsn`:
/// CCMP ciphers, 802.1X (`00-0F-AC:1`) among AKMs, and MFPR not set.
pub fn check_rsn_eap(rsn: &[u8]) -> Result<(), &'static str> {
    check_rsn_suite(rsn, &SUITE_1X, "no 802.1X (SHA-1) key management")
}

fn check_rsn_suite(
    rsn: &[u8],
    want_akm: &[u8; 4],
    akm_err: &'static str,
) -> Result<(), &'static str> {
    let b = rsn.get(2..).ok_or("no RSN element")?;
    if b.len() < 8 || le16(b, 0) != 1 {
        return Err("unknown RSN version");
    }
    if b[2..6] != SUITE_CCMP {
        return Err("group cipher is not CCMP (WPA/WPA2 mixed mode)");
    }
    let suites = |at: usize| -> Option<&[u8]> {
        let n = le16(b, at) as usize;
        b.get(at + 2..at + 2 + 4 * n)
    };
    let pairwise = suites(6).ok_or("truncated RSN element")?;
    if !pairwise.chunks(4).any(|s| s == SUITE_CCMP) {
        return Err("no CCMP pairwise cipher");
    }
    let akm_at = 8 + pairwise.len();
    let akm = b.get(akm_at..akm_at + 2).and(suites(akm_at));
    if !akm.is_some_and(|a| a.chunks(4).any(|s| s == want_akm)) {
        return Err(akm_err);
    }
    let caps_at = akm_at + 2 + akm.map_or(0, |a| a.len());
    if b.len() >= caps_at + 2 && le16(b, caps_at) & RSN_MFPR != 0 {
        return Err("management frame protection required");
    }
    Ok(())
}

/// The fields of a received EAPOL-Key frame; `eapol` runs from the
/// EAPOL header to the end of the key data, the extent of the MIC.
struct Key<'a> {
    src: [u8; 6],
    version: u8,
    info: u16,
    replay: [u8; 8],
    nonce: &'a [u8],
    rsc: &'a [u8],
    mic: &'a [u8],
    data: &'a [u8],
    eapol: &'a [u8],
}

fn parse(frame: &[u8]) -> Result<Key<'_>, &'static str> {
    if !is_eapol(frame) {
        return Err("not an EAPOL frame");
    }
    let e = &frame[ETH_HLEN..];
    if e.len() < K_DATA || e[1] != EAPOL_KEY || e[EAPOL_HDR] != DESC_RSN {
        return Err("not an RSN EAPOL-Key frame");
    }
    // Frames are padded to the Ethernet minimum: the header says where
    // the EAPOL body ends.
    let e = e
        .get(..EAPOL_HDR + be16(e, 2) as usize)
        .filter(|e| e.len() >= K_DATA)
        .ok_or("truncated EAPOL-Key frame")?;
    let data = e
        .get(K_DATA..K_DATA + be16(e, K_DATA_LEN) as usize)
        .ok_or("truncated key data")?;
    let info = be16(e, K_INFO);
    if info & KI_VERSION != VERSION_AES {
        return Err("key descriptor is not version 2 (HMAC-SHA1, AES)");
    }
    let mut src = [0u8; 6];
    src.copy_from_slice(&frame[6..12]);
    Ok(Key {
        src,
        version: e[0],
        info,
        replay: e[K_REPLAY..K_REPLAY + 8].try_into().unwrap(),
        nonce: &e[K_NONCE..K_NONCE + 32],
        rsc: &e[K_RSC..K_RSC + 8],
        mic: &e[K_MIC..K_MIC + MIC_LEN],
        data,
        eapol: e,
    })
}

/// MIC of an EAPOL-Key frame, computed with its MIC field zeroed.
fn mic(kck: &[u8], eapol: &[u8]) -> [u8; 20] {
    crypto::hmac_sha1(
        kck,
        &[&eapol[..K_MIC], &[0; MIC_LEN], &eapol[K_MIC + MIC_LEN..]],
    )
}

/// Group key from decrypted key data: a GTK KDE (00-0F-AC:1) among the
/// elements, padding (0xDD then zeros) ending the list.
fn find_gtk(data: &[u8], rsc: &[u8]) -> Result<Gtk, &'static str> {
    let mut k = 0;
    while k + 2 <= data.len() {
        let (id, len) = (data[k], data[k + 1] as usize);
        if id == EID_VENDOR && len == 0 {
            break;
        }
        let body = data
            .get(k + 2..k + 2 + len)
            .ok_or("truncated key data element")?;
        if id == EID_VENDOR && len >= 6 && body[..3] == OUI_IEEE && body[3] == KDE_GTK {
            let key = &body[6..];
            let key = key
                .try_into()
                .map_err(|_| "group key is not a 16-byte CCMP key")?;
            let mut r = [0u8; 6];
            r.copy_from_slice(&rsc[..6]);
            return Ok(Gtk {
                index: body[4] & 3,
                key,
                rsc: r,
            });
        }
        k += 2 + len;
    }
    Err("no group key in the key data")
}

impl Supplicant {
    /// For a join from `spa` with this PMK; `random` fills the nonces.
    pub fn new(pmk: [u8; 32], spa: [u8; 6], random: fn(&mut [u8])) -> Self {
        Supplicant {
            pmk,
            spa,
            aa: None,
            ie: RSN_IE.to_vec(),
            random,
            anonce: [0; 32],
            snonce: [0; 32],
            tptk: None,
            ptk: None,
            replay: None,
            gtk: None,
        }
    }

    /// The RSN element the association request really carried, when the
    /// firmware can tell: message 2 must repeat it byte for byte.
    pub fn set_ie(&mut self, ie: &[u8]) {
        self.ie = ie.to_vec();
    }

    pub fn aa(&self) -> Option<[u8; 6]> {
        self.aa
    }

    /// Message 2 went out and no message 3 checked out since: what a
    /// wrong passphrase looks like from here.
    pub fn unconfirmed(&self) -> bool {
        self.tptk.is_some() && self.ptk != self.tptk
    }

    /// One EAPOL frame from the access point: a reply and keys, or why
    /// it was dropped.
    pub fn receive(&mut self, frame: &[u8]) -> Result<Step, &'static str> {
        let k = parse(frame)?;
        if self.aa.is_some_and(|aa| aa != k.src) {
            return Err("EAPOL-Key frame from another station");
        }
        // 12.7.2: a counter not above the last MIC-checked one is a replay.
        if self.replay.is_some_and(|r| k.replay <= r) {
            return Err("replay counter did not increase");
        }
        let pairwise = k.info & KI_PAIRWISE != 0;
        let with_mic = k.info & KI_MIC != 0;
        match (pairwise, with_mic) {
            (true, false) if k.info & KI_ACK != 0 => Ok(self.message1(&k)),
            (true, true) if k.info & (KI_ACK | KI_INSTALL) == KI_ACK | KI_INSTALL => {
                self.message3(&k)
            }
            (false, true) if k.info & KI_ACK != 0 => self.group1(&k),
            _ => Err("unexpected EAPOL-Key message"),
        }
    }

    /// Message 1: ANonce in, SNonce and a PTK candidate out (message 2).
    fn message1(&mut self, k: &Key) -> Step {
        self.aa = Some(k.src);
        // A retransmitted message 1 keeps the SNonce and the PTK.
        if self.tptk.is_none() || k.nonce != self.anonce || self.tptk == self.ptk {
            self.anonce.copy_from_slice(k.nonce);
            (self.random)(&mut self.snonce);
            self.tptk = Some(self.derive(&k.src));
        }
        let kck = self.tptk.unwrap();
        let info = VERSION_AES | KI_PAIRWISE | KI_MIC;
        let (snonce, ie) = (self.snonce, self.ie.clone());
        Step {
            reply: Some(self.frame(k, info, &snonce, &ie, &kck[..16])),
            ..Step::default()
        }
    }

    /// PTK = PRF-384(PMK, "Pairwise key expansion", min(AA, SPA) ||
    /// max(AA, SPA) || min(ANonce, SNonce) || max(ANonce, SNonce)).
    fn derive(&self, aa: &[u8; 6]) -> [u8; PTK_LEN] {
        let (a, s) = (aa.as_slice(), self.spa.as_slice());
        let (n1, n2) = (self.anonce.as_slice(), self.snonce.as_slice());
        let mut data = Vec::with_capacity(76);
        data.extend_from_slice(a.min(s));
        data.extend_from_slice(a.max(s));
        data.extend_from_slice(n1.min(n2));
        data.extend_from_slice(n1.max(n2));
        let mut ptk = [0u8; PTK_LEN];
        crypto::prf(&self.pmk, b"Pairwise key expansion", &data, &mut ptk);
        ptk
    }

    /// Message 3: the PTK confirmed, the group key unwrapped, message 4.
    fn message3(&mut self, k: &Key) -> Result<Step, &'static str> {
        let tptk = self.tptk.ok_or("message 3 before message 1")?;
        if k.nonce != self.anonce {
            return Err("ANonce differs between messages 1 and 3");
        }
        if !crypto::same(&mic(&tptk[..16], k.eapol)[..MIC_LEN], k.mic) {
            return Err("message 3 MIC mismatch");
        }
        let gtk = self.unwrap_gtk(k, &tptk)?;
        self.replay = Some(k.replay);
        let info = VERSION_AES | KI_PAIRWISE | KI_MIC | KI_SECURE;
        let reply = self.frame(k, info, &[0; 32], &[], &tptk[..16]);
        // A repeated message 3 is answered, its keys never installed
        // twice (a reinstalled key resets its packet numbers).
        let fresh = self.ptk != Some(tptk);
        self.ptk = Some(tptk);
        Ok(Step {
            reply: Some(reply),
            ptk: fresh.then(|| tptk[32..].try_into().unwrap()),
            gtk: self.new_gtk(gtk),
        })
    }

    /// Group message 1 (a rekey): new group key, group message 2.
    fn group1(&mut self, k: &Key) -> Result<Step, &'static str> {
        let ptk = self.ptk.ok_or("group key before the pairwise handshake")?;
        if !crypto::same(&mic(&ptk[..16], k.eapol)[..MIC_LEN], k.mic) {
            return Err("group message 1 MIC mismatch");
        }
        let gtk = self.unwrap_gtk(k, &ptk)?;
        self.replay = Some(k.replay);
        let info = VERSION_AES | KI_MIC | KI_SECURE;
        Ok(Step {
            reply: Some(self.frame(k, info, &[0; 32], &[], &ptk[..16])),
            ptk: None,
            gtk: self.new_gtk(gtk),
        })
    }

    fn unwrap_gtk(&self, k: &Key, ptk: &[u8; PTK_LEN]) -> Result<Gtk, &'static str> {
        if k.info & KI_ENCRYPTED == 0 {
            return Err("key data not encrypted");
        }
        let mut plain = alloc::vec![0u8; k.data.len().saturating_sub(8)];
        let kek: &[u8; 16] = ptk[16..32].try_into().unwrap();
        if !crypto::unwrap(kek, k.data, &mut plain) {
            return Err("key data does not unwrap");
        }
        find_gtk(&plain, k.rsc)
    }

    fn new_gtk(&mut self, gtk: Gtk) -> Option<Gtk> {
        if self.gtk.as_ref() == Some(&gtk) {
            return None;
        }
        self.gtk = Some(gtk.clone());
        Some(gtk)
    }

    /// EAPOL-Key frame back to the access point, with its MIC.
    fn frame(&self, k: &Key, info: u16, nonce: &[u8], data: &[u8], kck: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(ETH_HLEN + K_DATA + data.len());
        f.extend_from_slice(&k.src);
        f.extend_from_slice(&self.spa);
        f.extend_from_slice(&ETH_P_PAE.to_be_bytes());
        // Same EAPOL version as the access point.
        f.push(k.version.clamp(1, 2));
        f.push(EAPOL_KEY);
        f.extend_from_slice(&((K_DATA - EAPOL_HDR + data.len()) as u16).to_be_bytes());
        f.push(DESC_RSN);
        f.extend_from_slice(&info.to_be_bytes());
        f.extend_from_slice(&0u16.to_be_bytes()); // key length: 0 from a supplicant
        f.extend_from_slice(&k.replay);
        f.extend_from_slice(nonce);
        f.extend_from_slice(&[0; 16 + 8 + 8 + MIC_LEN]); // IV, RSC, reserved, MIC
        f.extend_from_slice(&(data.len() as u16).to_be_bytes());
        f.extend_from_slice(data);
        let m = mic(kck, &f[ETH_HLEN..]);
        f[ETH_HLEN + K_MIC..ETH_HLEN + K_MIC + MIC_LEN].copy_from_slice(&m[..MIC_LEN]);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::{hex, wrap};
    use std::vec::Vec;

    const AA: [u8; 6] = [0x7A, 0xCA, 0x40, 0x6D, 0x21, 0xDD];
    const SPA: [u8; 6] = [0x02, 0x4E, 0x58, 0x4B, 0x57, 0x01];
    const GTK: [u8; 16] = [0x5A; 16];

    fn rng(b: &mut [u8]) {
        b.iter_mut()
            .enumerate()
            .for_each(|(i, x)| *x = i as u8 ^ 0xC3);
    }

    /// An access point's EAPOL-Key frame, MIC'd with `kck` when given.
    fn ap_frame(
        info: u16,
        replay: u64,
        nonce: &[u8; 32],
        data: &[u8],
        kck: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&SPA);
        f.extend_from_slice(&AA);
        f.extend_from_slice(&ETH_P_PAE.to_be_bytes());
        f.extend_from_slice(&[2, EAPOL_KEY]);
        f.extend_from_slice(&((95 + data.len()) as u16).to_be_bytes());
        f.push(DESC_RSN);
        f.extend_from_slice(&info.to_be_bytes());
        f.extend_from_slice(&16u16.to_be_bytes());
        f.extend_from_slice(&replay.to_be_bytes());
        f.extend_from_slice(nonce);
        f.extend_from_slice(&[0; 16]);
        f.extend_from_slice(&[7, 0, 0, 0, 0, 0, 0, 0]); // RSC: PN 7
        f.extend_from_slice(&[0; 8 + 16]);
        f.extend_from_slice(&(data.len() as u16).to_be_bytes());
        f.extend_from_slice(data);
        if let Some(kck) = kck {
            let m = crypto::hmac_sha1(kck, &[&f[14..]]);
            f[14 + K_MIC..14 + K_MIC + 16].copy_from_slice(&m[..16]);
        }
        f.resize(f.len().max(60), 0); // Ethernet padding
        f
    }

    fn ptk(pmk: &[u8], anonce: &[u8], snonce: &[u8]) -> [u8; 48] {
        let mut data = Vec::new();
        data.extend_from_slice(&SPA); // 02:... sorts before 7a:...
        data.extend_from_slice(&AA);
        let (lo, hi) = if anonce < snonce {
            (anonce, snonce)
        } else {
            (snonce, anonce)
        };
        data.extend_from_slice(lo);
        data.extend_from_slice(hi);
        let mut p = [0u8; 48];
        crypto::prf(pmk, b"Pairwise key expansion", &data, &mut p);
        p
    }

    fn check_mic(frame: &[u8], kck: &[u8]) {
        let e = &frame[14..];
        let want = mic(kck, e);
        assert_eq!(&e[K_MIC..K_MIC + 16], &want[..16], "MIC");
    }

    /// Key data: the RSN element (message 3 only), GTK KDE, padding.
    fn key_data(gtk: &[u8; 16], index: u8, ie: bool) -> Vec<u8> {
        let mut d = if ie { RSN_IE.to_vec() } else { Vec::new() };
        d.extend_from_slice(&[EID_VENDOR, 22, 0x00, 0x0F, 0xAC, KDE_GTK, index, 0]);
        d.extend_from_slice(gtk);
        d.push(EID_VENDOR);
        d.resize(d.len().next_multiple_of(8), 0);
        d
    }

    #[test]
    fn four_way_and_rekey() {
        let pmk = crypto::psk(b"nxkwifi2026", b"nxk-test");
        let mut s = Supplicant::new(pmk, SPA, rng);
        let anonce = [0x11u8; 32];

        let m1 = ap_frame(2 | KI_PAIRWISE | KI_ACK, 1, &anonce, &[], None);
        let step = s.receive(&m1).unwrap();
        let m2 = step.reply.unwrap();
        assert!(step.ptk.is_none() && s.unconfirmed());
        assert_eq!(&m2[..6], &AA);
        assert_eq!(be16(&m2, 14 + K_INFO), 0x010A);
        let snonce: [u8; 32] = m2[14 + K_NONCE..14 + K_NONCE + 32].try_into().unwrap();
        let p = ptk(&pmk, &anonce, &snonce);
        check_mic(&m2, &p[..16]);
        assert_eq!(&m2[14 + K_DATA..], &RSN_IE);

        let info3 = 2 | KI_PAIRWISE | KI_INSTALL | KI_ACK | KI_MIC | KI_SECURE | KI_ENCRYPTED;
        let wrapped = wrap(p[16..32].try_into().unwrap(), &key_data(&GTK, 1, true));
        let m3 = ap_frame(info3, 2, &anonce, &wrapped, Some(&p[..16]));
        let step = s.receive(&m3).unwrap();
        let m4 = step.reply.unwrap();
        assert_eq!(be16(&m4, 14 + K_INFO), 0x030A);
        check_mic(&m4, &p[..16]);
        assert_eq!(step.ptk.unwrap().as_slice(), &p[32..]);
        let g = step.gtk.unwrap();
        assert_eq!((g.index, g.key, g.rsc), (1, GTK, [7, 0, 0, 0, 0, 0]));
        assert!(!s.unconfirmed());

        // The same message 3 again: a replay. A retransmission with a
        // new counter: message 4 again, no key installed twice.
        assert!(s.receive(&m3).is_err());
        let m3b = ap_frame(info3, 3, &anonce, &wrapped, Some(&p[..16]));
        let step = s.receive(&m3b).unwrap();
        assert!(step.reply.is_some() && step.ptk.is_none() && step.gtk.is_none());

        // Group rekey to key 2.
        let gtk2 = [0xA5u8; 16];
        let info = 2 | KI_ACK | KI_MIC | KI_SECURE | KI_ENCRYPTED;
        let wrapped = wrap(p[16..32].try_into().unwrap(), &key_data(&gtk2, 2, false));
        let g1 = ap_frame(info, 4, &[0; 32], &wrapped, Some(&p[..16]));
        let step = s.receive(&g1).unwrap();
        let g2 = step.reply.unwrap();
        assert_eq!(be16(&g2, 14 + K_INFO), 0x0302);
        check_mic(&g2, &p[..16]);
        assert_eq!(step.gtk.unwrap().key, gtk2);
    }

    #[test]
    fn wrong_passphrase() {
        let mut s = Supplicant::new(crypto::psk(b"not-the-one", b"nxk-test"), SPA, rng);
        let anonce = [0x22u8; 32];
        let m1 = ap_frame(2 | KI_PAIRWISE | KI_ACK, 1, &anonce, &[], None);
        let m2 = s.receive(&m1).unwrap().reply.unwrap();
        // The access point's PTK, from the right passphrase.
        let snonce: [u8; 32] = m2[14 + K_NONCE..14 + K_NONCE + 32].try_into().unwrap();
        let p = ptk(&crypto::psk(b"nxkwifi2026", b"nxk-test"), &anonce, &snonce);
        let info3 = 2 | KI_PAIRWISE | KI_INSTALL | KI_ACK | KI_MIC | KI_SECURE | KI_ENCRYPTED;
        let wrapped = wrap(p[16..32].try_into().unwrap(), &key_data(&GTK, 1, true));
        let m3 = ap_frame(info3, 2, &anonce, &wrapped, Some(&p[..16]));
        assert_eq!(s.receive(&m3).err(), Some("message 3 MIC mismatch"));
        assert!(s.unconfirmed());
    }

    #[test]
    fn rsn_elements() {
        assert!(check_rsn(&RSN_IE).is_ok());
        // WPA2/WPA3 transition: PSK and SAE, MFP capable.
        let mixed = hex("30180100000fac040100000fac040200000fac02000fac088000");
        assert!(check_rsn(&mixed).is_ok());
        // WPA/WPA2 mixed: TKIP group.
        let tkip = hex("30140100000fac020100000fac040100000fac020000");
        assert!(check_rsn(&tkip).is_err());
        // WPA3 only: SAE, MFP required.
        let sae = hex("30140100000fac040100000fac040100000fac08c000");
        assert!(check_rsn(&sae).is_err());
        // WPA2-Enterprise (IONIS): CCMP, 802.1X + 802.1X-SHA256, MFPC=1 MFPR=0.
        assert!(check_rsn_eap(&RSN_IE_EAP).is_ok());
        let ionis = hex("30180100000fac040100000fac040200000fac01000fac058c00");
        assert!(check_rsn_eap(&ionis).is_ok());
        assert!(check_rsn(&ionis).is_err());
        let ies = hex("000874657374000000003014");
        assert_eq!(element(&ies, 0).map(|e| e.len()), Some(10));
        assert_eq!(element(&ies, EID_RSN), None);
    }
}

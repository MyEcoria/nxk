//! 802.1X EAP-PEAPv0/v1 with inner EAP-MSCHAPv2 (RFC 3748,
//! draft-josefsson-pppext-eap-tls-eap, MS-PEAP): drives a TLS 1.2
//! tunnel over EAPOL, runs MS-CHAPv2 and PEAP crypto-binding inside,
//! and yields the 32-byte PMK for the 4-way handshake.

#![no_std]
extern crate alloc;

pub mod mschapv2;
pub mod tls;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::mschapv2::Peer;
use crate::tls::Tls;

pub const ETH_P_PAE: u16 = 0x888E;
const ETH_HLEN: usize = 14;
const EAPOL_VERSION: u8 = 1;
pub const EAPOL_EAP: u8 = 0;
pub const EAPOL_START: u8 = 1;
pub const EAPOL_KEY: u8 = 3;

const CODE_REQUEST: u8 = 1;
const CODE_RESPONSE: u8 = 2;
const CODE_SUCCESS: u8 = 3;
const CODE_FAILURE: u8 = 4;

const TYPE_IDENTITY: u8 = 1;
const TYPE_NOTIFICATION: u8 = 2;
const TYPE_NAK: u8 = 3;
pub const TYPE_PEAP: u8 = 25;
const TYPE_TLV: u8 = 33;

const FLAG_LEN: u8 = 0x80;
const FLAG_MORE: u8 = 0x40;
const FLAG_START: u8 = 0x20;
const VER_MASK: u8 = 0x07;

/// Largest TLS payload per EAP-PEAP fragment (fits inside 1500-byte MTU).
const FRAG_MAX: usize = 1024;
const MSG_MAX: usize = 65536;

const TLV_MANDATORY: u16 = 0x8000;
const TLV_TYPE_MASK: u16 = 0x3FFF;
const TLV_NAK: u16 = 2;
const TLV_RESULT: u16 = 3;
const TLV_CRYPTO_BINDING: u16 = 12;
const RESULT_SUCCESS: u16 = 1;
const RESULT_FAILURE: u16 = 2;
const BINDING_LEN: usize = 56;
const SUBTYPE_REQ: u8 = 0;
const SUBTYPE_RESP: u8 = 1;

type HmacSha1 = Hmac<Sha1>;

#[cfg(target_os = "none")]
static RNG: spin::Mutex<Option<fn(&mut [u8])>> = spin::Mutex::new(None);

#[cfg(target_os = "none")]
fn getrandom_hook(dest: &mut [u8]) -> Result<(), getrandom::Error> {
    match *RNG.lock() {
        Some(f) => {
            f(dest);
            Ok(())
        }
        None => Err(getrandom::Error::UNSUPPORTED),
    }
}

#[cfg(target_os = "none")]
getrandom::register_custom_getrandom!(getrandom_hook);

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut mac = <HmacSha1 as Mac>::new_from_slice(key).expect("HMAC key");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// PEAP PRF+ (`peap_prfplus` in hostap): version 0 appends `counter || 0x0000`,
/// version 1 appends `len || counter`.
pub fn peap_prfplus(version: u8, key: &[u8], label: &str, seed: &[u8], out: &mut [u8]) {
    let mut hash = [0u8; 20];
    let mut pos = 0;
    let mut counter = 0u8;
    while pos < out.len() {
        counter = counter.wrapping_add(1);
        let prev: &[u8] = if counter == 1 { &[] } else { &hash };
        let tail_v0 = [counter, 0, 0];
        let tail_v1 = [out.len() as u8, counter];
        let tail: &[u8] = if version == 0 { &tail_v0 } else { &tail_v1 };
        hash = hmac_sha1(key, &[prev, label.as_bytes(), seed, tail]);
        let take = (out.len() - pos).min(20);
        out[pos..pos + take].copy_from_slice(&hash[..take]);
        pos += take;
    }
}

/// Outcome of processing one incoming EAPOL-EAP frame.
pub enum Step {
    /// No frame to send.
    None,
    /// An Ethernet EAPOL-EAP response frame to transmit.
    Reply(Vec<u8>),
    /// Authentication succeeded; yields the 32-byte PMK for WPA2 4-way handshake.
    Success([u8; 32]),
    /// The server rejected the credentials or aborted authentication.
    Failed(String),
}

/// EAP-PEAPv0/v1 supplicant state machine.
pub struct Peap {
    identity: Vec<u8>,
    spa: [u8; 6],
    aa: Option<[u8; 6]>,
    tls: Tls,
    mschap: Peer,
    peap_version: u8,
    rx_buf: Vec<u8>,
    rx_expect: Option<usize>,
    tx_buf: Vec<u8>,
    tx_off: usize,
    tx_phase1: bool,
    last_id: Option<u8>,
    last_resp: Option<Vec<u8>>,
    ipmk: Option<[u8; 40]>,
    phase2_ok: bool,
    failed_reason: Option<String>,
}

impl Peap {
    /// New PEAP supplicant for `identity` and `password` (UTF-8), with
    /// optional leaf certificate SHA-256 `pins`.
    pub fn new(
        identity: &[u8],
        password: &str,
        pins: &[[u8; 32]],
        spa: [u8; 6],
        random: fn(&mut [u8]),
    ) -> Result<Self, String> {
        #[cfg(target_os = "none")]
        {
            *RNG.lock() = Some(random);
        }
        let tls = Tls::new(pins)?;
        let hash = mschapv2::nt_hash(password);
        Ok(Self {
            identity: identity.to_vec(),
            spa,
            aa: None,
            tls,
            mschap: Peer::new(identity, hash, random),
            peap_version: 1,
            rx_buf: Vec::new(),
            rx_expect: None,
            tx_buf: Vec::new(),
            tx_off: 0,
            tx_phase1: true,
            last_id: None,
            last_resp: None,
            ipmk: None,
            phase2_ok: false,
            failed_reason: None,
        })
    }

    /// SHA-256 of the server's leaf certificate once seen.
    pub fn server_cert(&self) -> Option<[u8; 32]> {
        self.tls.server_cert()
    }

    /// True once at least one EAP request from the authenticator was seen.
    pub fn started(&self) -> bool {
        self.aa.is_some()
    }

    /// An Ethernet `EAPOL-Start` frame to prompt the authenticator.
    pub fn eapol_start(&self, aa: &[u8; 6]) -> Vec<u8> {
        let mut f = Vec::with_capacity(ETH_HLEN + 4);
        f.extend_from_slice(aa);
        f.extend_from_slice(&self.spa);
        f.extend_from_slice(&ETH_P_PAE.to_be_bytes());
        f.extend_from_slice(&[EAPOL_VERSION, EAPOL_START, 0, 0]);
        f
    }

    /// Process an Ethernet `ETH_P_PAE` frame (`EAPOL_EAP` packet).
    pub fn receive(&mut self, frame: &[u8]) -> Result<Step, String> {
        if frame.len() < ETH_HLEN + 4 {
            return Err(String::from("short EAPOL header"));
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        if ethertype != ETH_P_PAE {
            return Err(String::from("not an EAPOL frame"));
        }
        let ptype = frame[ETH_HLEN + 1];
        if ptype != EAPOL_EAP {
            return Ok(Step::None);
        }
        let body_len = u16::from_be_bytes([frame[ETH_HLEN + 2], frame[ETH_HLEN + 3]]) as usize;
        let eap = frame
            .get(ETH_HLEN + 4..ETH_HLEN + 4 + body_len)
            .ok_or_else(|| String::from("truncated EAPOL body"))?;
        let mut aa = [0u8; 6];
        aa.copy_from_slice(&frame[6..12]);
        self.aa = Some(aa);
        match self.receive_eap(eap)? {
            EapOut::None => Ok(Step::None),
            EapOut::Reply(pkt) => Ok(Step::Reply(self.wrap_eth(&aa, &pkt))),
            EapOut::Success(pmk) => Ok(Step::Success(pmk)),
            EapOut::Failed(why) => Ok(Step::Failed(why)),
        }
    }

    fn wrap_eth(&self, aa: &[u8; 6], eap: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(ETH_HLEN + 4 + eap.len());
        f.extend_from_slice(aa);
        f.extend_from_slice(&self.spa);
        f.extend_from_slice(&ETH_P_PAE.to_be_bytes());
        f.push(EAPOL_VERSION);
        f.push(EAPOL_EAP);
        f.extend_from_slice(&(eap.len() as u16).to_be_bytes());
        f.extend_from_slice(eap);
        f
    }

    fn receive_eap(&mut self, eap: &[u8]) -> Result<EapOut, String> {
        if eap.len() < 4 {
            return Err(String::from("short EAP header"));
        }
        let (code, id) = (eap[0], eap[1]);
        let len = u16::from_be_bytes([eap[2], eap[3]]) as usize;
        if len < 4 || len > eap.len() {
            return Err(String::from("bad EAP length"));
        }
        let eap = &eap[..len];
        match code {
            CODE_SUCCESS => {
                let pmk = self.derive_pmk()?;
                Ok(EapOut::Success(pmk))
            }
            CODE_FAILURE => {
                let why = self
                    .failed_reason
                    .take()
                    .unwrap_or_else(|| String::from("EAP-Failure from access point"));
                Ok(EapOut::Failed(why))
            }
            CODE_REQUEST => {
                if eap.len() < 5 {
                    return Err(String::from("EAP-Request without type"));
                }
                if self.last_id == Some(id) {
                    if let Some(prev) = &self.last_resp {
                        return Ok(EapOut::Reply(prev.clone()));
                    }
                }
                let resp = match eap[4] {
                    TYPE_IDENTITY => eap_resp(id, TYPE_IDENTITY, &self.identity),
                    TYPE_NOTIFICATION => eap_resp(id, TYPE_NOTIFICATION, &[]),
                    TYPE_PEAP => self.step_peap(id, &eap[5..])?,
                    _ => eap_resp(id, TYPE_NAK, &[TYPE_PEAP]),
                };
                if let Some(why) = self.failed_reason.take() {
                    return Ok(EapOut::Failed(why));
                }
                self.last_id = Some(id);
                self.last_resp = Some(resp.clone());
                Ok(EapOut::Reply(resp))
            }
            _ => Ok(EapOut::None),
        }
    }

    fn step_peap(&mut self, id: u8, body: &[u8]) -> Result<Vec<u8>, String> {
        if body.is_empty() {
            return Err(String::from("PEAP request without flags"));
        }
        let flags = body[0];
        let mut data = &body[1..];
        let ver = flags & VER_MASK;
        if flags & FLAG_START != 0 || ver < self.peap_version {
            self.peap_version = ver.min(1);
        }

        // Continue draining a multi-fragment outgoing TLS flight when ACKed.
        if self.tx_off < self.tx_buf.len() {
            if !data.is_empty() || (flags & (FLAG_START | FLAG_MORE | FLAG_LEN)) != 0 {
                return Err(String::from("unexpected payload while sending fragments"));
            }
            return Ok(self.next_tx_fragment(id));
        }

        if flags & FLAG_START != 0 {
            self.rx_buf.clear();
            self.rx_expect = None;
            let (out, _) = self.tls.feed(&[])?;
            self.tx_buf = out;
            self.tx_off = 0;
            self.tx_phase1 = true;
            return Ok(self.next_tx_fragment(id));
        }

        if flags & FLAG_LEN != 0 {
            if data.len() < 4 {
                return Err(String::from("PEAP length flag without 4-byte length"));
            }
            let total = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if total > MSG_MAX {
                return Err(format!("PEAP TLS message too large ({} bytes)", total));
            }
            if self.rx_buf.is_empty() {
                self.rx_expect = Some(total);
            }
            data = &data[4..];
        }

        if self.rx_buf.len() + data.len() > MSG_MAX {
            return Err(String::from("PEAP reassembly overflow"));
        }
        self.rx_buf.extend_from_slice(data);

        if flags & FLAG_MORE != 0 {
            return Ok(eap_resp(id, TYPE_PEAP, &[self.peap_version]));
        }

        let rx = core::mem::take(&mut self.rx_buf);
        self.rx_expect = None;
        let (mut out, app) = self.tls.feed(&rx)?;
        let phase1 = app.is_empty();
        for plain in app {
            if let Some(inner) = self.phase2(id, &plain)? {
                let enc = self.tls.encrypt(&inner)?;
                out.extend_from_slice(&enc);
            }
        }
        if out.is_empty() {
            return Ok(eap_resp(id, TYPE_PEAP, &[self.peap_version]));
        }
        self.tx_buf = out;
        self.tx_off = 0;
        self.tx_phase1 = phase1;
        Ok(self.next_tx_fragment(id))
    }

    /// Next fragment of `tx_buf`. Windows Server NPS requires `FLAG_LEN`
    /// on Phase 1 messages in PEAPv0 even when unfragmented.
    fn next_tx_fragment(&mut self, id: u8) -> Vec<u8> {
        let first = self.tx_off == 0;
        let rem = self.tx_buf.len() - self.tx_off;
        let take = rem.min(FRAG_MAX);
        let more = take < rem;
        let include_len = first && (more || (self.peap_version == 0 && self.tx_phase1));

        let mut flags = self.peap_version;
        if include_len {
            flags |= FLAG_LEN;
        }
        if more {
            flags |= FLAG_MORE;
        }
        let mut payload = Vec::with_capacity(1 + if include_len { 4 } else { 0 } + take);
        payload.push(flags);
        if include_len {
            payload.extend_from_slice(&(self.tx_buf.len() as u32).to_be_bytes());
        }
        payload.extend_from_slice(&self.tx_buf[self.tx_off..self.tx_off + take]);
        self.tx_off += take;
        if self.tx_off >= self.tx_buf.len() {
            self.tx_buf.clear();
            self.tx_off = 0;
        }
        eap_resp(id, TYPE_PEAP, &payload)
    }

    /// Process decrypted Phase 2 tunnel payload (`eap_peap_phase2_request`).
    fn phase2(&mut self, outer_id: u8, plain: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if plain.is_empty() {
            return Ok(None);
        }
        let (inner_id, body): (u8, &[u8]) = if self.peap_version == 0 {
            // FreeRADIUS full EAP-Request/Identity and EAP-TLV carry a 4-byte EAP header.
            if plain.len() == 5
                && plain[0] == CODE_REQUEST
                && plain[2] == 0
                && plain[3] == 5
                && plain[4] == TYPE_IDENTITY
            {
                (plain[1], &plain[4..])
            } else if plain.len() >= 5 && plain[0] == CODE_REQUEST && plain[4] == TYPE_TLV {
                let len = u16::from_be_bytes([plain[2], plain[3]]) as usize;
                if len < 5 || len > plain.len() {
                    return Err(String::from("bad inner EAP-TLV length"));
                }
                (plain[1], &plain[4..len])
            } else {
                (outer_id, plain)
            }
        } else {
            if plain.len() == 4 && (plain[0] == CODE_SUCCESS || plain[0] == CODE_FAILURE) {
                return Ok(None);
            }
            if plain.len() < 5 || plain[0] != CODE_REQUEST {
                return Err(String::from("malformed PEAPv1 inner EAP request"));
            }
            let len = u16::from_be_bytes([plain[2], plain[3]]) as usize;
            if len < 5 || len > plain.len() {
                return Err(String::from("truncated PEAPv1 inner EAP request"));
            }
            (plain[1], &plain[4..len])
        };

        let resp_body = match body[0] {
            TYPE_IDENTITY => {
                let mut r = Vec::with_capacity(1 + self.identity.len());
                r.push(TYPE_IDENTITY);
                r.extend_from_slice(&self.identity);
                r
            }
            TYPE_NOTIFICATION => vec![TYPE_NOTIFICATION],
            mschapv2::TYPE => match self.mschap.receive(body).map_err(String::from)? {
                mschapv2::Step::Reply(r) | mschapv2::Step::Success(r) => r,
                mschapv2::Step::Failure(r, code) => {
                    let msg = core::str::from_utf8(&code).unwrap_or("?");
                    self.failed_reason =
                        Some(format!("MS-CHAPv2 rejected credentials (E={})", msg));
                    r
                }
            },
            TYPE_TLV => self.process_tlv(&body[1..])?,
            _ => vec![TYPE_NAK, mschapv2::TYPE],
        };

        Ok(Some(self.wrap_phase2(inner_id, resp_body)))
    }

    fn wrap_phase2(&self, id: u8, body: Vec<u8>) -> Vec<u8> {
        if self.peap_version == 0 && body.first().copied() != Some(TYPE_TLV) {
            return body;
        }
        let len = (4 + body.len()) as u16;
        let mut out = Vec::with_capacity(4 + body.len());
        out.push(CODE_RESPONSE);
        out.push(id);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn process_tlv(&mut self, mut tlvs: &[u8]) -> Result<Vec<u8>, String> {
        let mut status = None;
        let mut binding: Option<&[u8]> = None;
        let mut unknown_mandatory = None;

        while !tlvs.is_empty() {
            if tlvs.len() < 4 {
                return Err(String::from("truncated EAP-TLV header"));
            }
            let raw_type = u16::from_be_bytes([tlvs[0], tlvs[1]]);
            let mandatory = (raw_type & TLV_MANDATORY) != 0;
            let tlv_type = raw_type & TLV_TYPE_MASK;
            let len = u16::from_be_bytes([tlvs[2], tlvs[3]]) as usize;
            if 4 + len > tlvs.len() {
                return Err(String::from("truncated EAP-TLV value"));
            }
            let val = &tlvs[4..4 + len];
            match tlv_type {
                TLV_RESULT => {
                    if len < 2 {
                        return Err(String::from("short EAP-TLV Result"));
                    }
                    status = Some(u16::from_be_bytes([val[0], val[1]]));
                }
                TLV_CRYPTO_BINDING => {
                    if len != BINDING_LEN {
                        return Err(String::from("invalid EAP-TLV Crypto-Binding length"));
                    }
                    binding = Some(&tlvs[..4 + BINDING_LEN]);
                }
                _ if mandatory => unknown_mandatory = Some(tlv_type),
                _ => {}
            }
            tlvs = &tlvs[4 + len..];
        }

        if let Some(t) = unknown_mandatory {
            let mut r = Vec::with_capacity(1 + 10);
            r.push(TYPE_TLV);
            r.extend_from_slice(&(TLV_MANDATORY | TLV_NAK).to_be_bytes());
            r.extend_from_slice(&6u16.to_be_bytes());
            r.extend_from_slice(&0u32.to_be_bytes());
            r.extend_from_slice(&t.to_be_bytes());
            return Ok(r);
        }

        let req_status = status.ok_or_else(|| String::from("EAP-TLV without Result TLV"))?;
        let ok = req_status == RESULT_SUCCESS && self.mschap.isk().is_some();
        let resp_status = if ok {
            self.phase2_ok = true;
            RESULT_SUCCESS
        } else {
            if self.failed_reason.is_none() {
                self.failed_reason = Some(String::from("PEAP Phase 2 authentication failed"));
            }
            RESULT_FAILURE
        };

        let mut out = Vec::with_capacity(1 + 6 + if binding.is_some() { 60 } else { 0 });
        out.push(TYPE_TLV);
        out.extend_from_slice(&(TLV_MANDATORY | TLV_RESULT).to_be_bytes());
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&resp_status.to_be_bytes());

        if let (true, Some(raw_tlv)) = (ok, binding) {
            let resp_tlv = self.verify_and_build_binding(raw_tlv)?;
            out.extend_from_slice(&resp_tlv);
        }
        Ok(out)
    }

    fn verify_and_build_binding(&mut self, raw_tlv: &[u8]) -> Result<[u8; 60], String> {
        let val = &raw_tlv[4..60];
        let ver = val[1];
        let recv_ver = val[2];
        let subtype = val[3];
        if recv_ver != self.peap_version || ver > 1 {
            return Err(String::from("PEAP crypto-binding version mismatch"));
        }
        if subtype != SUBTYPE_REQ {
            return Err(String::from("unexpected crypto-binding subtype"));
        }
        let tk = self
            .tls
            .export(b"client EAP encryption")
            .ok_or_else(|| String::from("TLS keys unavailable for crypto-binding"))?;
        let isk = self.mschap.isk().unwrap_or([0u8; 32]);
        let (ipmk, cmk) = derive_imck(self.peap_version, &tk[..40], &isk);

        let mut check = [0u8; 61];
        check[..40].copy_from_slice(&raw_tlv[..40]);
        check[60] = TYPE_PEAP;
        let expected = hmac_sha1(&cmk, &[&check]);
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(&val[36..56]) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return Err(String::from("PEAP crypto-binding Compound_MAC mismatch"));
        }
        self.ipmk = Some(ipmk);

        let mut resp = [0u8; 60];
        resp[..2].copy_from_slice(&TLV_CRYPTO_BINDING.to_be_bytes());
        resp[2..4].copy_from_slice(&(BINDING_LEN as u16).to_be_bytes());
        resp[4] = 0;
        resp[5] = self.peap_version;
        resp[6] = ver;
        resp[7] = SUBTYPE_RESP;
        resp[8..40].copy_from_slice(&val[4..36]);
        let mac = hmac_sha1(&cmk, &[&resp, &[TYPE_PEAP]]);
        resp[40..60].copy_from_slice(&mac);
        Ok(resp)
    }

    fn derive_pmk(&self) -> Result<[u8; 32], String> {
        if !self.phase2_ok && self.mschap.isk().is_none() {
            return Err(String::from(
                "outer EAP-Success before Phase 2 authentication",
            ));
        }
        let mut pmk = [0u8; 32];
        if let Some(ipmk) = &self.ipmk {
            let mut csk = [0u8; 128];
            peap_prfplus(
                self.peap_version,
                ipmk,
                "Session Key Generating Function",
                &[0u8],
                &mut csk,
            );
            pmk.copy_from_slice(&csk[..32]);
        } else {
            let tk = self
                .tls
                .export(b"client EAP encryption")
                .ok_or_else(|| String::from("TLS keys unavailable for MSK"))?;
            pmk.copy_from_slice(&tk[..32]);
        }
        Ok(pmk)
    }
}

enum EapOut {
    None,
    Reply(Vec<u8>),
    Success([u8; 32]),
    Failed(String),
}

fn derive_imck(version: u8, tk40: &[u8], isk: &[u8; 32]) -> ([u8; 40], [u8; 20]) {
    let mut imck = [0u8; 60];
    peap_prfplus(version, tk40, "Inner Methods Compound Keys", isk, &mut imck);
    let (mut ipmk, mut cmk) = ([0u8; 40], [0u8; 20]);
    ipmk.copy_from_slice(&imck[..40]);
    cmk.copy_from_slice(&imck[40..60]);
    (ipmk, cmk)
}

fn eap_resp(id: u8, eap_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = (5 + payload.len()) as u16;
    let mut out = Vec::with_capacity(len as usize);
    out.push(CODE_RESPONSE);
    out.push(id);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(eap_type);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prfplus_v0_and_v1_vectors() {
        let key = [0x0bu8; 20];
        let seed = [0xaa, 0xbb, 0xcc, 0xdd];
        let mut out0 = [0u8; 40];
        peap_prfplus(0, &key, "Inner Methods Compound Keys", &seed, &mut out0);
        let t1 = hmac_sha1(&key, &[b"Inner Methods Compound Keys", &seed, &[1, 0, 0]]);
        let t2 = hmac_sha1(
            &key,
            &[&t1, b"Inner Methods Compound Keys", &seed, &[2, 0, 0]],
        );
        assert_eq!(&out0[..20], &t1);
        assert_eq!(&out0[20..40], &t2);

        let mut out1 = [0u8; 40];
        peap_prfplus(1, &key, "Inner Methods Compound Keys", &seed, &mut out1);
        let u1 = hmac_sha1(&key, &[b"Inner Methods Compound Keys", &seed, &[40, 1]]);
        let u2 = hmac_sha1(
            &key,
            &[&u1, b"Inner Methods Compound Keys", &seed, &[40, 2]],
        );
        assert_eq!(&out1[..20], &u1);
        assert_eq!(&out1[20..40], &u2);
    }

    fn from_ap(spa: &[u8; 6], aa: &[u8; 6], eap: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(ETH_HLEN + 4 + eap.len());
        f.extend_from_slice(spa);
        f.extend_from_slice(aa);
        f.extend_from_slice(&ETH_P_PAE.to_be_bytes());
        f.push(EAPOL_VERSION);
        f.push(EAPOL_EAP);
        f.extend_from_slice(&(eap.len() as u16).to_be_bytes());
        f.extend_from_slice(eap);
        f
    }

    #[test]
    fn outer_identity_nak_and_start() {
        let spa = [0x02, 1, 2, 3, 4, 5];
        let mut p = Peap::new(b"user@example.org", "nxkwifi2026", &[], spa, |b| {
            b.fill(0x42)
        })
        .unwrap();
        let aa = [0xa8, 0xf7, 0xd9, 0x10, 0x34, 0x21];
        let id_req = from_ap(&spa, &aa, &[CODE_REQUEST, 1, 0, 5, TYPE_IDENTITY]);
        let Step::Reply(r) = p.receive(&id_req).unwrap() else {
            panic!("expected Identity response");
        };
        assert_eq!(&r[..6], &aa);
        assert_eq!(&r[6..12], &spa);
        assert_eq!(&r[18..23], &[CODE_RESPONSE, 1, 0, 21, TYPE_IDENTITY]);
        assert_eq!(&r[23..], b"user@example.org");

        // Retransmitted EAP-Request with same ID returns cached response.
        let Step::Reply(r2) = p.receive(&id_req).unwrap() else {
            panic!("expected cached response");
        };
        assert_eq!(r, r2);

        // Unsupported outer method (e.g. MD5-Challenge = 4) gets Legacy NAK(25).
        let md5_req = from_ap(&spa, &aa, &[CODE_REQUEST, 2, 0, 5, 4]);
        let Step::Reply(nak) = p.receive(&md5_req).unwrap() else {
            panic!("expected NAK");
        };
        assert_eq!(&nak[18..], &[CODE_RESPONSE, 2, 0, 6, TYPE_NAK, TYPE_PEAP]);

        // PEAP Start triggers TLS ClientHello with length header in PEAPv0.
        let start_req = from_ap(&spa, &aa, &[CODE_REQUEST, 3, 0, 6, TYPE_PEAP, FLAG_START]);
        let Step::Reply(ch) = p.receive(&start_req).unwrap() else {
            panic!("expected ClientHello");
        };
        assert_eq!(ch[22], TYPE_PEAP);
        assert_eq!(ch[23] & FLAG_LEN, FLAG_LEN);
        assert_eq!(ch[23] & VER_MASK, 0);
        // TLS record starts with Handshake (22), ClientHello (1).
        assert_eq!(ch[28], 22);
        assert_eq!(ch[33], 1);
    }

    #[test]
    fn phase2_peapv0_header_rules_and_csk() {
        let peer_chal = |b: &mut [u8]| {
            b.copy_from_slice(&[
                0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x3A, 0x33,
                0x7C, 0x7E,
            ]);
        };
        let mut p =
            Peap::new(b"User", "clientPass", &[], [0x02, 1, 2, 3, 4, 5], peer_chal).unwrap();
        p.peap_version = 0;

        // PEAPv0 headerless inner Identity request `[1]` -> headerless `[1, ...identity]`.
        let id_resp = p.phase2(4, &[TYPE_IDENTITY]).unwrap().unwrap();
        assert_eq!(id_resp, b"\x01User");

        // FreeRADIUS 5-byte full EAP-Request/Identity `[1, 9, 0, 5, 1]` -> headerless `[1, ...identity]`.
        let fr_resp = p
            .phase2(4, &[CODE_REQUEST, 9, 0, 5, TYPE_IDENTITY])
            .unwrap()
            .unwrap();
        assert_eq!(fr_resp, b"\x01User");

        // Headerless MS-CHAPv2 Challenge -> headerless MS-CHAPv2 Response.
        let mut chal = vec![mschapv2::TYPE, 1, 7, 0, 24, 16];
        chal.extend_from_slice(&[
            0x5B, 0x5D, 0x7C, 0x7D, 0x7B, 0x3F, 0x2F, 0x3E, 0x3C, 0x2C, 0x60, 0x21, 0x32, 0x26,
            0x26, 0x28,
        ]);
        chal.extend_from_slice(b"nps");
        let ms_resp = p.phase2(5, &chal).unwrap().unwrap();
        assert_eq!(ms_resp[0], mschapv2::TYPE);
        assert_eq!(ms_resp[1], 2);

        // Headerless MS-CHAPv2 Success -> headerless `[26, 3]`.
        let mut succ = vec![mschapv2::TYPE, 3, 7, 0, 47];
        succ.extend_from_slice(b"S=407A5589115FD0D6209F510FE9C04566932CDA56 M=ok");
        let ms_ack = p.phase2(6, &succ).unwrap().unwrap();
        assert_eq!(ms_ack, [mschapv2::TYPE, 3]);
        assert!(p.mschap.isk().is_some());

        // EAP-TLV in PEAPv0 carries a full 4-byte EAP header in both directions.
        let tlv_req = [
            CODE_REQUEST,
            8,
            0,
            11,
            TYPE_TLV,
            0x80,
            TLV_RESULT as u8,
            0,
            2,
            0,
            RESULT_SUCCESS as u8,
        ];
        let tlv_resp = p.phase2(8, &tlv_req).unwrap().unwrap();
        assert_eq!(
            tlv_resp,
            [
                CODE_RESPONSE,
                8,
                0,
                11,
                TYPE_TLV,
                0x80,
                TLV_RESULT as u8,
                0,
                2,
                0,
                RESULT_SUCCESS as u8,
            ]
        );
        assert!(p.phase2_ok);

        // Simulate verified IPMK and check CSK -> PMK derivation.
        p.ipmk = Some([0x55u8; 40]);
        let pmk = p.derive_pmk().unwrap();
        let mut expect_csk = [0u8; 128];
        peap_prfplus(
            0,
            &[0x55u8; 40],
            "Session Key Generating Function",
            &[0u8],
            &mut expect_csk,
        );
        assert_eq!(pmk, expect_csk[..32]);
    }
}

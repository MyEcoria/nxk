//! A DTLS 1.2 client with no I/O, for Luna's session servers: datagrams
//! in, datagrams out, the caller owns the socket and the clock. One suite,
//! ECDHE-RSA-CHACHA20-POLY1305 (RFC 7905), x25519 or P-256; the server's
//! leaf certificate is pinned (SHA-256 of its DER and its CN, as the
//! session API gives them) and its key must sign the ECDHE parameters.
//! Handshake messages are reassembled from fragments, the cookie exchange
//! is followed and a flight is sent again when its answer is late (1 s,
//! doubling) or the peer's flight comes again.

mod crypto;
mod x509;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use sha2::Sha256;

use crypto::Keys;

/// Datagrams are at most this long (an Ethernet MTU less IPv4 and UDP).
pub const MTU: usize = 1472;
/// Application bytes per datagram.
pub const MAX_PAYLOAD: usize = MTU - HEADER - crypto::TAG;

const HEADER: usize = 13;
const SUITE: u16 = 0xCCA8;
const X25519: u16 = 0x001D;
const P256: u16 = 0x0017;
const RSA_PKCS1_SHA256: u16 = 0x0401;

const CCS: u8 = 20;
const ALERT: u8 = 21;
const HANDSHAKE: u8 = 22;
const APP: u8 = 23;

const CLIENT_HELLO: u8 = 1;
const SERVER_HELLO: u8 = 2;
const HELLO_VERIFY: u8 = 3;
const CERTIFICATE: u8 = 11;
const SERVER_KEY_EXCHANGE: u8 = 12;
const SERVER_HELLO_DONE: u8 = 14;
const CLIENT_KEY_EXCHANGE: u8 = 16;
const FINISHED: u8 = 20;

const FIRST_WAIT: Duration = Duration::from_secs(1);
const LAST_WAIT: Duration = Duration::from_secs(16);
/// A handshake message larger than this is refused.
const MAX_MESSAGE: usize = 64 << 10;

/// The server certificate the session API names.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub common_name: String,
    pub sha256: [u8; 32],
}

impl Pin {
    /// From `certificateInformation`: its `commonName` and hex `sha256`.
    pub fn from_hex(common_name: &str, sha256: &str) -> Option<Self> {
        if common_name.is_empty() || sha256.len() != 64 {
            return None;
        }
        let mut h = [0u8; 32];
        for (i, b) in h.iter_mut().enumerate() {
            *b = u8::from_str_radix(sha256.get(2 * i..2 * i + 2)?, 16).ok()?;
        }
        Some(Self {
            common_name: common_name.into(),
            sha256: h,
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// The certificate is not the pinned one.
    Pin(&'static str),
    /// The server broke the protocol or offered what this client lacks.
    Handshake(&'static str),
    /// A fatal alert from the server (its description).
    Alert(u8),
    /// The server closed the connection (close_notify).
    Closed,
}

/// Datagrams to send back, and the application data received.
pub type Exchange = (Vec<Vec<u8>>, Vec<Vec<u8>>);

/// Fresh random bytes for one connection (the client random, the ECDHE
/// secret): the caller's generator, so tests can fix them.
pub struct Secrets {
    pub random: [u8; 32],
    pub ecdh: [u8; 32],
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Hello,
    ServerFlight,
    Finished,
    Connected,
    Closed,
}

/// A record of the last flight, sent again as it was (new sequence
/// numbers).
struct Sent {
    kind: u8,
    epoch: u16,
    body: Vec<u8>,
}

/// A handshake message being reassembled.
struct Partial {
    kind: u8,
    data: Vec<u8>,
    have: Vec<bool>,
}

pub struct Client {
    pin: Pin,
    secrets: Secrets,
    phase: Phase,
    cookie: Vec<u8>,
    send_seq: u16,
    recv_seq: u16,
    record_seq: [u64; 2],
    transcript: Vec<u8>,
    server_random: [u8; 32],
    server_key: Option<RsaPublicKey>,
    group: u16,
    server_point: Vec<u8>,
    master: Vec<u8>,
    keys: Option<(Keys, Keys)>,
    partial: BTreeMap<u16, Partial>,
    flight: Vec<Sent>,
    timer: Option<(Instant, Duration)>,
    server_ccs: bool,
}

/// Bounds-checked reading of handshake bodies.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Handshake("message cut short"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    fn int(&mut self, n: usize) -> Result<usize, Error> {
        Ok(self.take(n)?.iter().fold(0, |a, &b| a << 8 | b as usize))
    }

    fn vec(&mut self, len_bytes: usize) -> Result<&'a [u8], Error> {
        let n = self.int(len_bytes)?;
        self.take(n)
    }
}

fn u24(n: usize) -> [u8; 3] {
    [(n >> 16) as u8, (n >> 8) as u8, n as u8]
}

impl Client {
    /// A client for the server pinned by `pin`; send what `start` returns.
    pub fn new(pin: Pin, secrets: Secrets) -> Self {
        Self {
            pin,
            secrets,
            phase: Phase::Hello,
            cookie: Vec::new(),
            send_seq: 0,
            recv_seq: 0,
            record_seq: [0; 2],
            transcript: Vec::new(),
            server_random: [0; 32],
            server_key: None,
            group: 0,
            server_point: Vec::new(),
            master: Vec::new(),
            keys: None,
            partial: BTreeMap::new(),
            flight: Vec::new(),
            timer: None,
            server_ccs: false,
        }
    }

    /// The first ClientHello.
    pub fn start(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.send_hello(now)
    }

    pub fn is_connected(&self) -> bool {
        self.phase == Phase::Connected
    }

    /// When `on_timer` should run next (a flight waiting for its answer).
    pub fn deadline(&self) -> Option<Instant> {
        self.timer.map(|(at, _)| at)
    }

    /// The flight again if its answer is late.
    pub fn on_timer(&mut self, now: Instant) -> Vec<Vec<u8>> {
        match self.timer {
            Some((at, wait)) if now >= at => {
                let wait = (wait * 2).min(LAST_WAIT);
                self.timer = Some((now + wait, wait));
                self.resend()
            }
            _ => Vec::new(),
        }
    }

    /// A datagram from the server: what to send back, and the application
    /// data it carried.
    pub fn on_datagram(&mut self, now: Instant, mut d: &[u8]) -> Result<Exchange, Error> {
        let (mut out, mut app) = (Vec::new(), Vec::new());
        while d.len() >= HEADER {
            let kind = d[0];
            let epoch = u16::from_be_bytes([d[3], d[4]]);
            let seq = d[5..11].iter().fold(0u64, |a, &b| a << 8 | b as u64);
            let len = u16::from_be_bytes([d[11], d[12]]) as usize;
            let Some(body) = d.get(HEADER..HEADER + len) else {
                break;
            };
            d = &d[HEADER + len..];
            let plain = match epoch {
                0 => body.to_vec(),
                1 => match &self.keys {
                    Some((_, server)) => match server.open(1 << 48 | seq, kind, body) {
                        Some(p) => p,
                        None => continue,
                    },
                    None => continue,
                },
                _ => continue,
            };
            match kind {
                HANDSHAKE => out.extend(self.on_handshake_record(now, epoch, &plain)?),
                CCS if self.phase == Phase::Finished => self.server_ccs = true,
                ALERT if plain.len() >= 2 => {
                    self.phase = Phase::Closed;
                    return Err(if plain[1] == 0 {
                        Error::Closed
                    } else {
                        Error::Alert(plain[1])
                    });
                }
                APP if self.phase == Phase::Connected && epoch == 1 => app.push(plain),
                _ => {}
            }
        }
        Ok((out, app))
    }

    /// One datagram carrying `data` (`MAX_PAYLOAD` at most).
    pub fn seal(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        (self.phase == Phase::Connected && data.len() <= MAX_PAYLOAD)
            .then(|| self.record(APP, 1, data))
    }

    /// The close_notify alert, once connected.
    pub fn close(&mut self) -> Option<Vec<u8>> {
        let r = (self.phase == Phase::Connected).then(|| self.record(ALERT, 1, &[1, 0]));
        self.phase = Phase::Closed;
        r
    }

    fn record(&mut self, kind: u8, epoch: u16, body: &[u8]) -> Vec<u8> {
        let e = epoch as usize;
        let seq = self.record_seq[e];
        self.record_seq[e] += 1;
        let payload = match (epoch, &self.keys) {
            (1, Some((client, _))) => client.seal((epoch as u64) << 48 | seq, kind, body),
            _ => body.to_vec(),
        };
        let mut r = Vec::with_capacity(HEADER + payload.len());
        r.push(kind);
        r.extend_from_slice(&[0xFE, 0xFD]);
        r.extend_from_slice(&epoch.to_be_bytes());
        r.extend_from_slice(&seq.to_be_bytes()[2..]);
        r.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        r.extend_from_slice(&payload);
        r
    }

    /// A handshake message with its DTLS header (whole: offset 0).
    fn message(&mut self, kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![kind];
        m.extend_from_slice(&u24(body.len()));
        m.extend_from_slice(&self.send_seq.to_be_bytes());
        m.extend_from_slice(&[0, 0, 0]);
        m.extend_from_slice(&u24(body.len()));
        m.extend_from_slice(body);
        self.send_seq += 1;
        m
    }

    /// The flight as datagrams, records packed up to the MTU.
    fn resend(&mut self) -> Vec<Vec<u8>> {
        let items: Vec<(u8, u16, Vec<u8>)> = self
            .flight
            .iter()
            .map(|s| (s.kind, s.epoch, s.body.clone()))
            .collect();
        let mut out: Vec<Vec<u8>> = Vec::new();
        for (kind, epoch, body) in items {
            let r = self.record(kind, epoch, &body);
            match out.last_mut() {
                Some(d) if d.len() + r.len() <= MTU => d.extend_from_slice(&r),
                _ => out.push(r),
            }
        }
        out
    }

    fn send_flight(&mut self, now: Instant, flight: Vec<Sent>) -> Vec<Vec<u8>> {
        self.flight = flight;
        self.timer = Some((now + FIRST_WAIT, FIRST_WAIT));
        self.resend()
    }

    fn send_hello(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut b = vec![0xFE, 0xFD];
        b.extend_from_slice(&self.secrets.random);
        b.push(0); // no session id
        b.push(self.cookie.len() as u8);
        b.extend_from_slice(&self.cookie);
        b.extend_from_slice(&[0, 2]);
        b.extend_from_slice(&SUITE.to_be_bytes());
        b.extend_from_slice(&[1, 0]); // no compression
        let ext: &[u8] = &[
            0x00, 0x0A, 0x00, 0x06, 0x00, 0x04, 0x00, 0x1D, 0x00, 0x17, // groups
            0x00, 0x0B, 0x00, 0x02, 0x01, 0x00, // uncompressed points
            0x00, 0x0D, 0x00, 0x04, 0x00, 0x02, 0x04, 0x01, // rsa_pkcs1_sha256
            0xFF, 0x01, 0x00, 0x01, 0x00, // renegotiation_info
        ];
        b.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        b.extend_from_slice(ext);
        let m = self.message(CLIENT_HELLO, &b);
        // A ClientHello answered by HelloVerifyRequest is not in the transcript.
        self.transcript = m.clone();
        self.send_flight(
            now,
            vec![Sent {
                kind: HANDSHAKE,
                epoch: 0,
                body: m,
            }],
        )
    }

    fn on_handshake_record(
        &mut self,
        now: Instant,
        epoch: u16,
        mut r: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = Vec::new();
        while r.len() >= 12 {
            let kind = r[0];
            let len = r[1..4].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            let seq = u16::from_be_bytes([r[4], r[5]]);
            let off = r[6..9].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            let flen = r[9..12].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            let Some(frag) = r.get(12..12 + flen) else {
                return Err(Error::Handshake("fragment cut short"));
            };
            r = &r[12 + flen..];
            if len > MAX_MESSAGE || off + flen > len {
                return Err(Error::Handshake("bad fragment"));
            }
            if seq < self.recv_seq {
                // Its flight again: ours was lost.
                if !self.flight.is_empty() && self.phase != Phase::Hello {
                    out.extend(self.resend());
                }
                continue;
            }
            // Finished is the only message protected by epoch 1.
            if (kind == FINISHED) != (epoch == 1) {
                continue;
            }
            let p = self.partial.entry(seq).or_insert_with(|| Partial {
                kind,
                data: vec![0; len],
                have: vec![false; len],
            });
            if p.kind != kind || p.data.len() != len {
                return Err(Error::Handshake("fragments disagree"));
            }
            p.data[off..off + flen].copy_from_slice(frag);
            p.have[off..off + flen].iter_mut().for_each(|h| *h = true);
        }
        while let Some(p) = self.partial.get(&self.recv_seq) {
            if !p.have.iter().all(|&h| h) {
                break;
            }
            let p = self.partial.remove(&self.recv_seq).expect("present");
            let seq = self.recv_seq;
            self.recv_seq += 1;
            out.extend(self.on_message(now, seq, p.kind, &p.data)?);
        }
        Ok(out)
    }

    /// The message as the transcript holds it: header with offset 0.
    fn record_in_transcript(&mut self, seq: u16, kind: u8, body: &[u8]) {
        self.transcript.push(kind);
        self.transcript.extend_from_slice(&u24(body.len()));
        self.transcript.extend_from_slice(&seq.to_be_bytes());
        self.transcript.extend_from_slice(&[0, 0, 0]);
        self.transcript.extend_from_slice(&u24(body.len()));
        self.transcript.extend_from_slice(body);
    }

    fn on_message(
        &mut self,
        now: Instant,
        seq: u16,
        kind: u8,
        body: &[u8],
    ) -> Result<Vec<Vec<u8>>, Error> {
        let mut rd = Reader(body);
        match (self.phase, kind) {
            (Phase::Hello, HELLO_VERIFY) => {
                rd.take(2)?;
                self.cookie = rd.vec(1)?.to_vec();
                Ok(self.send_hello(now))
            }
            (Phase::Hello, SERVER_HELLO) => {
                rd.take(2)?;
                self.server_random.copy_from_slice(rd.take(32)?);
                rd.vec(1)?;
                if rd.int(2)? != SUITE as usize || rd.int(1)? != 0 {
                    return Err(Error::Handshake("not ECDHE-RSA-CHACHA20-POLY1305"));
                }
                self.record_in_transcript(seq, kind, body);
                self.phase = Phase::ServerFlight;
                self.timer = None;
                Ok(Vec::new())
            }
            (Phase::ServerFlight, CERTIFICATE) => {
                let mut list = Reader(rd.vec(3)?);
                let der = list.vec(3)?;
                if crypto::sha256(der) != self.pin.sha256 {
                    return Err(Error::Pin("certificate digest"));
                }
                let leaf = x509::leaf(der).ok_or(Error::Pin("certificate unreadable"))?;
                if leaf.common_name != self.pin.common_name {
                    return Err(Error::Pin("certificate name"));
                }
                let key = RsaPublicKey::from_public_key_der(leaf.spki)
                    .map_err(|_| Error::Pin("not an RSA key"))?;
                self.server_key = Some(key);
                self.record_in_transcript(seq, kind, body);
                Ok(Vec::new())
            }
            (Phase::ServerFlight, SERVER_KEY_EXCHANGE) => {
                let params_len = {
                    let mut p = Reader(body);
                    if p.int(1)? != 3 {
                        return Err(Error::Handshake("not a named curve"));
                    }
                    self.group = p.int(2)? as u16;
                    self.server_point = p.vec(1)?.to_vec();
                    body.len() - p.0.len()
                };
                let mut s = Reader(&body[params_len..]);
                if s.int(2)? != RSA_PKCS1_SHA256 as usize {
                    return Err(Error::Handshake("not rsa_pkcs1_sha256"));
                }
                let sig = Signature::try_from(s.vec(2)?)
                    .map_err(|_| Error::Handshake("bad signature"))?;
                let signed = [
                    &self.secrets.random[..],
                    &self.server_random,
                    &body[..params_len],
                ]
                .concat();
                let key = self
                    .server_key
                    .clone()
                    .ok_or(Error::Handshake("no certificate"))?;
                VerifyingKey::<Sha256>::new(key)
                    .verify(&signed, &sig)
                    .map_err(|_| Error::Pin("parameters not signed by the certificate"))?;
                self.record_in_transcript(seq, kind, body);
                Ok(Vec::new())
            }
            (Phase::ServerFlight, SERVER_HELLO_DONE) => {
                self.record_in_transcript(seq, kind, body);
                Ok(self.finish(now)?)
            }
            (Phase::Finished, FINISHED) if self.server_ccs => {
                let want = crypto::prf(
                    &self.master,
                    b"server finished",
                    &crypto::sha256(&self.transcript),
                    12,
                );
                if body != want.as_slice() {
                    return Err(Error::Handshake("server Finished does not match"));
                }
                self.phase = Phase::Connected;
                self.timer = None;
                Ok(Vec::new())
            }
            _ => Err(Error::Handshake("unexpected handshake message")),
        }
    }

    /// The key exchange, our ChangeCipherSpec and Finished.
    fn finish(&mut self, now: Instant) -> Result<Vec<Vec<u8>>, Error> {
        let (public, shared) = self.ecdh()?;
        let cr = self.secrets.random;
        let seed = [&cr[..], &self.server_random].concat();
        self.master = crypto::prf(&shared, b"master secret", &seed, 48);
        self.keys = Some(crypto::key_block(&self.master, &cr, &self.server_random));
        let mut cke = vec![public.len() as u8];
        cke.extend_from_slice(&public);
        let cke = self.message(CLIENT_KEY_EXCHANGE, &cke);
        self.transcript.extend_from_slice(&cke);
        let verify = crypto::prf(
            &self.master,
            b"client finished",
            &crypto::sha256(&self.transcript),
            12,
        );
        let fin = self.message(FINISHED, &verify);
        self.transcript.extend_from_slice(&fin);
        self.phase = Phase::Finished;
        let flight = vec![
            Sent {
                kind: HANDSHAKE,
                epoch: 0,
                body: cke,
            },
            Sent {
                kind: CCS,
                epoch: 0,
                body: vec![1],
            },
            Sent {
                kind: HANDSHAKE,
                epoch: 1,
                body: fin,
            },
        ];
        Ok(self.send_flight(now, flight))
    }

    /// Our public point and the shared secret for the server's group.
    fn ecdh(&self) -> Result<(Vec<u8>, Vec<u8>), Error> {
        match self.group {
            X25519 => {
                let peer: [u8; 32] = self
                    .server_point
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Handshake("bad x25519 point"))?;
                let secret = x25519_dalek::StaticSecret::from(self.secrets.ecdh);
                let public = x25519_dalek::PublicKey::from(&secret);
                let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
                if !shared.was_contributory() {
                    return Err(Error::Handshake("weak x25519 point"));
                }
                Ok((public.as_bytes().to_vec(), shared.as_bytes().to_vec()))
            }
            P256 => {
                use p256::elliptic_curve::sec1::ToEncodedPoint;
                let peer = p256::PublicKey::from_sec1_bytes(&self.server_point)
                    .map_err(|_| Error::Handshake("bad P-256 point"))?;
                // A scalar from the random bytes (rehashed in the rare case
                // they are out of range).
                let mut seed = self.secrets.ecdh;
                let secret = loop {
                    if let Ok(s) = p256::SecretKey::from_slice(&seed) {
                        break s;
                    }
                    seed = crypto::sha256(&seed);
                };
                let public = secret
                    .public_key()
                    .to_encoded_point(false)
                    .as_bytes()
                    .to_vec();
                let shared =
                    p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
                Ok((public, shared.raw_secret_bytes().to_vec()))
            }
            _ => Err(Error::Handshake("no common curve")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_from_hex() {
        let p = Pin::from_hex("cn", &"ab".repeat(32)).unwrap();
        assert_eq!(p.sha256, [0xAB; 32]);
        assert!(Pin::from_hex("cn", "abc").is_none());
        assert!(Pin::from_hex("", &"ab".repeat(32)).is_none());
        assert!(Pin::from_hex("cn", &"zz".repeat(32)).is_none());
    }

    #[test]
    fn the_first_hello_offers_the_one_suite() {
        let mut c = Client::new(
            Pin::from_hex("cn", &"00".repeat(32)).unwrap(),
            Secrets {
                random: [9; 32],
                ecdh: [3; 32],
            },
        );
        let now = Instant::now();
        let d = c.start(now);
        assert_eq!(d.len(), 1);
        let r = &d[0];
        assert_eq!((r[0], r[1], r[2]), (HANDSHAKE, 0xFE, 0xFD));
        assert_eq!(r[HEADER], CLIENT_HELLO);
        assert!(r.windows(2).any(|w| w == [0xCC, 0xA8]));
        // Late: the same hello again, a new record number.
        let again = c.on_timer(now + Duration::from_millis(1100));
        assert_eq!(again[0][HEADER..], r[HEADER..]);
        assert_ne!(again[0][5..11], r[5..11]);
    }

    #[test]
    fn nothing_is_sealed_before_the_handshake() {
        let mut c = Client::new(
            Pin::from_hex("cn", &"00".repeat(32)).unwrap(),
            Secrets {
                random: [0; 32],
                ecdh: [1; 32],
            },
        );
        assert!(c.seal(b"x").is_none());
    }
}

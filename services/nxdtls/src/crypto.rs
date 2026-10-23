//! TLS 1.2's PRF (P_SHA256), the key block of ECDHE-RSA-CHACHA20-POLY1305
//! and its record protection (RFC 7905: no explicit nonce, the sequence
//! number XORed into the IV).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub const TAG: usize = 16;

/// `PRF(secret, label, seed)`, `n` bytes (RFC 5246 §5).
pub fn prf(secret: &[u8], label: &[u8], seed: &[u8], n: usize) -> Vec<u8> {
    let mac = |data: &[&[u8]]| {
        let mut m = <HmacSha256 as Mac>::new_from_slice(secret).expect("any key length");
        data.iter().for_each(|d| m.update(d));
        m.finalize().into_bytes()
    };
    let mut out = Vec::with_capacity(n + 32);
    let mut a = mac(&[label, seed]);
    while out.len() < n {
        out.extend_from_slice(&mac(&[&a, label, seed]));
        a = mac(&[&a]);
    }
    out.truncate(n);
    out
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// One direction's key and IV.
#[derive(Clone)]
pub struct Keys {
    key: [u8; 32],
    iv: [u8; 12],
}

/// The client's and the server's keys from the master secret.
pub fn key_block(master: &[u8], client_random: &[u8], server_random: &[u8]) -> (Keys, Keys) {
    let seed = [server_random, client_random].concat();
    let b = prf(master, b"key expansion", &seed, 2 * 32 + 2 * 12);
    let part = |at: usize, n: usize| b[at..at + n].to_vec();
    let mk = |k: Vec<u8>, iv: Vec<u8>| Keys {
        key: k.try_into().expect("32"),
        iv: iv.try_into().expect("12"),
    };
    (
        mk(part(0, 32), part(64, 12)),
        mk(part(32, 32), part(76, 12)),
    )
}

impl Keys {
    fn nonce(&self, seq: u64) -> [u8; 12] {
        let mut n = self.iv;
        for (i, b) in seq.to_be_bytes().iter().enumerate() {
            n[4 + i] ^= b;
        }
        n
    }

    /// `seq` is epoch (16 bits) and sequence number (48 bits).
    pub fn seal(&self, seq: u64, kind: u8, plain: &[u8]) -> Vec<u8> {
        let aad = aad(seq, kind, plain.len());
        ChaCha20Poly1305::new(&self.key.into())
            .encrypt(
                &self.nonce(seq).into(),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .expect("ChaCha20-Poly1305 seals any length")
    }

    pub fn open(&self, seq: u64, kind: u8, sealed: &[u8]) -> Option<Vec<u8>> {
        let n = sealed.len().checked_sub(TAG)?;
        let aad = aad(seq, kind, n);
        ChaCha20Poly1305::new(&self.key.into())
            .decrypt(
                &self.nonce(seq).into(),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .ok()
    }
}

fn aad(seq: u64, kind: u8, len: usize) -> [u8; 13] {
    let mut a = [0u8; 13];
    a[..8].copy_from_slice(&seq.to_be_bytes());
    a[8] = kind;
    a[9..11].copy_from_slice(&[0xFE, 0xFD]);
    a[11..13].copy_from_slice(&(len as u16).to_be_bytes());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prf_matches_the_tls12_test_vector() {
        // From the IETF TLS working group's PRF test vectors (SHA-256).
        let secret = hex("9bbe436ba940f017b17652849a71db35");
        let seed = hex("a0ba9f936cda311827a6f796ffd5198c");
        let out = prf(&secret, b"test label", &seed, 100);
        assert_eq!(out[..16], hex("e3f229ba727be17b8d122620557cd453")[..],);
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn records_open_what_they_seal_and_nothing_else() {
        let (c, _) = key_block(&[7u8; 48], &[1; 32], &[2; 32]);
        let s = c.seal(1 << 48 | 5, 23, b"hello");
        assert_eq!(s.len(), 5 + TAG);
        assert_eq!(c.open(1 << 48 | 5, 23, &s).unwrap(), b"hello");
        assert!(c.open(1 << 48 | 6, 23, &s).is_none());
        assert!(c.open(1 << 48 | 5, 22, &s).is_none());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}

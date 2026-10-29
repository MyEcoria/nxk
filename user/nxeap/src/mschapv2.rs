//! MS-CHAPv2 as an EAP method (RFC 2759, draft-kamath-pppext-eap-mschapv2):
//! the NT response to the server's challenge, the check of its
//! authenticator response, and the MPPE keys PEAP binds to (RFC 3079).
//! MD4 (RFC 1320) and DES (FIPS 46-3) are here for it alone.

use alloc::vec::Vec;

use sha1::{Digest, Sha1};

pub const TYPE: u8 = 26;
const OP_CHALLENGE: u8 = 1;
const OP_RESPONSE: u8 = 2;
const OP_SUCCESS: u8 = 3;
const OP_FAILURE: u8 = 4;
const CHALLENGE_LEN: usize = 16;
/// Peer challenge, 8 reserved bytes, NT response, flags.
const RESPONSE_LEN: usize = 49;

const MAGIC1: &[u8] = b"Magic server to client signing constant";
const MAGIC2: &[u8] = b"Pad to make it do more than one iteration";
const MASTER_MAGIC: &[u8] = b"This is the MPPE Master Key";
const SEND_MAGIC: &[u8] =
    b"On the client side, this is the send key; on the server side, it is the receive key.";
const RECV_MAGIC: &[u8] =
    b"On the client side, this is the receive key; on the server side, it is the send key.";

/// The password as MS-CHAPv2 uses it: MD4 of its UTF-16LE form.
pub fn nt_hash(password: &str) -> [u8; 16] {
    let wide: Vec<u8> = password.encode_utf16().flat_map(u16::to_le_bytes).collect();
    md4(&wide)
}

/// What one step of the method calls for.
pub enum Step {
    /// An EAP-Response body (type byte first) to send.
    Reply(Vec<u8>),
    /// The server proved it knows the password: reply, then done.
    Success(Vec<u8>),
    /// Refused (`E=` code and message from the server), reply then stop.
    Failure(Vec<u8>, Vec<u8>),
}

/// One run of the method.
pub struct Peer {
    identity: Vec<u8>,
    hash: [u8; 16],
    random: fn(&mut [u8]),
    /// Expected authenticator response and master key, once answered.
    expect: Option<[u8; 20]>,
    master: Option<[u8; 16]>,
    success: bool,
}

impl Peer {
    pub fn new(identity: &[u8], hash: [u8; 16], random: fn(&mut [u8])) -> Self {
        Self {
            identity: identity.to_vec(),
            hash,
            random,
            expect: None,
            master: None,
            success: false,
        }
    }

    /// One EAP-Request body of type 26 (type byte first).
    pub fn receive(&mut self, req: &[u8]) -> Result<Step, &'static str> {
        if req.len() < 5 || req[0] != TYPE {
            return Err("not an MS-CHAPv2 request");
        }
        let (op, id) = (req[1], req[2]);
        match op {
            OP_CHALLENGE => {
                let c = req.get(6..6 + CHALLENGE_LEN).filter(|_| req[5] == 16);
                let c = c.ok_or("malformed MS-CHAPv2 challenge")?;
                let mut auth = [0u8; CHALLENGE_LEN];
                auth.copy_from_slice(c);
                Ok(Step::Reply(self.respond(id, &auth)))
            }
            OP_SUCCESS => {
                let expect = self.expect.ok_or("MS-CHAPv2 success before a response")?;
                if !authenticator_ok(&req[5..], &expect) {
                    return Err("server authenticator response is wrong (rogue server?)");
                }
                self.success = true;
                Ok(Step::Success([TYPE, OP_SUCCESS].to_vec()))
            }
            OP_FAILURE => {
                let msg = req.get(5..).unwrap_or(&[]).to_vec();
                let code = field(&msg, b"E=").unwrap_or(b"?").to_vec();
                Ok(Step::Failure([TYPE, OP_FAILURE].to_vec(), code))
            }
            _ => Err("unknown MS-CHAPv2 operation"),
        }
    }

    fn respond(&mut self, id: u8, auth: &[u8; 16]) -> Vec<u8> {
        let mut peer = [0u8; CHALLENGE_LEN];
        (self.random)(&mut peer);
        let user = username(&self.identity);
        let ch = challenge_hash(&peer, auth, user);
        let nt = challenge_response(&ch, &self.hash);
        self.expect = Some(authenticator(&self.hash, &nt, &ch));
        self.master = Some(master_key(&self.hash, &nt));

        let ms_len = 4 + 1 + RESPONSE_LEN + self.identity.len();
        let mut r = Vec::with_capacity(1 + ms_len);
        r.extend_from_slice(&[TYPE, OP_RESPONSE, id]);
        r.extend_from_slice(&(ms_len as u16).to_be_bytes());
        r.push(RESPONSE_LEN as u8);
        r.extend_from_slice(&peer);
        r.extend_from_slice(&[0; 8]);
        r.extend_from_slice(&nt);
        r.push(0);
        r.extend_from_slice(&self.identity);
        r
    }

    /// The inner session key PEAP's crypto binding takes (hostap's
    /// `eap_mschapv2_getKey`): the peer's MPPE send key, then its
    /// receive key. None before a verified success.
    pub fn isk(&self) -> Option<[u8; 32]> {
        let m = self.master.filter(|_| self.success)?;
        let mut k = [0u8; 32];
        k[..16].copy_from_slice(&start_key(&m, SEND_MAGIC));
        k[16..].copy_from_slice(&start_key(&m, RECV_MAGIC));
        Some(k)
    }
}

/// The value after `key` in a "S=... M=..." message.
fn field<'a>(msg: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let at = msg.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &msg[at..];
    Some(&rest[..rest.iter().position(|&b| b == b' ').unwrap_or(rest.len())])
}

fn authenticator_ok(msg: &[u8], expect: &[u8; 20]) -> bool {
    let Some(s) = field(msg, b"S=").filter(|s| s.len() == 40) else {
        return false;
    };
    let hex = |c: u8| (c as char).to_digit(16);
    let mut diff = 0u8;
    for (i, e) in expect.iter().enumerate() {
        match (hex(s[2 * i]), hex(s[2 * i + 1])) {
            (Some(h), Some(l)) => diff |= ((h << 4 | l) as u8) ^ e,
            _ => return false,
        }
    }
    diff == 0
}

/// The user name the hashes cover: without a `DOMAIN\` prefix.
fn username(identity: &[u8]) -> &[u8] {
    match identity.iter().position(|&b| b == b'\\') {
        Some(i) => &identity[i + 1..],
        None => identity,
    }
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn challenge_hash(peer: &[u8; 16], auth: &[u8; 16], user: &[u8]) -> [u8; 8] {
    let mut c = [0u8; 8];
    c.copy_from_slice(&sha1(&[peer, auth, user])[..8]);
    c
}

/// The NT response: the challenge hash DES-encrypted under the three
/// 7-byte thirds of the password hash padded to 21 bytes.
fn challenge_response(ch: &[u8; 8], hash: &[u8; 16]) -> [u8; 24] {
    let mut z = [0u8; 21];
    z[..16].copy_from_slice(hash);
    let mut r = [0u8; 24];
    for i in 0..3 {
        let k: [u8; 7] = z[7 * i..7 * i + 7].try_into().unwrap();
        r[8 * i..8 * i + 8].copy_from_slice(&des_encrypt(&k, ch));
    }
    r
}

fn authenticator(hash: &[u8; 16], nt: &[u8; 24], ch: &[u8; 8]) -> [u8; 20] {
    let hh = md4(hash);
    let d = sha1(&[&hh, nt, MAGIC1]);
    sha1(&[&d, ch, MAGIC2])
}

fn master_key(hash: &[u8; 16], nt: &[u8; 24]) -> [u8; 16] {
    let mut m = [0u8; 16];
    m.copy_from_slice(&sha1(&[&md4(hash), nt, MASTER_MAGIC])[..16]);
    m
}

/// RFC 3079 3.4 `GetAsymmetricStartKey`, 128-bit.
fn start_key(master: &[u8; 16], magic: &[u8]) -> [u8; 16] {
    let mut k = [0u8; 16];
    k.copy_from_slice(&sha1(&[master, &[0; 40], magic, &[0xF2; 40]])[..16]);
    k
}

pub fn md4(data: &[u8]) -> [u8; 16] {
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_le_bytes());
    for block in msg.chunks(64) {
        let x: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = h;
        let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
        let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
        let hh = |x: u32, y: u32, z: u32| x ^ y ^ z;
        for i in 0..16 {
            let s = [3, 7, 11, 19][i % 4];
            let t = a.wrapping_add(f(b, c, d)).wrapping_add(x[i]).rotate_left(s);
            (a, b, c, d) = (d, t, b, c);
        }
        for i in 0..16 {
            let s = [3, 5, 9, 13][i % 4];
            let k = (i % 4) * 4 + i / 4;
            let t = a
                .wrapping_add(g(b, c, d))
                .wrapping_add(x[k])
                .wrapping_add(0x5a827999)
                .rotate_left(s);
            (a, b, c, d) = (d, t, b, c);
        }
        for i in 0..16 {
            let s = [3, 9, 11, 15][i % 4];
            let k = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15][i];
            let t = a
                .wrapping_add(hh(b, c, d))
                .wrapping_add(x[k])
                .wrapping_add(0x6ed9eba1)
                .rotate_left(s);
            (a, b, c, d) = (d, t, b, c);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, w) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

// DES tables (FIPS 46-3), bit positions counted from 1 at the MSB.
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// The `table` bits of `x` (`width` bits wide, MSB first), packed.
fn permute(x: u64, width: u32, table: &[u8]) -> u64 {
    table
        .iter()
        .fold(0, |acc, &p| (acc << 1) | ((x >> (width - p as u32)) & 1))
}

/// DES with a 56-bit key given as 7 bytes (parity bits left out, as
/// MS-CHAP does).
fn des_encrypt(key7: &[u8; 7], block: &[u8; 8]) -> [u8; 8] {
    let k56 = key7.iter().fold(0u64, |a, &b| (a << 8) | b as u64);
    // Spread to 64 bits, 7 key bits then a (zero) parity bit per byte.
    let k64 = (0..8).fold(0u64, |a, i| {
        (a << 8) | (((k56 >> (49 - 7 * i)) & 0x7F) << 1)
    });
    let cd = permute(k64, 64, &PC1);
    let (mut c, mut d) = ((cd >> 28) as u32, (cd & 0x0FFF_FFFF) as u32);
    let rot = |v: u32, n: u32| ((v << n) | (v >> (28 - n))) & 0x0FFF_FFFF;
    let mut keys = [0u64; 16];
    for (i, k) in keys.iter_mut().enumerate() {
        c = rot(c, SHIFTS[i]);
        d = rot(d, SHIFTS[i]);
        *k = permute(((c as u64) << 28) | d as u64, 56, &PC2);
    }
    let x = permute(u64::from_be_bytes(*block), 64, &IP);
    let (mut l, mut r) = ((x >> 32) as u32, x as u32);
    for k in keys {
        let e = permute(r as u64, 32, &E) ^ k;
        let s = (0..8).fold(0u32, |acc, i| {
            let six = ((e >> (42 - 6 * i)) & 0x3F) as usize;
            let row = ((six & 0x20) >> 4) | (six & 1);
            (acc << 4) | S[i][row * 16 + ((six >> 1) & 0xF)] as u32
        });
        (l, r) = (r, l ^ permute(s as u64, 32, &P) as u32);
    }
    permute(((r as u64) << 32) | l as u64, 64, &FP).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn md4_rfc1320() {
        assert_eq!(md4(b"").to_vec(), h("31d6cfe0d16ae931b73c59d7e0c089c0"));
        assert_eq!(md4(b"abc").to_vec(), h("a448017aaf21d8525fc10ae87aa6729d"));
        let long =
            b"12345678901234567890123456789012345678901234567890123456789012345678901234567890";
        assert_eq!(md4(long).to_vec(), h("e33b4ddc9c38f2199c3e7b164fcc0536"));
    }

    #[test]
    fn des_known_answer() {
        // FIPS 81 / the classic "Now is t" vector, key 0123456789ABCDEF
        // with its parity bits dropped.
        let k64 = 0x0123456789ABCDEFu64;
        let k56 = (0..8).fold(0u64, |a, i| (a << 7) | ((k64 >> (57 - 8 * i)) & 0x7F));
        let k7: [u8; 7] = k56.to_be_bytes()[1..].try_into().unwrap();
        let c = des_encrypt(&k7, b"Now is t");
        assert_eq!(c.to_vec(), h("3fa40e8a984d4815"));
    }

    // RFC 2759 9.2 and RFC 3079 3.5.3.
    const AUTH: &str = "5b5d7c7d7b3f2f3e3c2c602132262628";
    const PEER: &str = "21402324255e262a28295f2b3a337c7e";

    #[test]
    fn rfc2759_vectors() {
        let hash = nt_hash("clientPass");
        assert_eq!(hash.to_vec(), h("44ebba8d5312b8d611474411f56989ae"));
        let ch = challenge_hash(
            &h(PEER).try_into().unwrap(),
            &h(AUTH).try_into().unwrap(),
            b"User",
        );
        assert_eq!(ch.to_vec(), h("d02e4386bce91226"));
        let nt = challenge_response(&ch, &hash);
        assert_eq!(
            nt.to_vec(),
            h("82309ecd8d708b5ea08faa3981cd83544233114a3d85d6df")
        );
        assert_eq!(md4(&hash).to_vec(), h("41c00c584bd2d91c4017a2a12fa59f3f"));
        let a = authenticator(&hash, &nt, &ch);
        assert_eq!(a.to_vec(), h("407a5589115fd0d6209f510fe9c04566932cda56"));
        assert!(authenticator_ok(
            b"S=407A5589115FD0D6209F510FE9C04566932CDA56 M=OK",
            &a
        ));
        assert!(!authenticator_ok(
            b"S=407A5589115FD0D6209F510FE9C04566932CDA57",
            &a
        ));
        let m = master_key(&hash, &nt);
        assert_eq!(m.to_vec(), h("fdece3717a8c838cb388e527ae3cdd31"));
        assert_eq!(
            start_key(&m, SEND_MAGIC).to_vec(),
            h("d5f0e9521e3ea9589645e86051c82226")
        );
        assert_eq!(
            start_key(&m, RECV_MAGIC).to_vec(),
            h("8b7cdc149b993a1ba118cb153f56dccb")
        );
    }

    #[test]
    fn exchange() {
        fn peer_challenge(b: &mut [u8]) {
            b.copy_from_slice(&h(PEER));
        }
        let mut p = Peer::new(b"DOMAIN\\User", nt_hash("clientPass"), peer_challenge);
        let mut req = [TYPE, OP_CHALLENGE, 7, 0, 26, 16].to_vec();
        req.extend_from_slice(&h(AUTH));
        req.extend_from_slice(b"srv");
        let Ok(Step::Reply(r)) = p.receive(&req) else {
            panic!("no response")
        };
        assert_eq!(&r[..3], &[TYPE, OP_RESPONSE, 7]);
        assert_eq!(u16::from_be_bytes([r[3], r[4]]) as usize, r.len() - 1);
        assert_eq!(
            r[30..54].to_vec(),
            h("82309ecd8d708b5ea08faa3981cd83544233114a3d85d6df")
        );
        assert_eq!(&r[55..], b"DOMAIN\\User");
        assert!(p.isk().is_none());
        let mut ok = [TYPE, OP_SUCCESS, 7, 0, 0].to_vec();
        ok.extend_from_slice(b"S=407A5589115FD0D6209F510FE9C04566932CDA56 M=Welcome");
        assert!(matches!(p.receive(&ok), Ok(Step::Success(_))));
        assert!(p.isk().is_some());
    }
}

//! What WPA2-Personal asks of the host (802.11-2016 12.7): SHA-1 and
//! HMAC-SHA1 (FIPS 180-4, RFC 2104), the passphrase hash (PBKDF2,
//! 802.11 J.4), the 802.11 PRF, AES-128 (FIPS 197) and AES key unwrap
//! (RFC 3394). Only `core`: the known-answer tests run on the PC with
//! `rustc --edition 2021 --test user/wlan/hosttest.rs -o T && ./T`.

#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Sha1 {
    pub const fn new() -> Self {
        Sha1 {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if self.n > 0 {
            let take = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + take].copy_from_slice(&data[..take]);
            self.n += take;
            data = &data[take..];
            if self.n < 64 {
                return;
            }
            let block = self.buf;
            compress(&mut self.h, &block);
            self.n = 0;
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for b in blocks {
            compress(&mut self.h, b);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.n = rest.len();
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.len * 8;
        self.update(&[0x80]);
        while self.n != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 20];
        for (o, h) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = h.to_be_bytes();
        }
        out
    }
}

fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (w, b) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *w = u32::from_be_bytes(*b);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, &wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A827999),
            20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
            _ => (b ^ c ^ d, 0xCA62C1D6),
        };
        let t = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (h, v) in h.iter_mut().zip([a, b, c, d, e]) {
        *h = h.wrapping_add(v);
    }
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::new();
    s.update(data);
    s.finish()
}

/// HMAC-SHA1 with its key absorbed once: both pads already hashed, so
/// PBKDF2's 8192 MACs cost two blocks each.
pub struct Hmac {
    inner: Sha1,
    outer: Sha1,
}

impl Hmac {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 64];
        if key.len() > 64 {
            k[..20].copy_from_slice(&sha1(key));
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let (mut inner, mut outer) = (Sha1::new(), Sha1::new());
        inner.update(&k.map(|b| b ^ 0x36));
        outer.update(&k.map(|b| b ^ 0x5C));
        Hmac { inner, outer }
    }

    /// MAC of the concatenation of `parts`.
    pub fn mac(&self, parts: &[&[u8]]) -> [u8; 20] {
        let mut i = self.inner.clone();
        for p in parts {
            i.update(p);
        }
        let mut o = self.outer.clone();
        o.update(&i.finish());
        o.finish()
    }
}

pub fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    Hmac::new(key).mac(parts)
}

/// Pairwise master key of a WPA2-Personal network: PBKDF2-HMAC-SHA1 of
/// the passphrase salted with the SSID, 4096 rounds, 256 bits.
pub fn psk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let prf = Hmac::new(passphrase);
    let mut out = [0u8; 32];
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let mut u = prf.mac(&[ssid, &(i as u32 + 1).to_be_bytes()]);
        let mut t = u;
        for _ in 1..4096 {
            u = prf.mac(&[&u]);
            for (t, u) in t.iter_mut().zip(u) {
                *t ^= u;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    out
}

/// 802.11 PRF (12.7.1.2): HMAC-SHA1(key, label || 0 || data || i) for
/// i = 0, 1, ... until `out` is full.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let h = Hmac::new(key);
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let d = h.mac(&[label, &[0], data, &[i as u8]]);
        chunk.copy_from_slice(&d[..chunk.len()]);
    }
}

/// Equal contents, in a time that does not depend on where they differ.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

const fn xtime(x: u8) -> u8 {
    (x << 1) ^ if x & 0x80 != 0 { 0x1B } else { 0 }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

/// S-box and its inverse, from GF(2^8) inverses (powers of 3) and the
/// FIPS 197 affine map, rather than 512 typed-in bytes.
const fn sboxes() -> [[u8; 256]; 2] {
    let (mut exp, mut log) = ([0u8; 256], [0u8; 256]);
    let mut x: u8 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x;
        log[x as usize] = i as u8;
        x ^= xtime(x);
        i += 1;
    }
    let mut s = [[0u8; 256]; 2];
    let mut v = 0;
    while v < 256 {
        let inv = if v == 0 {
            0
        } else {
            exp[(255 - log[v] as usize) % 255]
        };
        let b = inv
            ^ inv.rotate_left(1)
            ^ inv.rotate_left(2)
            ^ inv.rotate_left(3)
            ^ inv.rotate_left(4)
            ^ 0x63;
        s[0][v] = b;
        s[1][b as usize] = v as u8;
        v += 1;
    }
    s
}

const SBOX: [[u8; 256]; 2] = sboxes();

pub struct Aes128 {
    rk: [[u8; 16]; 11],
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        let mut w = [[0u8; 4]; 44];
        for (w, k) in w.iter_mut().zip(key.as_chunks::<4>().0) {
            *w = *k;
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                let s = |b: u8| SBOX[0][b as usize];
                t = [s(t[1]) ^ rcon, s(t[2]), s(t[3]), s(t[0])];
                rcon = xtime(rcon);
            }
            for j in 0..4 {
                w[i][j] = w[i - 4][j] ^ t[j];
            }
        }
        let mut rk = [[0u8; 16]; 11];
        for (r, k) in rk.iter_mut().enumerate() {
            for (c, col) in k.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                *col = w[4 * r + c];
            }
        }
        Aes128 { rk }
    }

    /// Only the tests encrypt (FIPS 197 vector, the access point's key
    /// wrap): the driver just unwraps.
    #[cfg(test)]
    pub fn encrypt(&self, b: &mut [u8; 16]) {
        add(b, &self.rk[0]);
        for round in 1..=10 {
            b.iter_mut().for_each(|x| *x = SBOX[0][*x as usize]);
            shift_rows(b, false);
            if round != 10 {
                mix_columns(b, [2, 3, 1, 1]);
            }
            add(b, &self.rk[round]);
        }
    }

    pub fn decrypt(&self, b: &mut [u8; 16]) {
        add(b, &self.rk[10]);
        for round in (0..10).rev() {
            shift_rows(b, true);
            b.iter_mut().for_each(|x| *x = SBOX[1][*x as usize]);
            add(b, &self.rk[round]);
            if round != 0 {
                mix_columns(b, [14, 11, 13, 9]);
            }
        }
    }
}

fn add(b: &mut [u8; 16], k: &[u8; 16]) {
    for (b, k) in b.iter_mut().zip(k) {
        *b ^= k;
    }
}

/// Row r of the column-major state rotated left by r (right to undo).
fn shift_rows(b: &mut [u8; 16], inverse: bool) {
    let s = *b;
    for r in 1..4 {
        for c in 0..4 {
            let from = if inverse {
                (c + 4 - r) % 4
            } else {
                (c + r) % 4
            };
            b[r + 4 * c] = s[r + 4 * from];
        }
    }
}

/// Each column times the circulant matrix whose first row is `m`.
fn mix_columns(b: &mut [u8; 16], m: [u8; 4]) {
    for col in b.as_chunks_mut::<4>().0 {
        let a = *col;
        for (r, out) in col.iter_mut().enumerate() {
            *out = (0..4).fold(0, |acc, k| acc ^ gmul(m[(k + 4 - r) % 4], a[k]));
        }
    }
}

/// RFC 3394 key unwrap of `wrapped` into `out`, 8 bytes shorter; false
/// when the integrity check fails (wrong key or damaged data).
pub fn unwrap(kek: &[u8; 16], wrapped: &[u8], out: &mut [u8]) -> bool {
    if !wrapped.len().is_multiple_of(8) || wrapped.len() < 16 || out.len() != wrapped.len() - 8 {
        return false;
    }
    let n = out.len() / 8;
    let aes = Aes128::new(kek);
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    out.copy_from_slice(&wrapped[8..]);
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t = (n * j + i + 1) as u64;
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&(u64::from_be_bytes(a) ^ t).to_be_bytes());
            b[8..].copy_from_slice(&out[8 * i..8 * i + 8]);
            aes.decrypt(&mut b);
            a.copy_from_slice(&b[..8]);
            out[8 * i..8 * i + 8].copy_from_slice(&b[8..]);
        }
    }
    same(&a, &[0xA6; 8])
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn hex(s: &str) -> std::vec::Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 3394 wrap, the authenticator's side: for the tests only.
    pub fn wrap(kek: &[u8; 16], plain: &[u8]) -> std::vec::Vec<u8> {
        let n = plain.len() / 8;
        let aes = Aes128::new(kek);
        let mut a = [0xA6u8; 8];
        let mut r = plain.to_vec();
        for j in 0..6 {
            for i in 0..n {
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&a);
                b[8..].copy_from_slice(&r[8 * i..8 * i + 8]);
                aes.encrypt(&mut b);
                let t = (n * j + i + 1) as u64;
                a = (u64::from_be_bytes(b[..8].try_into().unwrap()) ^ t).to_be_bytes();
                r[8 * i..8 * i + 8].copy_from_slice(&b[8..]);
            }
        }
        let mut out = a.to_vec();
        out.extend_from_slice(&r);
        out
    }

    #[test]
    fn sha1_fips180() {
        assert_eq!(
            sha1(b"abc").to_vec(),
            hex("a9993e364706816aba3e25717850c26c9cd0d89d")
        );
        let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha1(two).to_vec(),
            hex("84983e441c3bd26ebaae4aa1f95129e5e54670f1")
        );
        let mut s = Sha1::new();
        for _ in 0..1000 {
            s.update(&[b'a'; 1000]);
        }
        assert_eq!(
            s.finish().to_vec(),
            hex("34aa973cd4c4daa4f61eeb2bdbad27316534016f")
        );
    }

    #[test]
    fn hmac_rfc2202() {
        let mac = hmac_sha1(&[0x0B; 20], &[b"Hi There"]);
        assert_eq!(
            mac.to_vec(),
            hex("b617318655057264e28bc0b6fb378c8ef146be00")
        );
        let mac = hmac_sha1(b"Jefe", &[b"what do ya ", b"want for nothing?"]);
        assert_eq!(
            mac.to_vec(),
            hex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79")
        );
        // Key longer than a block: hashed first.
        let mac = hmac_sha1(
            &[0xAA; 80],
            &[b"Test Using Larger Than Block-Size Key - Hash Key First"],
        );
        assert_eq!(
            mac.to_vec(),
            hex("aa4ae5e15272d00e95705637ce8a3b55ed402112")
        );
    }

    #[test]
    fn psk_ieee80211_j4() {
        assert_eq!(
            psk(b"password", b"IEEE").to_vec(),
            hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e")
        );
        assert_eq!(
            psk(b"ThisIsAPassword", b"ThisIsASSID").to_vec(),
            hex("0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af")
        );
    }

    #[test]
    fn prf_ieee80211() {
        // 802.11 J.3 test case 1, cross-checked with Python's hmac.
        let mut out = [0u8; 48];
        prf(&[0x0B; 20], b"prefix", b"Hi There", &mut out);
        let want = "bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606\
                    e17d8da35402ffee75df78c3d31e0f889f012120c0862beb";
        assert_eq!(out.to_vec(), hex(want));
    }

    #[test]
    fn aes_fips197() {
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut b: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        let aes = Aes128::new(&key);
        aes.encrypt(&mut b);
        assert_eq!(b.to_vec(), hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt(&mut b);
        assert_eq!(b.to_vec(), hex("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn unwrap_rfc3394() {
        let kek: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let wrapped = hex("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        let mut out = [0u8; 16];
        assert!(unwrap(&kek, &wrapped, &mut out));
        assert_eq!(out.to_vec(), hex("00112233445566778899aabbccddeeff"));
        assert_eq!(wrap(&kek, &out), wrapped);
        let mut bad = wrapped.clone();
        bad[20] ^= 1;
        assert!(!unwrap(&kek, &bad, &mut out));
    }
}

//! Aleatoire du noyau : ChaCha20 avec effacement de cle.
//!
//! Graine prise au demarrage dans le generateur materiel du moteur de
//! securite (SE) du Tegra, melangee a la gigue du compteur. Chaque bloc de
//! flux fournit d'abord une nouvelle cle, aussitot substituee a l'ancienne :
//! une fuite de l'etat ne revele rien des valeurs deja produites.
//! Sert a la disposition aleatoire des espaces d'adressage et a `getrandom`.

use crate::arch::timer;
use crate::kern::bkl::BklCell;
use crate::mmio::{read32, write32};

struct Rng {
    key: [u32; 8],
    counter: u64,
    buf: [u8; 64],
    pos: usize,
}

static RNG: BklCell<Rng> = BklCell::new(Rng { key: [0; 8], counter: 0, buf: [0; 64], pos: 64 });

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn block(key: &[u32; 8], counter: u64, out: &mut [u8; 64]) {
    let mut s = [0u32; 16];
    s[0] = 0x6170_7865;
    s[1] = 0x3320_646e;
    s[2] = 0x7962_2d32;
    s[3] = 0x6b20_6574;
    s[4..12].copy_from_slice(key);
    s[12] = counter as u32;
    s[13] = (counter >> 32) as u32;
    let init = s;
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12);
        qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14);
        qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15);
        qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13);
        qr(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
}

impl Rng {
    /// Un bloc de flux : 32 octets deviennent la nouvelle cle, 32 sont livres.
    fn refill(&mut self) {
        let mut out = [0u8; 64];
        block(&self.key, self.counter, &mut out);
        self.counter = self.counter.wrapping_add(1);
        for i in 0..8 {
            self.key[i] = u32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
        }
        self.buf[..32].fill(0);
        self.buf[32..].copy_from_slice(&out[32..]);
        out.fill(0);
        self.pos = 32;
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            if self.pos == 64 {
                self.refill();
            }
            let n = (64 - self.pos).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.buf[self.pos..self.pos + n].fill(0);
            self.pos += n;
            done += n;
        }
    }

    fn mix(&mut self, seed: &[u8]) {
        for (i, b) in seed.iter().enumerate() {
            let k = (i / 4) % 8;
            self.key[k] ^= (*b as u32) << ((i % 4) * 8);
            if i % 32 == 31 {
                self.refill();
            }
        }
        self.pos = 64;
    }
}

pub fn fill(out: &mut [u8]) {
    RNG.get().fill(out)
}

pub fn next_u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}

// --- Moteur de securite (SE) : generateur SP 800-90 ---
const SE: usize = 0x7001_2000;
const SE_OPERATION: usize = SE + 0x008;
const SE_INT_STATUS: usize = SE + 0x010;
const SE_CONFIG: usize = SE + 0x014;
const SE_IN_LL_ADDR: usize = SE + 0x018;
const SE_OUT_LL_ADDR: usize = SE + 0x024;
const SE_CRYPTO_CONFIG: usize = SE + 0x304;
const SE_CRYPTO_LAST_BLOCK: usize = SE + 0x318;
const SE_RNG_CONFIG: usize = SE + 0x340;
const SE_RNG_SRC_CONFIG: usize = SE + 0x344;
const SE_RNG_RESEED_INTERVAL: usize = SE + 0x348;
const SE_ERR_STATUS: usize = SE + 0x804;
const SE_INT_OP_DONE: u32 = 1 << 4;

#[repr(C, align(64))]
struct SeBuf {
    ll: [u32; 4], // num, adresse, taille
    data: [u8; 64],
}

static mut SE_BUF: SeBuf = SeBuf { ll: [0; 4], data: [0; 64] };

/// 64 octets du generateur materiel, ou `None` si le SE ne repond pas.
fn se_random() -> Option<[u8; 64]> {
    let b = unsafe { &mut *core::ptr::addr_of_mut!(SE_BUF) };
    let data_pa = b.data.as_ptr() as u64;
    b.ll = [0, data_pa as u32, 64, 0];
    let ll_pa = b.ll.as_ptr() as u64;
    // Le SE lit et ecrit la memoire sans passer par les caches du CPU.
    clean_inval(ll_pa, 16);
    clean_inval(data_pa, 64);
    unsafe {
        write32(SE_CONFIG, 2 << 12); // ENC_ALG RNG, DST memoire, cle 128 bits
        write32(SE_CRYPTO_CONFIG, 1 << 3); // INPUT_SEL aleatoire
        write32(SE_RNG_CONFIG, 1 << 2); // SRC_ENTROPY, mode normal
        write32(SE_RNG_SRC_CONFIG, read32(SE_RNG_SRC_CONFIG) | (1 << 1));
        write32(SE_RNG_RESEED_INTERVAL, 4096);
        write32(SE_CRYPTO_LAST_BLOCK, 64 / 16 - 1);
        write32(SE_IN_LL_ADDR, 0);
        write32(SE_OUT_LL_ADDR, ll_pa as u32);
        write32(SE_ERR_STATUS, read32(SE_ERR_STATUS));
        write32(SE_INT_STATUS, read32(SE_INT_STATUS));
        write32(SE_OPERATION, 1); // START
    }
    let deadline = timer::counter() + timer::ns_to_ticks(10_000_000);
    while unsafe { read32(SE_INT_STATUS) } & SE_INT_OP_DONE == 0 {
        if timer::counter() > deadline {
            return None;
        }
        core::hint::spin_loop();
    }
    if unsafe { read32(SE_ERR_STATUS) } != 0 {
        return None;
    }
    clean_inval(data_pa, 64);
    let out = b.data;
    b.data.fill(0);
    (out.iter().any(|&x| x != 0)).then_some(out)
}

fn clean_inval(pa: u64, len: u64) {
    let mut a = pa & !63;
    while a < pa + len {
        unsafe { core::arch::asm!("dc civac, {}", in(reg) a, options(nostack)) };
        a += 64;
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack)) };
}

/// Graine initiale : SE si disponible, plus la gigue du compteur.
pub fn init() -> bool {
    let r = RNG.get();
    let hw = se_random();
    if let Some(s) = hw {
        r.mix(&s);
    }
    let mut jitter = [0u8; 64];
    for (i, j) in jitter.iter_mut().enumerate() {
        let t0 = timer::counter();
        let mut x = t0;
        for k in 0..(16 + i) {
            x = x.rotate_left(7) ^ k as u64;
        }
        core::hint::black_box(x);
        *j = (timer::counter() ^ t0 ^ x) as u8;
    }
    r.mix(&jitter);
    r.mix(&timer::counter().to_le_bytes());
    hw.is_some()
}

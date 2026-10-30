//! The AES modes of the formats, on `aes::Aes128`: XTS for the NCA header,
//! CTR for the sections, ECB for the key area.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;

const BLOCK: usize = 16;
/// XTS sector size of NCA headers.
pub const SECTOR: usize = 0x200;

/// Decrypts whole 0x200-byte sectors of `data`, the first numbered `first`.
/// The tweak is the sector number as a big-endian 128-bit value (NCA).
pub fn xts_decrypt(key: &[u8; 32], first: u64, data: &mut [u8]) {
    xts(key, first, data, true);
}

/// Encrypts whole sectors, as `xts_decrypt` (tests build headers with it).
pub fn xts_encrypt(key: &[u8; 32], first: u64, data: &mut [u8]) {
    xts(key, first, data, false);
}

/// XTS over `data`, direction chosen by `decrypt`.
fn xts(key: &[u8; 32], first: u64, data: &mut [u8], decrypt: bool) {
    let data_key = Aes128::new(GenericArray::from_slice(&key[..16]));
    let tweak_key = Aes128::new(GenericArray::from_slice(&key[16..]));
    for (n, sector) in data.chunks_mut(SECTOR).enumerate() {
        let mut tweak = GenericArray::from(u128::from(first + n as u64).to_be_bytes());
        tweak_key.encrypt_block(&mut tweak);
        for block in sector.chunks_mut(BLOCK) {
            xor_in(block, &tweak);
            let b = GenericArray::from_mut_slice(block);
            if decrypt {
                data_key.decrypt_block(b);
            } else {
                data_key.encrypt_block(b);
            }
            xor_in(block, &tweak);
            double(&mut tweak);
        }
    }
}

/// XORs `tweak` into `block`.
fn xor_in(block: &mut [u8], tweak: &GenericArray<u8, aes::cipher::consts::U16>) {
    for (x, t) in block.iter_mut().zip(tweak.iter()) {
        *x ^= t;
    }
}

/// Multiplies the tweak by x in GF(2^128), little-endian (IEEE 1619).
fn double(tweak: &mut GenericArray<u8, aes::cipher::consts::U16>) {
    let mut carry = 0u8;
    for b in tweak.iter_mut() {
        let next = *b >> 7;
        *b = (*b << 1) | carry;
        carry = next;
    }
    if carry != 0 {
        tweak[0] ^= 0x87;
    }
}

/// XORs `data` with the CTR keystream of `key` at byte `offset` of a
/// section. The counter block is `upper` (big-endian) then the block index
/// `offset / 16` (big-endian), as NCA sections use.
pub fn ctr_xor(key: &[u8; 16], upper: u64, offset: u64, data: &mut [u8]) {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut ctr = (u128::from(upper) << 64) | u128::from(offset >> 4);
    let mut skip = (offset & 0xF) as usize;
    let mut i = 0;
    while i < data.len() {
        let mut ks = GenericArray::from(ctr.to_be_bytes());
        cipher.encrypt_block(&mut ks);
        let n = (BLOCK - skip).min(data.len() - i);
        for j in 0..n {
            data[i + j] ^= ks[skip + j];
        }
        i += n;
        skip = 0;
        ctr = ctr.wrapping_add(1);
    }
}

/// AES-128 ECB decryption of one block (the NCA key area).
pub fn ecb_decrypt(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut b = GenericArray::from(*block);
    cipher.decrypt_block(&mut b);
    b.into()
}

/// AES-128 ECB encryption of one block (tests wrap keys with it).
pub fn ecb_encrypt(key: &[u8; 16], block: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut b = GenericArray::from(*block);
    cipher.encrypt_block(&mut b);
    b.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> std::string::String {
        b.iter().map(|x| std::format!("{x:02x}")).collect()
    }

    /// Known answers from Python `cryptography` (XTS, CTR, ECB).
    #[test]
    fn xts_matches_reference() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let mut data: [u8; SECTOR] = core::array::from_fn(|i| (i * 7) as u8);
        xts_encrypt(&key, 3, &mut data);
        assert_eq!(
            hex(&data[..32]),
            "3f375abe7d239e3250fe8d6199bbdfc1ace12713c0d965c7438b75e67947e613"
        );
        assert_eq!(
            hex(&data[SECTOR - 16..]),
            "0b00692416b0ba0c1d74e1ea96a8b026"
        );
        xts_decrypt(&key, 3, &mut data);
        assert!(data.iter().enumerate().all(|(i, &b)| b == (i * 7) as u8));
    }

    #[test]
    fn ctr_matches_reference_from_an_unaligned_offset() {
        let key: [u8; 16] = core::array::from_fn(|i| 0x10 + i as u8);
        let mut data: [u8; 40] = core::array::from_fn(|i| i as u8);
        ctr_xor(&key, 0x0102_0304_0506_0708, 0x1234, &mut data);
        assert_eq!(
            hex(&data),
            "9b36aa76d8fc36ee6d76ce95d65113ce7860d6dac107ca40dcafaad06cb56b56ecbfcf5b6e266653"
        );
        // CTR is its own inverse.
        ctr_xor(&key, 0x0102_0304_0506_0708, 0x1234, &mut data);
        assert!(data.iter().enumerate().all(|(i, &b)| b == i as u8));
    }

    #[test]
    fn ecb_matches_reference() {
        let key: [u8; 16] = core::array::from_fn(|i| 0x10 + i as u8);
        let block: [u8; 16] = core::array::from_fn(|i| i as u8);
        let ct = ecb_encrypt(&key, &block);
        assert_eq!(hex(&ct), "9c54d571702cfa0f03f36215676bab78");
        assert_eq!(ecb_decrypt(&key, &ct), block);
    }
}

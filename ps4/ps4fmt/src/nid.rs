//! PS4 symbol NIDs: the 11-character names the dynamic linker matches imports by.

use crate::sha1::sha1;

const SALT: [u8; 16] = [
    0x51, 0x8d, 0x64, 0xa6, 0x35, 0xde, 0xd8, 0xc1, 0xe6, 0xb0, 0x39, 0xb1, 0xc3, 0xe5, 0x52, 0x30,
];
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-";

/// NID of a symbol name.
///
/// # Args
/// * `name` - symbol name, e.g. `memcpy`
///
/// # Returns
/// The unpadded base64 (`/` written as `-`) of the first 8 digest bytes, reversed.
///
/// # Example
/// ```
/// assert_eq!(ps4fmt::nid::nid("memcpy"), "Q3VBxCXhUHs");
/// ```
pub fn nid(name: &str) -> String {
    let mut msg = name.as_bytes().to_vec();
    msg.extend_from_slice(&SALT);
    let mut head: [u8; 8] = sha1(&msg)[..8].try_into().unwrap();
    head.reverse();
    let mut out = String::with_capacity(11);
    for chunk in head.chunks(3) {
        let mut v = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            v |= (*b as u32) << (16 - 8 * i);
        }
        for i in 0..(chunk.len() + 1) {
            out.push(B64[((v >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pairs taken from public PS4 symbol lists (shadPS4 aerolib), as listed by iss_pc.
    #[test]
    fn known_nids() {
        for (name, want) in [
            ("_init_env", "bzQExy189ZI"),
            ("atexit", "8G2LB+A3rzg"),
            ("exit", "uMei1W9uyNo"),
            ("memcpy", "Q3VBxCXhUHs"),
            ("strlen", "j4ViWNHEgww"),
            ("memcmp", "DfivPArhucg"),
            ("pthread_mutex_lock", "7H0iTOciTLo"),
            ("__stack_chk_fail", "Ou3iL1abvng"),
        ] {
            assert_eq!(nid(name), want, "{name}");
        }
    }
}

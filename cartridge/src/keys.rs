//! `prod.keys` (Lockpick's format): `name = hex` lines. Only the header key
//! and the application key-area keys are kept.

use crate::{Error, Result};

/// Key-area key slots: `key_area_key_application_00` to `_1f`.
const SLOTS: usize = 32;

/// The console's keys that the cartridge formats need.
pub struct Keys {
    header: Option<[u8; 32]>,
    kaek: [Option<[u8; 16]>; SLOTS],
}

impl Keys {
    /// Parses the key file's text; other names are ignored.
    pub fn parse(text: &[u8]) -> Keys {
        let mut keys = Keys {
            header: None,
            kaek: [None; SLOTS],
        };
        for line in text.split(|&b| b == b'\n') {
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let name = trim(&line[..eq]);
            let value = &line[eq + 1..];
            if name == b"header_key" {
                keys.header = hex(value);
            } else if let Some(slot) = name.strip_prefix(b"key_area_key_application_") {
                if let (Some(index), Some(key)) = (hex_index(slot), hex(value)) {
                    if index < SLOTS {
                        keys.kaek[index] = Some(key);
                    }
                }
            }
        }
        keys
    }

    /// The XTS key of the NCA headers (data key, then tweak key).
    pub fn header(&self) -> Result<[u8; 32]> {
        self.header.ok_or(Error::Key)
    }

    /// The application key-area key of key generation `generation`.
    pub fn key_area_key(&self, generation: u8) -> Result<[u8; 16]> {
        self.kaek
            .get(usize::from(generation))
            .copied()
            .flatten()
            .ok_or(Error::Key)
    }
}

/// `s` without leading and trailing ASCII whitespace.
fn trim(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &s[start..end]
}

/// Exactly `N` bytes of hex digits, spaces and a CR ignored.
fn hex<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    let mut n = 0;
    let mut high = None;
    for &c in text {
        if matches!(c, b' ' | b'\t' | b'\r') {
            continue;
        }
        let v = digit(c)?;
        match high.take() {
            None => high = Some(v),
            Some(h) => {
                *out.get_mut(n)? = h << 4 | v;
                n += 1;
            }
        }
    }
    (n == N && high.is_none()).then_some(out)
}

/// The value of a two-digit hex key-slot index.
fn hex_index(s: &[u8]) -> Option<usize> {
    match s {
        [a, b] => Some(usize::from(digit(*a)? << 4 | digit(*b)?)),
        _ => None,
    }
}

/// The value of one hex digit.
fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "header_key = 000102030405060708090a0b0c0d0e0f 101112131415161718191a1b1c1d1e1f\r\n";

    #[test]
    fn parses_header_and_application_key_area_keys() {
        let text = std::format!(
            "header_key_source = 00\n{HEADER}key_area_key_application_0f = ff ff ff ff ff ff ff ff ff ff ff ff ff ff ff ff\nmaster_key_00 = 01\n"
        );
        let keys = Keys::parse(text.as_bytes());
        assert_eq!(keys.header(), Ok(core::array::from_fn(|i| i as u8)));
        assert_eq!(keys.key_area_key(0x0F), Ok([0xFF; 16]));
        assert_eq!(keys.key_area_key(0x00), Err(Error::Key));
        assert_eq!(keys.key_area_key(0x20), Err(Error::Key));
    }

    #[test]
    fn missing_or_malformed_header_key_is_a_key_error() {
        assert_eq!(Keys::parse(b"").header(), Err(Error::Key));
        let short = b"header_key = 0011\n";
        assert_eq!(Keys::parse(short).header(), Err(Error::Key));
    }
}

//! `param.sfo`: the key/value table of a PS4 title (title id, version, ...).

use crate::{span, u16le, u32le, Error};

/// One SFO value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Int(u32),
}

/// Parsed `param.sfo`, entries in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sfo {
    pub entries: Vec<(String, Value)>,
}

impl Sfo {
    /// Value of `key`, if present.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Text value of `key`, if present and textual.
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Text(s)) => Some(s),
            _ => None,
        }
    }
}

/// Largest `param.sfo` accepted; a real one is a few KiB.
pub const SFO_LIMIT: usize = 1 << 20;

/// Parse a `param.sfo`.
///
/// # Args
/// * `data` - the whole file
///
/// # Returns
/// The entries, or an error naming the first problem found.
///
/// # Raises
/// `Error::Empty`, `Error::BadMagic`, `Error::OutOfBounds`, `Error::Unsupported`.
///
/// # Example
/// ```
/// assert!(ps4fmt::sfo::parse(b"").is_err());
/// ```
pub fn parse(data: &[u8]) -> Result<Sfo, Error> {
    if data.is_empty() {
        return Err(Error::Empty);
    }
    if data.len() > SFO_LIMIT {
        return Err(Error::Unsupported("param.sfo larger than 1 MiB"));
    }
    if span(data, 0, 4)? != b"\0PSF" {
        return Err(Error::BadMagic);
    }
    let keys = u32le(data, 8)? as u64;
    let values = u32le(data, 12)? as u64;
    let count = u32le(data, 16)? as u64;
    let mut entries = Vec::new();
    for i in 0..count {
        let e = 20 + i * 16;
        let key = u16le(data, e)? as u64;
        let fmt = u16le(data, e + 2)?;
        let size = u32le(data, e + 4)? as u64;
        let off = u32le(data, e + 12)? as u64;
        let name = cstr(data, keys + key)?;
        let raw = span(data, values + off, size)?;
        let value = match fmt {
            0x0404 if raw.len() == 4 => Value::Int(u32::from_le_bytes(raw.try_into().unwrap())),
            0x0004 | 0x0204 => {
                let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
                Value::Text(String::from_utf8_lossy(&raw[..end]).into_owned())
            }
            _ => return Err(Error::Unsupported("SFO value format")),
        };
        entries.push((name, value));
    }
    Ok(Sfo { entries })
}

fn cstr(data: &[u8], offset: u64) -> Result<String, Error> {
    let tail = span(data, offset, 0).and_then(|_| {
        data.get(offset as usize..).ok_or(Error::OutOfBounds {
            offset,
            size: 1,
            len: data.len() as u64,
        })
    })?;
    let end = tail
        .iter()
        .position(|b| *b == 0)
        .ok_or(Error::Unsupported("unterminated SFO key"))?;
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a param.sfo from (key, value) pairs.
    pub(crate) fn build(items: &[(&str, Value)]) -> Vec<u8> {
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        let mut table = Vec::new();
        for (k, v) in items {
            let (fmt, bytes): (u16, Vec<u8>) = match v {
                Value::Int(n) => (0x0404, n.to_le_bytes().to_vec()),
                Value::Text(s) => (0x0204, [s.as_bytes(), &[0]].concat()),
            };
            table.extend_from_slice(&(keys.len() as u16).to_le_bytes());
            table.extend_from_slice(&fmt.to_le_bytes());
            table.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            table.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            table.extend_from_slice(&(vals.len() as u32).to_le_bytes());
            keys.extend_from_slice(k.as_bytes());
            keys.push(0);
            vals.extend_from_slice(&bytes);
        }
        let key_off = 20 + table.len() as u32;
        let val_off = key_off + keys.len() as u32;
        let mut out = b"\0PSF".to_vec();
        out.extend_from_slice(&0x101u32.to_le_bytes());
        out.extend_from_slice(&key_off.to_le_bytes());
        out.extend_from_slice(&val_off.to_le_bytes());
        out.extend_from_slice(&(items.len() as u32).to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&keys);
        out.extend_from_slice(&vals);
        out
    }

    #[test]
    fn round_trip() {
        let items = [
            ("TITLE_ID", Value::Text("CUSA00223".into())),
            ("APP_VER", Value::Text("01.07".into())),
            ("ATTRIBUTE", Value::Int(0x8000)),
        ];
        let sfo = parse(&build(&items)).unwrap();
        assert_eq!(sfo.text("TITLE_ID"), Some("CUSA00223"));
        assert_eq!(sfo.get("ATTRIBUTE"), Some(&Value::Int(0x8000)));
        assert_eq!(sfo.get("NOPE"), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse(b""), Err(Error::Empty));
        assert_eq!(parse(b"PSF\0xxxxxxxxxxxxxxxxxxxx"), Err(Error::BadMagic));
        let mut sfo = build(&[("A", Value::Int(1))]);
        sfo.truncate(sfo.len() - 2);
        assert!(matches!(parse(&sfo), Err(Error::OutOfBounds { .. })));
    }
}

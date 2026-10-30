//! PFS0 (NSP) and HFS0 (card partitions): a file table, then a string
//! table, then the files. The two differ only in the entry size.

use crate::source::{Source, Window};
use crate::{Error, Result};

const HEADER: u64 = 0x10;
/// Longest file name read (NCA names are 36 bytes).
pub const NAME_MAX: usize = 128;
/// Entries above this are taken as a corrupt header.
const COUNT_MAX: u32 = 0x1000;

/// An opened PFS0 or HFS0.
pub struct Partition {
    count: u32,
    entry_size: u64,
    strings: u64,
    strings_len: u64,
    data: u64,
}

/// One file: its name and where its bytes are, relative to the partition.
pub struct Entry {
    name: [u8; NAME_MAX],
    name_len: usize,
    offset: u64,
    size: u64,
}

impl Entry {
    /// The file name, without its NUL.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

impl Partition {
    /// Reads the header of the partition at the start of `src`.
    pub fn open<S: Source + ?Sized>(src: &S) -> Result<Partition> {
        let mut h = [0u8; 16];
        src.read_at(0, &mut h)?;
        let entry_size = match &h[..4] {
            b"PFS0" => 0x18,
            b"HFS0" => 0x40,
            _ => return Err(Error::Format),
        };
        let count = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let strings_len = u64::from(u32::from_le_bytes([h[8], h[9], h[10], h[11]]));
        if count > COUNT_MAX {
            return Err(Error::Format);
        }
        let strings = HEADER + u64::from(count) * entry_size;
        Ok(Partition {
            count,
            entry_size,
            strings,
            strings_len,
            data: strings + strings_len,
        })
    }

    /// Number of files.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// File `i`, with its name.
    pub fn entry<S: Source + ?Sized>(&self, src: &S, i: u32) -> Result<Entry> {
        if i >= self.count {
            return Err(Error::NotFound);
        }
        let mut e = [0u8; 0x14];
        src.read_at(HEADER + u64::from(i) * self.entry_size, &mut e)?;
        let offset = u64::from_le_bytes(e[..8].try_into().map_err(|_| Error::Format)?);
        let size = u64::from_le_bytes(e[8..16].try_into().map_err(|_| Error::Format)?);
        let name_at = u64::from(u32::from_le_bytes(
            e[16..20].try_into().map_err(|_| Error::Format)?,
        ));
        let avail = self.strings_len.checked_sub(name_at).ok_or(Error::Format)?;
        let mut name = [0u8; NAME_MAX];
        let want = (NAME_MAX as u64).min(avail) as usize;
        src.read_at(self.strings + name_at, &mut name[..want])?;
        let name_len = name[..want]
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::Format)?;
        Ok(Entry {
            name,
            name_len,
            offset,
            size,
        })
    }

    /// The bytes of `entry`, as a source of their own.
    pub fn file<'a, S: Source + ?Sized>(&self, src: &'a S, entry: &Entry) -> Window<'a, S> {
        Window::new(src, self.data.saturating_add(entry.offset), entry.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// A PFS0 (or HFS0 with `entry`=0x40) of `files`: name and bytes.
    pub fn build(magic: &[u8; 4], entry: usize, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut name_at = Vec::new();
        for (name, _) in files {
            name_at.push(strings.len() as u32);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        let table = 0x10 + entry * files.len();
        let mut out = Vec::new();
        out.extend_from_slice(magic);
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        let mut data = Vec::new();
        for ((_, bytes), at) in files.iter().zip(&name_at) {
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(&at.to_le_bytes());
            out.resize(out.len() + entry - 20, 0);
            data.extend_from_slice(bytes);
        }
        assert_eq!(out.len(), table);
        out.extend_from_slice(&strings);
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn lists_pfs0_entries_and_their_bytes() {
        let img = build(b"PFS0", 0x18, &[("a.nca", b"first"), ("b.tik", b"second!")]);
        let src = img.as_slice();
        let parts = Partition::open(&src).unwrap();
        assert_eq!(parts.count(), 2);
        let b = parts.entry(&src, 1).unwrap();
        assert_eq!(b.name(), b"b.tik");
        let mut buf = [0u8; 7];
        parts.file(&src, &b).read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"second!");
    }

    #[test]
    fn hfs0_uses_its_own_entry_size() {
        let img = build(b"HFS0", 0x40, &[("00000000.nca", b"card")]);
        let src = img.as_slice();
        let parts = Partition::open(&src).unwrap();
        let e = parts.entry(&src, 0).unwrap();
        assert_eq!(e.name(), b"00000000.nca");
        let mut buf = [0u8; 4];
        parts.file(&src, &e).read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"card");
    }

    #[test]
    fn rejects_other_magics_and_truncated_tables() {
        let mut img = build(b"PFS0", 0x18, &[("a", b"x")]);
        img[0] = b'X';
        assert_eq!(Partition::open(&img.as_slice()).err(), Some(Error::Format));
        let img = build(b"PFS0", 0x18, &[("a", b"x")]);
        let short = &img[..0x14];
        assert_eq!(Partition::open(&short).err(), None);
        let src = short;
        let parts = Partition::open(&src).unwrap();
        assert!(parts.entry(&src, 0).is_err());
    }
}

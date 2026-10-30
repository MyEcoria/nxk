//! RomFS: the data level of a section's IVFC tree. A header locates the
//! directory and file tables; files hang off the root directory as a
//! sibling chain.

use crate::source::Source;
use crate::{Error, Result};

const HEADER_LEN: u64 = 0x50;
const DIR_ENTRY: usize = 0x18;
const FILE_ENTRY: usize = 0x20;
/// Sibling chains longer than this are taken as a corrupt table.
const FILES_MAX: u32 = 0x10000;
const NAME_MAX: usize = 255;
const NONE: u32 = u32::MAX;

/// The tables of an opened RomFS (offsets from its first byte).
pub struct RomFs {
    dir_table: u64,
    file_table: u64,
    file_data: u64,
}

/// A file found by name: `size` bytes at `offset` in the RomFS.
pub struct File {
    pub offset: u64,
    pub size: u64,
}

impl RomFs {
    /// Reads the RomFS header at the start of `src`.
    pub fn open<S: Source + ?Sized>(src: &S) -> Result<RomFs> {
        let mut h = [0u8; HEADER_LEN as usize];
        src.read_at(0, &mut h)?;
        if u64::from_le_bytes(take(&h, 0x00)) != HEADER_LEN {
            return Err(Error::Format);
        }
        Ok(RomFs {
            dir_table: u64::from_le_bytes(take(&h, 0x18)),
            file_table: u64::from_le_bytes(take(&h, 0x38)),
            file_data: u64::from_le_bytes(take(&h, 0x48)),
        })
    }

    /// The root directory's file called `name`, if it has one.
    pub fn find<S: Source + ?Sized>(&self, src: &S, name: &[u8]) -> Result<Option<File>> {
        let mut root = [0u8; DIR_ENTRY];
        src.read_at(self.dir_table, &mut root)?;
        let mut next = u32::from_le_bytes(take(&root, 0x0C));
        for _ in 0..FILES_MAX {
            if next == NONE {
                return Ok(None);
            }
            let at = self.file_table.saturating_add(u64::from(next));
            let mut e = [0u8; FILE_ENTRY];
            src.read_at(at, &mut e)?;
            let name_len = u32::from_le_bytes(take(&e, 0x1C)) as usize;
            if name_len > NAME_MAX {
                return Err(Error::Format);
            }
            if name_len == name.len() {
                let mut buf = [0u8; NAME_MAX];
                src.read_at(at + FILE_ENTRY as u64, &mut buf[..name_len])?;
                if &buf[..name_len] == name {
                    return Ok(Some(File {
                        offset: self
                            .file_data
                            .saturating_add(u64::from_le_bytes(take(&e, 0x08))),
                        size: u64::from_le_bytes(take(&e, 0x10)),
                    }));
                }
            }
            next = u32::from_le_bytes(take(&e, 0x04));
        }
        Err(Error::Format)
    }
}

/// `N` bytes of `b` from byte `at`.
fn take<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[at..at + N]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const NONE_U32: u32 = u32::MAX;

    /// A RomFS of two root files: `first` at data offset 0, `second` after it.
    fn build(first: &[u8], second: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let file_table = 0x50 + DIR_ENTRY as u64;
        let second_at = FILE_ENTRY as u32 + 16;
        let data = 0x100u64;
        let mut u64s = [0u64; 10];
        u64s[0] = HEADER_LEN;
        u64s[3] = 0x50;
        u64s[4] = DIR_ENTRY as u64;
        u64s[7] = file_table;
        u64s[8] = u64::from(second_at) + FILE_ENTRY as u64 + 16;
        u64s[9] = data;
        for v in u64s.iter().take(10) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.resize(0x50, 0);
        // Root directory: no sub-directories, first file at file-table 0.
        let mut root = [0u8; DIR_ENTRY];
        root[4..8].copy_from_slice(&NONE_U32.to_le_bytes());
        root[0xC..0x10].copy_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&root);
        let file = |sibling: u32, offset: u64, bytes: &[u8], name: &[u8]| {
            let mut e = Vec::new();
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&sibling.to_le_bytes());
            e.extend_from_slice(&offset.to_le_bytes());
            e.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&(name.len() as u32).to_le_bytes());
            e.extend_from_slice(name);
            e.resize(e.len() + (16 - name.len() % 16) % 16, 0);
            e
        };
        out.extend_from_slice(&file(second_at, 0, first, b"control.nacp"));
        out.extend_from_slice(&file(NONE_U32, first.len() as u64, second, b"icon.dat"));
        out.resize(data as usize, 0);
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        out
    }

    #[test]
    fn finds_root_files_by_name() {
        let img = build(b"title", b"jpeg-bytes");
        let src = img.as_slice();
        let romfs = RomFs::open(&src).unwrap();
        let f = romfs.find(&src, b"control.nacp").unwrap().unwrap();
        assert_eq!((f.offset, f.size), (0x100, 5));
        let g = romfs.find(&src, b"icon.dat").unwrap().unwrap();
        assert_eq!((g.offset, g.size), (0x105, 10));
        assert!(romfs.find(&src, b"missing").unwrap().is_none());
    }

    #[test]
    fn rejects_a_wrong_header_length() {
        let mut img = build(b"a", b"b");
        img[0] = 0x51;
        assert_eq!(RomFs::open(&img.as_slice()).err(), Some(Error::Format));
    }
}

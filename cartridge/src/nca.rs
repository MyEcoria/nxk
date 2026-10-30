//! NCA headers. The first 0xC00 bytes are AES-XTS encrypted under the header
//! key. The key area holds each section's AES-CTR key wrapped by the
//! key-area key of the title's key generation.

use crate::crypto;
use crate::keys::Keys;
use crate::source::Source;
use crate::{Error, Result};

const HEADER_LEN: usize = 0xC00;
/// Content type of a Control NCA (title name, icons).
pub const CONTENT_CONTROL: u8 = 2;
const SECTIONS: usize = 4;
const FS_HEADER: usize = 0x400;
const FS_HEADER_LEN: usize = 0x200;
/// RomFS sections of a RomFS-type FS header.
const FS_ROMFS: u8 = 0;
/// Section encryption: AES-CTR.
const ENC_CTR: u8 = 3;
/// IVFC level of the RomFS data: the last of the six levels.
const IVFC_DATA_LEVEL: usize = 5;
/// Key-area slot of the CTR key (slots 0 and 1 are XTS keys).
const CTR_SLOT: usize = 2;

/// A decrypted NCA header.
pub struct Nca {
    h: [u8; HEADER_LEN],
}

/// A RomFS section: where it is in the NCA and how to decrypt it.
pub struct Section {
    start: u64,
    upper: u64,
    /// The RomFS inside the section: offset and size, from the section start.
    pub romfs: (u64, u64),
}

/// A section's bytes, decrypted with AES-CTR as they are read.
pub struct CtrSource<'a, S: Source + ?Sized> {
    src: &'a S,
    base: u64,
    key: [u8; 16],
    upper: u64,
}

impl Nca {
    /// Reads and decrypts the header at the start of `src`. Keys that do
    /// not fit the file give `Format` (the magic is wrong).
    pub fn open<S: Source + ?Sized>(keys: &Keys, src: &S) -> Result<Nca> {
        let mut h = [0u8; HEADER_LEN];
        src.read_at(0, &mut h)?;
        crypto::xts_decrypt(&keys.header()?, 0, &mut h);
        match &h[0x200..0x204] {
            b"NCA3" | b"NCA2" => Ok(Nca { h }),
            _ => Err(Error::Format),
        }
    }

    /// Content type (0 program, 1 meta, 2 control, 3 manual, 4 data).
    pub fn content_type(&self) -> u8 {
        self.h[0x205]
    }

    /// Key generation: the newer of the two fields, less one (1.0.0 is 0).
    fn generation(&self) -> u8 {
        self.h[0x206].max(self.h[0x220]).saturating_sub(1)
    }

    /// The first RomFS section.
    pub fn rom_section(&self) -> Result<Section> {
        for i in 0..SECTIONS {
            let entry = 0x240 + 0x10 * i;
            let start = u64::from(le32(&self.h, entry)) * 0x200;
            let end = u64::from(le32(&self.h, entry + 4)) * 0x200;
            let fs = FS_HEADER + FS_HEADER_LEN * i;
            if end <= start || self.h[fs + 2] != FS_ROMFS || self.h[fs + 4] != ENC_CTR {
                continue;
            }
            let level = fs + 0x18 + 0x18 * IVFC_DATA_LEVEL;
            return Ok(Section {
                start,
                upper: u64::from_be_bytes(take(&self.h, fs + 0x140)),
                romfs: (
                    u64::from_le_bytes(take(&self.h, level)),
                    u64::from_le_bytes(take(&self.h, level + 8)),
                ),
            });
        }
        Err(Error::NotFound)
    }

    /// The section's bytes, decrypted. `src` is the NCA's bytes.
    pub fn section_source<'a, S: Source + ?Sized>(
        &self,
        keys: &Keys,
        src: &'a S,
        section: &Section,
    ) -> Result<CtrSource<'a, S>> {
        let kaek = keys.key_area_key(self.generation())?;
        let wrapped: [u8; 16] = take(&self.h, 0x300 + 0x10 * CTR_SLOT);
        Ok(CtrSource {
            src,
            base: section.start,
            key: crypto::ecb_decrypt(&kaek, &wrapped),
            upper: section.upper,
        })
    }
}

impl<S: Source + ?Sized> Source for CtrSource<'_, S> {
    /// Reads the decrypted bytes at `off` of the section.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> crate::Result<()> {
        let at = self.base.checked_add(off).ok_or(Error::Format)?;
        self.src.read_at(at, buf)?;
        crypto::ctr_xor(&self.key, self.upper, at, buf);
        Ok(())
    }
}

/// `N` bytes of `b` from byte `at`.
fn take<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[at..at + N]);
    out
}

/// The little-endian `u32` at byte `at`.
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(take(b, at))
}

//! PS4 executables: plain decrypted ELF64 (x86-64) or a SELF wrapping one.
//!
//! Only already-plaintext images are read: no decryption, no decompression.

use crate::{span, u16le, u32le, u64le, Error};

/// ELF types of PS4 executables (ET_SCE_EXEC, ET_SCE_DYNEXEC, ET_SCE_DYNAMIC).
const SCE_TYPES: [u16; 3] = [0xfe00, 0xfe10, 0xfe18];
const SELF_MAGIC: [u8; 4] = [0x4f, 0x15, 0x3d, 0x1d];
/// Largest image accepted, as a guard against corrupt headers.
pub const IMAGE_LIMIT: u64 = 512 << 20;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
/// Sony-specific program header types.
pub const PT_SCE_DYNLIBDATA: u32 = 0x6100_0000;
pub const PT_SCE_COMMENT: u32 = 0x6fff_ff00;
pub const PT_SCE_LIBVERSION: u32 = 0x6fff_ff01;

/// A program header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phdr {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

impl Phdr {
    /// Segments the loader cannot do without.
    pub fn required(&self) -> bool {
        matches!(
            self.kind,
            PT_LOAD | PT_DYNAMIC | PT_SCE_DYNLIBDATA | 0x6100_0010
        )
    }
}

/// A parsed executable: the ELF bytes rebuilt in file order plus its program headers.
#[derive(Debug, Clone)]
pub struct Image {
    /// ELF type (0xfe00, 0xfe10 or 0xfe18).
    pub kind: u16,
    pub entry: u64,
    pub phdrs: Vec<Phdr>,
    /// The ELF file; bytes of segments absent from a SELF stay zero (see `missing`).
    pub elf: Vec<u8>,
    /// Indexes of non-empty program headers whose bytes the input does not contain.
    pub missing: Vec<usize>,
}

/// Parse a plain ELF or a plaintext SELF.
///
/// # Args
/// * `data` - the whole file
///
/// # Returns
/// The image, or the first problem found.
///
/// # Raises
/// `Error::Empty`, `Error::BadMagic`, `Error::OutOfBounds`, `Error::Unsupported`
/// (encrypted or compressed segment, missing required segment, odd layout).
///
/// # Example
/// ```
/// assert!(ps4fmt::elf::parse(b"").is_err());
/// ```
pub fn parse(data: &[u8]) -> Result<Image, Error> {
    if data.is_empty() {
        return Err(Error::Empty);
    }
    let magic = span(data, 0, 4)?;
    if magic == b"\x7fELF" {
        parse_elf(data)
    } else if magic == SELF_MAGIC {
        parse_self(data)
    } else {
        Err(Error::BadMagic)
    }
}

fn header(data: &[u8], base: u64) -> Result<(u16, u64, u64, u16), Error> {
    if span(data, base, 7)? != b"\x7fELF\x02\x01\x01" || u16le(data, base + 18)? != 62 {
        return Err(Error::Unsupported("not a little-endian x86-64 ELF"));
    }
    let kind = u16le(data, base + 16)?;
    let (phoff, phentsize, phnum) = (
        u64le(data, base + 32)?,
        u16le(data, base + 54)?,
        u16le(data, base + 56)?,
    );
    if phentsize != 56 || phnum == 0 || phnum >= 256 {
        return Err(Error::Unsupported("program header table"));
    }
    Ok((kind, u64le(data, base + 24)?, phoff, phnum))
}

fn phdrs(data: &[u8], at: u64, num: u16) -> Result<Vec<Phdr>, Error> {
    (0..num as u64)
        .map(|i| {
            let p = at + i * 56;
            Ok(Phdr {
                kind: u32le(data, p)?,
                flags: u32le(data, p + 4)?,
                offset: u64le(data, p + 8)?,
                vaddr: u64le(data, p + 16)?,
                filesz: u64le(data, p + 32)?,
                memsz: u64le(data, p + 40)?,
                align: u64le(data, p + 48)?,
            })
        })
        .collect()
}

fn image_end(phdrs: &[Phdr]) -> Result<u64, Error> {
    let end = phdrs
        .iter()
        .map(|p| p.offset.saturating_add(p.filesz))
        .max()
        .unwrap_or(0);
    if end > IMAGE_LIMIT {
        return Err(Error::Unsupported("image larger than 512 MiB"));
    }
    Ok(end)
}

fn parse_elf(data: &[u8]) -> Result<Image, Error> {
    let (kind, entry, phoff, num) = header(data, 0)?;
    if !SCE_TYPES.contains(&kind) {
        return Err(Error::Unsupported("ELF type is not ET_SCE_*"));
    }
    let phdrs = phdrs(data, phoff, num)?;
    let end = image_end(&phdrs)?;
    for p in phdrs.iter().filter(|p| p.filesz > 0) {
        span(data, p.offset, p.filesz)?;
    }
    Ok(Image {
        kind,
        entry,
        phdrs,
        elf: span(data, 0, end)?.to_vec(),
        missing: Vec::new(),
    })
}

fn parse_self(data: &[u8]) -> Result<Image, Error> {
    let count = u16le(data, 24)? as u64;
    let base = 32 + count * 32;
    let (kind, entry, phoff, num) = header(data, base)?;
    let phdrs = phdrs(data, base + phoff, num)?;
    let end = image_end(&phdrs)?;
    let header_end = phoff + num as u64 * 56;
    let mut elf = vec![0u8; end.max(header_end) as usize];
    elf[..header_end as usize].copy_from_slice(span(data, base, header_end)?);
    let mut covered = vec![(0, header_end)];
    for i in 0..count {
        let e = 32 + i * 32;
        let (flags, off, size, mem) = (
            u64le(data, e)?,
            u64le(data, e + 8)?,
            u64le(data, e + 16)?,
            u64le(data, e + 24)?,
        );
        if flags & 0x800 == 0 {
            continue; // not a data block of a program header
        }
        if flags & 0xa != 0 {
            return Err(Error::Unsupported("encrypted or compressed SELF segment"));
        }
        let p = phdrs
            .get(((flags >> 20) & 0xfff) as usize)
            .ok_or(Error::Unsupported("SELF segment index"))?;
        if size != p.filesz || size != mem {
            return Err(Error::Unsupported("blocked SELF segment layout"));
        }
        span(&elf, p.offset, size)?;
        elf[p.offset as usize..(p.offset + size) as usize].copy_from_slice(span(data, off, size)?);
        covered.push((p.offset, p.offset + size));
    }
    let mut missing = Vec::new();
    for (i, p) in phdrs.iter().enumerate().filter(|(_, p)| p.filesz > 0) {
        if !covered
            .iter()
            .any(|&(a, z)| a <= p.offset && p.offset + p.filesz <= z)
        {
            if p.required() {
                return Err(Error::Unsupported("required segment missing from SELF"));
            }
            missing.push(i);
        }
    }
    Ok(Image {
        kind,
        entry,
        phdrs,
        elf,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ELF with one PT_LOAD of 16 bytes at 0x100.
    fn elf() -> Vec<u8> {
        let mut e = vec![0u8; 0x110];
        e[..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x09");
        e[16..18].copy_from_slice(&0xfe10u16.to_le_bytes());
        e[18..20].copy_from_slice(&62u16.to_le_bytes());
        e[24..32].copy_from_slice(&0x400100u64.to_le_bytes());
        e[32..40].copy_from_slice(&64u64.to_le_bytes());
        e[54..56].copy_from_slice(&56u16.to_le_bytes());
        e[56..58].copy_from_slice(&1u16.to_le_bytes());
        e[64..72].copy_from_slice(&(PT_LOAD as u64 | (5 << 32)).to_le_bytes());
        for (i, v) in [0x100u64, 0x400100, 0x400100, 16, 16, 0x4000]
            .iter()
            .enumerate()
        {
            e[72 + i * 8..80 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        e[0x100..0x110].copy_from_slice(&[0xc3; 16]);
        e
    }

    #[test]
    fn plain_elf() {
        let img = parse(&elf()).unwrap();
        assert_eq!(img.kind, 0xfe10);
        assert_eq!(img.entry, 0x400100);
        assert_eq!(img.phdrs[0].kind, PT_LOAD);
        assert_eq!(img.elf.len(), 0x110);
        assert!(img.missing.is_empty());
    }

    #[test]
    fn self_wrapper() {
        let e = elf();
        let mut s = vec![0u8; 32 + 32];
        s[..4].copy_from_slice(&SELF_MAGIC);
        s[24..26].copy_from_slice(&1u16.to_le_bytes());
        // one data block for phdr 0: flags = 0x800 | index 0
        let blob_off = 32 + 32 + 0x110;
        s[32..40].copy_from_slice(&0x800u64.to_le_bytes());
        s[40..48].copy_from_slice(&(blob_off as u64).to_le_bytes());
        s[48..56].copy_from_slice(&16u64.to_le_bytes());
        s[56..64].copy_from_slice(&16u64.to_le_bytes());
        s.extend_from_slice(&e[..0x110]);
        s.extend_from_slice(&[0x90; 16]);
        let img = parse(&s).unwrap();
        assert_eq!(&img.elf[0x100..0x110], &[0x90; 16]);
        assert_eq!(&img.elf[..4], b"\x7fELF");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse(b"").unwrap_err(), Error::Empty);
        assert_eq!(parse(b"MZ\0\0").unwrap_err(), Error::BadMagic);
        let mut wrong = elf();
        wrong[16..18].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(parse(&wrong), Err(Error::Unsupported(_))));
        let mut short = elf();
        short.truncate(0x108);
        assert!(matches!(parse(&short), Err(Error::OutOfBounds { .. })));
    }

    #[test]
    fn encrypted_self_refused() {
        let mut s = vec![0u8; 64];
        s[..4].copy_from_slice(&SELF_MAGIC);
        s[24..26].copy_from_slice(&1u16.to_le_bytes());
        s[32..40].copy_from_slice(&(0x800u64 | 2).to_le_bytes());
        s.extend_from_slice(&elf()[..0x110]);
        assert!(matches!(parse(&s), Err(Error::Unsupported(_))));
    }
}

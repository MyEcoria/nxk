//! PS4 file formats read by the nxk PS4 layer (a port of the loader side of iss_pc).
//!
//! No `std` I/O: every parser takes a byte slice and checks each offset before use.

pub mod elf;
pub mod nid;
pub mod sfo;
pub mod sha1;

/// Why a PS4 file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Empty,
    BadMagic,
    OutOfBounds { offset: u64, size: u64, len: u64 },
    Unsupported(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Empty => write!(f, "empty file"),
            Error::BadMagic => write!(f, "bad signature"),
            Error::OutOfBounds { offset, size, len } => {
                write!(f, "out of bounds: {offset:#x}+{size:#x} / {len:#x}")
            }
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

/// Bounds-checked sub-slice (`offset..offset+size`).
pub(crate) fn span(data: &[u8], offset: u64, size: u64) -> Result<&[u8], Error> {
    let len = data.len() as u64;
    match offset.checked_add(size) {
        Some(end) if end <= len => Ok(&data[offset as usize..end as usize]),
        _ => Err(Error::OutOfBounds { offset, size, len }),
    }
}

pub(crate) fn u16le(data: &[u8], offset: u64) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        span(data, offset, 2)?.try_into().unwrap(),
    ))
}
pub(crate) fn u32le(data: &[u8], offset: u64) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        span(data, offset, 4)?.try_into().unwrap(),
    ))
}
pub(crate) fn u64le(data: &[u8], offset: u64) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(
        span(data, offset, 8)?.try_into().unwrap(),
    ))
}

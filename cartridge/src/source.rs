//! Byte sources: a card, a file or a slice, read at an offset.

use crate::{Error, Result};

/// Reads bytes at an absolute offset. A short or refused read is `Io`.
pub trait Source {
    /// Fills `buf` from offset `off`.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()>;
}

impl Source for &[u8] {
    /// Copies from the slice; past its end is `Io`.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(off).map_err(|_| Error::Io)?;
        let end = start.checked_add(buf.len()).ok_or(Error::Io)?;
        let src = self.get(start..end).ok_or(Error::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

/// A byte range of another source, addressed from 0.
pub struct Window<'a, S: Source + ?Sized> {
    src: &'a S,
    base: u64,
    len: u64,
}

impl<'a, S: Source + ?Sized> Window<'a, S> {
    /// `len` bytes of `src` from `base`.
    pub fn new(src: &'a S, base: u64, len: u64) -> Self {
        Window { src, base, len }
    }
}

impl<S: Source + ?Sized> Source for Window<'_, S> {
    /// Reads inside the window; past its length is `Format`.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let end = off.checked_add(buf.len() as u64).ok_or(Error::Format)?;
        if end > self.len {
            return Err(Error::Format);
        }
        let at = self.base.checked_add(off).ok_or(Error::Format)?;
        self.src.read_at(at, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_refuse_offsets_that_overflow() {
        let bytes = [0u8; 16];
        let src = bytes.as_slice();
        let far = Window::new(&src, u64::MAX - 1, 10);
        assert_eq!(far.read_at(4, &mut [0u8; 4]), Err(Error::Format));
        // Inside the window but past the end of the source itself.
        let beyond = Window::new(&src, 12, 10);
        assert_eq!(beyond.read_at(0, &mut [0u8; 8]), Err(Error::Io));
        // Past the window's own length.
        assert_eq!(beyond.read_at(4, &mut [0u8; 8]), Err(Error::Format));
    }
}

//! NACP (`control.nacp`): 16 title entries of 0x300 bytes, one per
//! language, each a name (0x200 bytes) then a publisher (0x100 bytes).

use crate::source::Source;
use crate::{Error, Result};

const ENTRY: u64 = 0x300;
/// Name field length; names are UTF-8, NUL-terminated.
pub const NAME_LEN: usize = 0x200;
/// Language entries in a NACP.
pub const LANGUAGES: usize = 16;

/// Reads the name of language `lang` (0: American English) into `out` and
/// returns its length (0: the entry is empty).
pub fn name<S: Source + ?Sized>(src: &S, lang: usize, out: &mut [u8; NAME_LEN]) -> Result<usize> {
    if lang >= LANGUAGES {
        return Err(Error::NotFound);
    }
    src.read_at(lang as u64 * ENTRY, out)?;
    Ok(out.iter().position(|&b| b == 0).unwrap_or(NAME_LEN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn nacp() -> Vec<u8> {
        let mut b = std::vec![0u8; 0x4000];
        b[..10].copy_from_slice(b"Test Game!");
        b[0x300..0x306].copy_from_slice(b"Other!");
        b
    }

    #[test]
    fn reads_the_name_of_a_language() {
        let img = nacp();
        let src = img.as_slice();
        let mut out = [0u8; NAME_LEN];
        let n = name(&src, 0, &mut out).unwrap();
        assert_eq!(&out[..n], b"Test Game!");
        let n = name(&src, 1, &mut out).unwrap();
        assert_eq!(&out[..n], b"Other!");
        assert_eq!(name(&src, 2, &mut out), Ok(0));
        assert_eq!(name(&src, LANGUAGES, &mut out), Err(Error::NotFound));
    }
}

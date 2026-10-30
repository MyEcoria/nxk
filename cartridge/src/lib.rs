//! Switch game card contents, read through a `Source`: the HFS0 partitions
//! of a card (and the PFS0 of an NSP), the NCA headers and their keys
//! (`prod.keys`), the RomFS of a Control NCA and the NACP title names.
//! Cartridge NCAs use the same format as an NSP's, so the real-data tests
//! run on an NSP. The XCI's own top level (root HFS0 offset) is not here:
//! no cartridge dump was available to confirm its layout.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod crypto;
pub mod keys;
pub mod nacp;
pub mod nca;
pub mod pfs;
pub mod romfs;
pub mod source;

pub use keys::Keys;
use source::{Source, Window};

/// Why a read failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The source refused the read.
    Io,
    /// A structure is truncated, or its magic or version is not the format's.
    Format,
    /// `prod.keys` lacks a key this data needs.
    Key,
    /// The container holds no Control NCA with that file.
    NotFound,
}

pub type Result<T> = core::result::Result<T, Error>;

/// Title name of the Control NCA in `container` (a PFS0 or a card's HFS0
/// secure partition), in NACP language `lang` (0: American English). The
/// name is written to `out`; the return value is its length.
pub fn title_name<S: Source>(
    keys: &Keys,
    container: &S,
    lang: usize,
    out: &mut [u8; nacp::NAME_LEN],
) -> Result<usize> {
    let parts = pfs::Partition::open(container)?;
    for i in 0..parts.count() {
        let entry = parts.entry(container, i)?;
        if !entry.name().ends_with(b".nca") {
            continue;
        }
        let nca_src = parts.file(container, &entry);
        let nca = match nca::Nca::open(keys, &nca_src) {
            Ok(nca) => nca,
            // Not an NCA, or made under other keys: the next file may be fine.
            Err(Error::Format) => continue,
            Err(e) => return Err(e),
        };
        if nca.content_type() != nca::CONTENT_CONTROL {
            continue;
        }
        return control_title(keys, &nca, &nca_src, lang, out);
    }
    Err(Error::NotFound)
}

/// The NACP name inside one Control NCA (`nca_src` is the NCA's bytes).
fn control_title<S: Source>(
    keys: &Keys,
    nca: &nca::Nca,
    nca_src: &S,
    lang: usize,
    out: &mut [u8; nacp::NAME_LEN],
) -> Result<usize> {
    let section = nca.rom_section()?;
    let plain = nca.section_source(keys, nca_src, &section)?;
    let romfs_src = Window::new(&plain, section.romfs.0, section.romfs.1);
    let romfs = romfs::RomFs::open(&romfs_src)?;
    let file = romfs
        .find(&romfs_src, b"control.nacp")?
        .ok_or(Error::NotFound)?;
    let nacp_src = Window::new(&romfs_src, file.offset, file.size);
    nacp::name(&nacp_src, lang, out)
}

#[cfg(test)]
mod tests;

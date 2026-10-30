//! A synthetic Control NCA in a PFS0, built with the same keys and layout
//! the real files use, read back through `title_name`. No real key or
//! dump is needed: the keys below are test values.

use crate::crypto::{ctr_xor, ecb_encrypt, xts_encrypt};
use crate::keys::Keys;
use crate::{title_name, Error};
use std::vec::Vec;

const HEADER_KEY: [u8; 32] = [0x11; 32];
const AREA_KEY: [u8; 16] = [0x22; 16];
const SECTION_KEY: [u8; 16] = [0x33; 16];
const UPPER: u64 = 0x0102_0304_0506_0708;
/// RomFS offset inside the section (IVFC data level).
const ROMFS_AT: usize = 0x1000;

fn keys_text() -> std::string::String {
    let hex =
        |b: &[u8]| -> std::string::String { b.iter().map(|x| std::format!("{x:02x}")).collect() };
    std::format!(
        "header_key = {}\nkey_area_key_application_00 = {}\n",
        hex(&HEADER_KEY),
        hex(&AREA_KEY)
    )
}

fn romfs(name: &[u8], nacp: &[u8]) -> Vec<u8> {
    let mut img = Vec::new();
    let file_at: u64 = 0x68;
    let data: u64 = 0x200;
    let meta_len: u64 = 0x20 + 12;
    let mut words = [0u64; 10];
    words[0] = 0x50;
    words[3] = 0x50;
    words[4] = 0x18;
    words[7] = file_at;
    words[8] = meta_len;
    words[9] = data;
    for w in words {
        img.extend_from_slice(&w.to_le_bytes());
    }
    img.resize(0x50, 0);
    let mut root = [0u8; 0x18];
    root[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    img.extend_from_slice(&root);
    let mut e = Vec::new();
    e.extend_from_slice(&0u32.to_le_bytes());
    e.extend_from_slice(&u32::MAX.to_le_bytes());
    e.extend_from_slice(&0u64.to_le_bytes());
    e.extend_from_slice(&(nacp.len() as u64).to_le_bytes());
    e.extend_from_slice(&0u32.to_le_bytes());
    e.extend_from_slice(&(name.len() as u32).to_le_bytes());
    e.extend_from_slice(name);
    e.resize(0x20 + 12, 0);
    img.extend_from_slice(&e);
    img.resize(data as usize, 0);
    img.extend_from_slice(nacp);
    img
}

fn nacp(name: &str) -> Vec<u8> {
    let mut b = std::vec![0u8; 0x4000];
    b[..name.len()].copy_from_slice(name.as_bytes());
    b[0x200..0x207].copy_from_slice(b"Nintend");
    b
}

/// A Control NCA: header, key area, one RomFS section at `ROMFS_AT`.
fn control_nca(romfs: &[u8]) -> Vec<u8> {
    let section_len = ROMFS_AT + romfs.len();
    let section_len = section_len.next_multiple_of(0x200);
    let mut h = std::vec![0u8; 0xC00];
    h[0x200..0x204].copy_from_slice(b"NCA3");
    h[0x205] = 2;
    h[0x221] = 0;
    h[0x220] = 1;
    let entry = 0x240;
    h[entry..entry + 4].copy_from_slice(&(0xC00u32 / 0x200).to_le_bytes());
    h[entry + 4..entry + 8].copy_from_slice(&((0xC00 + section_len) as u32 / 0x200).to_le_bytes());
    let fs = 0x400;
    h[fs + 2] = 0;
    h[fs + 4] = 3;
    h[fs + 8..fs + 12].copy_from_slice(b"IVFC");
    let level5 = fs + 0x18 + 0x18 * 5;
    h[level5..level5 + 8].copy_from_slice(&(ROMFS_AT as u64).to_le_bytes());
    h[level5 + 8..level5 + 16].copy_from_slice(&(romfs.len() as u64).to_le_bytes());
    h[fs + 0x140..fs + 0x148].copy_from_slice(&UPPER.to_be_bytes());
    let wrapped = ecb_encrypt(&AREA_KEY, &SECTION_KEY);
    h[0x320..0x330].copy_from_slice(&wrapped);
    xts_encrypt(&HEADER_KEY, 0, &mut h);

    let mut section = std::vec![0u8; section_len];
    section[ROMFS_AT..ROMFS_AT + romfs.len()].copy_from_slice(romfs);
    ctr_xor(&SECTION_KEY, UPPER, 0xC00, &mut section);
    h.extend_from_slice(&section);
    h
}

/// A PFS0 of `(name, bytes)`, as an NSP lays out its files.
fn pfs0(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut strings = Vec::new();
    let mut names = Vec::new();
    for (name, _) in files {
        names.push(strings.len() as u32);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"PFS0");
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    let mut data = Vec::new();
    for ((_, bytes), at) in files.iter().zip(&names) {
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        data.extend_from_slice(bytes);
    }
    out.extend_from_slice(&strings);
    out.extend_from_slice(&data);
    out
}

#[test]
fn reads_the_title_of_a_control_nca_in_a_pfs0() {
    let nacp = nacp("Synthetic Title");
    let nca = control_nca(&romfs(b"control.nacp", &nacp));
    let container = pfs0(&[("ticket.tik", b"ignored".to_vec()), ("0a1b.nca", nca)]);
    let keys = Keys::parse(keys_text().as_bytes());
    let mut out = [0u8; crate::nacp::NAME_LEN];
    let n = title_name(&keys, &container.as_slice(), 0, &mut out).unwrap();
    assert_eq!(&out[..n], b"Synthetic Title");
}

#[test]
fn a_file_under_other_keys_is_skipped_as_not_nca() {
    let nca = control_nca(&romfs(b"control.nacp", &nacp("X")));
    let container = pfs0(&[("0a1b.nca", nca)]);
    let mut other = keys_text();
    other = other.replacen("header_key = 1111", "header_key = 2222", 1);
    let keys = Keys::parse(other.as_bytes());
    let mut out = [0u8; crate::nacp::NAME_LEN];
    assert_eq!(
        title_name(&keys, &container.as_slice(), 0, &mut out),
        Err(Error::NotFound)
    );
}

#[test]
fn a_container_without_keys_is_a_key_error() {
    let nca = control_nca(&romfs(b"control.nacp", &nacp("X")));
    let container = pfs0(&[("0a1b.nca", nca)]);
    let keys = Keys::parse(b"");
    let mut out = [0u8; crate::nacp::NAME_LEN];
    assert_eq!(
        title_name(&keys, &container.as_slice(), 0, &mut out),
        Err(Error::Key)
    );
}

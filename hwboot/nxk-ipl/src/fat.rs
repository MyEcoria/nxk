//! Read-only FAT32 for stage 1: the first partition of the MBR (or a
//! volume without one), files found by their 8.3 names, read whole
//! straight from the card into their place in memory.

use crate::sd::SdCard;

/// A sector buffer in DRAM (the card's DMA writes it).
const BUF: u32 = 0x8F01_0000;
const SECTOR: u32 = 512;

pub struct Volume {
    /// First sector of the FAT and of the data area (cluster 2).
    fat: u32,
    data: u32,
    per_cluster: u32,
    root: u32,
}

/// A file found: its first cluster and size.
pub struct File {
    pub cluster: u32,
    pub size: u32,
}

fn sector(card: &SdCard, lba: u32) -> Option<&'static [u8; 512]> {
    if let Err(e) = card.transfer(lba as u64, 1, true, BUF as u64) {
        crate::println!("[nxk-ipl] SD: sector {} not read: {}", lba, nxk_sdcard::err_name(e));
        return None;
    }
    Some(unsafe { &*(BUF as *const [u8; 512]) })
}

fn le16(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Volume {
    pub fn open(card: &SdCard) -> Option<Self> {
        let Some(mbr) = sector(card, 0) else {
            crate::println!("[nxk-ipl] SD: sector 0 not read");
            return None;
        };
        let (sig, kind) = (le16(mbr, 510), mbr[0x1C2]);
        let mut start = 0;
        if sig == 0xAA55 && matches!(kind, 0x0B | 0x0C) {
            start = le32(mbr, 0x1C6);
        }
        let Some(bpb) = sector(card, start) else {
            crate::println!("[nxk-ipl] SD: sector {} (volume start) not read", start);
            return None;
        };
        if le16(bpb, 11) != SECTOR || le16(bpb, 0x16) != 0 {
            crate::println!(
                "[nxk-ipl] SD: no FAT32 volume (MBR {:#x}, first partition type {:#x} at {}, sector size {})",
                sig,
                kind,
                le32(mbr, 0x1C6),
                le16(bpb, 11)
            );
            return None;
        }
        let per_cluster = bpb[13] as u32;
        let reserved = le16(bpb, 14);
        let fats = bpb[16] as u32;
        let fat_size = le32(bpb, 0x24);
        let root = le32(bpb, 0x2C);
        let fat = start + reserved;
        crate::println!(
            "[nxk-ipl] FAT32 at {}: {} sectors a cluster, FAT at {}, data at {}, root cluster {}",
            start,
            per_cluster,
            fat,
            fat + fats * fat_size,
            root
        );
        Some(Volume {
            fat,
            data: fat + fats * fat_size,
            per_cluster,
            root,
        })
    }

    fn cluster_lba(&self, c: u32) -> u32 {
        self.data + (c - 2) * self.per_cluster
    }

    fn next(&self, card: &SdCard, c: u32) -> Option<u32> {
        let s = sector(card, self.fat + c * 4 / SECTOR)?;
        let n = le32(s, (c * 4 % SECTOR) as usize) & 0x0FFF_FFFF;
        (2..0x0FFF_FFF8).contains(&n).then_some(n)
    }

    /// `name` in the directory at `dir`: its 8.3 name, or its long name
    /// (Linux writes a lowercase "bl31.bin" as "BL31~1.BIN" plus a long name).
    fn find(&self, card: &SdCard, dir: u32, name: &str) -> Option<File> {
        let short = short_name(name);
        let mut long = Long::new();
        let mut c = dir;
        let mut seen = 0;
        loop {
            for k in 0..self.per_cluster {
                let s = sector(card, self.cluster_lba(c) + k)?;
                for e in s.chunks_exact(32) {
                    if e[0] == 0 {
                        crate::println!("[nxk-ipl] FAT: {} entries in cluster {}, no {}", seen, dir, name);
                        return None;
                    }
                    seen += 1;
                    if e[0] == 0xE5 {
                        long.reset();
                        continue;
                    }
                    if e[11] == 0x0F {
                        long.add(e);
                        continue;
                    }
                    let matches = short.is_some_and(|n| e[..11].eq_ignore_ascii_case(&n))
                        || long.is(name, checksum(&e[..11]));
                    long.reset();
                    if e[11] & 0x08 != 0 || !matches {
                        continue; // volume label, or another name
                    }
                    let cluster = le16(e, 20) << 16 | le16(e, 26);
                    return Some(File {
                        cluster,
                        size: le32(e, 28),
                    });
                }
            }
            c = match self.next(card, c) {
                Some(n) => n,
                None => {
                    crate::println!("[nxk-ipl] FAT: end of directory {} after {} entries, no {}", dir, seen, name);
                    return None;
                }
            };
        }
    }

    /// A path such as "nxk/bl31.bin".
    pub fn lookup(&self, card: &SdCard, path: &str) -> Option<File> {
        let mut dir = self.root;
        let mut found = None;
        for part in path.split('/') {
            if found.is_some() {
                dir = found.take().map(|f: File| f.cluster)?;
            }
            found = Some(self.find(card, dir, part)?);
        }
        found
    }

    /// The whole file to `dst` (4-byte aligned), cluster by cluster: the
    /// last cluster's tail lands past the file's end. Returns the size.
    pub fn read(&self, card: &SdCard, f: &File, dst: u32) -> Option<u32> {
        let bytes = self.per_cluster * SECTOR;
        let mut c = f.cluster;
        let mut off = 0;
        while off < f.size {
            card.transfer(self.cluster_lba(c) as u64, self.per_cluster as usize, true, (dst + off) as u64)
                .ok()?;
            off += bytes;
            if off < f.size {
                c = self.next(card, c)?;
            }
        }
        Some(f.size)
    }
}

/// "bl31.bin" -> "BL31    BIN".
fn short_name(s: &str) -> Option<[u8; 11]> {
    let mut out = [b' '; 11];
    let (base, ext) = s.split_once('.').unwrap_or((s, ""));
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    for (o, b) in out.iter_mut().zip(base.bytes()) {
        *o = b.to_ascii_uppercase();
    }
    for (o, b) in out[8..].iter_mut().zip(ext.bytes()) {
        *o = b.to_ascii_uppercase();
    }
    Some(out)
}

/// The 8.3 name's checksum a long name's entries carry.
fn checksum(short: &[u8]) -> u8 {
    short.iter().fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

/// A long name being gathered from its entries (they precede the 8.3 one,
/// last part first). ASCII only: other characters never match.
struct Long {
    chars: [u16; 260],
    len: usize,
    sum: u8,
    valid: bool,
}

impl Long {
    fn new() -> Self {
        Long {
            chars: [0; 260],
            len: 0,
            sum: 0,
            valid: false,
        }
    }

    fn reset(&mut self) {
        self.valid = false;
    }

    fn add(&mut self, e: &[u8]) {
        let seq = (e[0] & 0x1F) as usize;
        if seq == 0 || seq > 20 {
            self.valid = false;
            return;
        }
        if e[0] & 0x40 != 0 {
            (self.valid, self.len, self.sum) = (true, seq * 13, e[13]);
        } else if !self.valid || e[13] != self.sum {
            self.valid = false;
            return;
        }
        const AT: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (i, &o) in AT.iter().enumerate() {
            self.chars[(seq - 1) * 13 + i] = u16::from_le_bytes([e[o], e[o + 1]]);
        }
    }

    fn is(&self, name: &str, sum: u8) -> bool {
        if !self.valid || sum != self.sum {
            return false;
        }
        let chars = &self.chars[..self.len];
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        end == name.len()
            && chars[..end]
                .iter()
                .zip(name.bytes())
                .all(|(&c, b)| c < 0x80 && (c as u8).eq_ignore_ascii_case(&b))
    }
}

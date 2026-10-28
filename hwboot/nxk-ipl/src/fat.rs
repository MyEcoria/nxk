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
    card.transfer(lba as u64, 1, true, BUF as u64).ok()?;
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
        let mbr = sector(card, 0)?;
        let mut start = 0;
        if mbr[510] == 0x55 && mbr[511] == 0xAA && matches!(mbr[0x1C2], 0x0B | 0x0C) {
            start = le32(mbr, 0x1C6);
        }
        let bpb = sector(card, start)?;
        if le16(bpb, 11) != SECTOR || le16(bpb, 0x16) != 0 {
            return None; // not 512-byte sectors, or not FAT32
        }
        let per_cluster = bpb[13] as u32;
        let reserved = le16(bpb, 14);
        let fats = bpb[16] as u32;
        let fat_size = le32(bpb, 0x24);
        let root = le32(bpb, 0x2C);
        let fat = start + reserved;
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

    /// `name` (8.3, "NAME    EXT" form) in the directory at `dir`.
    fn find(&self, card: &SdCard, dir: u32, name: &[u8; 11]) -> Option<File> {
        let mut c = dir;
        loop {
            for k in 0..self.per_cluster {
                let s = sector(card, self.cluster_lba(c) + k)?;
                for e in s.chunks_exact(32) {
                    if e[0] == 0 {
                        return None;
                    }
                    if e[0] == 0xE5 || e[11] == 0x0F || e[11] & 0x08 != 0 {
                        continue; // free, long name part, volume label
                    }
                    if e[..11].eq_ignore_ascii_case(name) {
                        let cluster = le16(e, 20) << 16 | le16(e, 26);
                        return Some(File {
                            cluster,
                            size: le32(e, 28),
                        });
                    }
                }
            }
            c = self.next(card, c)?;
        }
    }

    /// A path such as "nxk/bl31.bin" (8.3 components only).
    pub fn lookup(&self, card: &SdCard, path: &str) -> Option<File> {
        let mut dir = self.root;
        let mut found = None;
        for part in path.split('/') {
            if found.is_some() {
                dir = found.take().map(|f: File| f.cluster)?;
            }
            found = Some(self.find(card, dir, &short_name(part)?)?);
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

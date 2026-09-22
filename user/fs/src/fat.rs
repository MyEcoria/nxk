//! FAT32: volume, allocation table, directories (long names), files.

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::{
    EBUSY, EEXIST, EINVAL, EIO, EISDIR, ENAMETOOLONG, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY,
};

use crate::cache::{Cache, SECTOR};

pub const ATTR_DIR: u8 = 0x10;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;
const EOC: u32 = 0x0FFF_FFFF;
const BAD: u32 = 0x0FFF_FFF7;
const ENTRY: usize = 32;

pub type Res<T> = Result<T, i64>;

/// A directory entry, and where it is stored.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    /// Modification time (seconds since 1970).
    pub mtime: u64,
    /// Parent directory (first cluster; 0 for the root itself).
    pub dir: u32,
    /// Index of the short entry in the parent, and LFN entries before it.
    pub index: u32,
    pub lfn: u32,
    /// Stored 8.3 name.
    pub short: [u8; 11],
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIR != 0
    }
}

/// Position in a file chain: avoids rewalking FAT from the start on
/// each sequential read.
#[derive(Clone, Copy, Default)]
pub struct Cursor {
    pub index: u32,
    pub cluster: u32,
}

pub struct Volume {
    pub cache: Cache,
    spc: u32,
    fat_start: u64,
    fat_size: u32,
    nfats: u32,
    data_start: u64,
    root: u32,
    clusters: u32,
    fsinfo: u64,
    next_free: u32,
    free: Option<u32>,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// ------------------------------------------------------------- dates ---

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (time, date) FAT pair for the current instant.
fn fat_now() -> (u16, u16) {
    let s = (libnx::sys::realtime_ns() / 1_000_000_000) as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    let y = (y - 1980).clamp(0, 127) as u16;
    let time = ((t / 3600) as u16) << 11 | (((t % 3600) / 60) as u16) << 5 | ((t % 60) / 2) as u16;
    (time, y << 9 | (m as u16) << 5 | d as u16)
}

fn unix_time(time: u16, date: u16) -> u64 {
    if date == 0 {
        return 0;
    }
    let y = 1980 + (date >> 9) as i64;
    let (m, d) = (
        ((date >> 5) & 0xF).clamp(1, 12) as u32,
        (date & 0x1F).max(1) as u32,
    );
    let s =
        (time >> 11) as i64 * 3600 + ((time >> 5) & 0x3F) as i64 * 60 + (time & 0x1F) as i64 * 2;
    (days_from_civil(y, m, d) * 86_400 + s).max(0) as u64
}

// ------------------------------------------------------------- noms ---

fn eq_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut s = 0u8;
    for &c in short {
        s = ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(c);
    }
    s
}

fn short_valid(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || b"$%'-_@~`!(){}^#&".contains(&c) || c >= 0x80
}

/// Exact 8.3 name if `name` fits as-is (uppercase, no LFN).
fn exact_short(name: &str) -> Option<[u8; 11]> {
    let (base, ext) = match name.rfind('.') {
        Some(p) if p > 0 => (&name[..p], &name[p + 1..]),
        _ => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || !name.is_ascii() {
        return None;
    }
    if !base.bytes().chain(ext.bytes()).all(short_valid) {
        return None;
    }
    let mut s = [b' '; 11];
    s[..base.len()].copy_from_slice(base.as_bytes());
    s[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    Some(s)
}

/// Substitute "BASE~N.EXT" 8.3 name for a long name.
fn alias(name: &str, n: u32) -> [u8; 11] {
    let clean = |s: &str, max: usize| -> Vec<u8> {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let c = c.to_ascii_uppercase();
                if c.is_ascii() && short_valid(c as u8) {
                    c as u8
                } else {
                    b'_'
                }
            })
            .take(max)
            .collect()
    };
    let (base, ext) = match name.rfind('.') {
        Some(p) if p > 0 => (&name[..p], &name[p + 1..]),
        _ => (name, ""),
    };
    let tail = alloc::format!("~{}", n);
    let mut b = clean(base, 8 - tail.len());
    if b.is_empty() {
        b.push(b'_');
    }
    b.extend_from_slice(tail.as_bytes());
    let e = clean(ext, 3);
    let mut s = [b' '; 11];
    s[..b.len()].copy_from_slice(&b);
    s[8..8 + e.len()].copy_from_slice(&e);
    s
}

/// Readable name of a short entry (lowercase per NT flags).
fn short_to_string(e: &[u8]) -> String {
    let lower_base = e[12] & 0x08 != 0;
    let lower_ext = e[12] & 0x10 != 0;
    let mut s = String::new();
    for (k, &c) in e[..8].iter().enumerate() {
        if c == b' ' {
            break;
        }
        let c = if k == 0 && c == 0x05 { 0xE5 } else { c };
        s.push(if lower_base {
            (c as char).to_ascii_lowercase()
        } else {
            c as char
        });
    }
    if e[8] != b' ' {
        s.push('.');
        for &c in &e[8..11] {
            if c == b' ' {
                break;
            }
            s.push(if lower_ext {
                (c as char).to_ascii_lowercase()
            } else {
                c as char
            });
        }
    }
    s
}

fn valid_name(name: &str) -> Res<()> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(-EINVAL);
    }
    if name.encode_utf16().count() > 255 {
        return Err(-ENAMETOOLONG);
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
    {
        return Err(-EINVAL);
    }
    Ok(())
}

// ------------------------------------------------------------- volume ---

impl Volume {
    /// Mount the FAT32 volume (MBR table or partition-less volume).
    pub fn mount(mut cache: Cache) -> Res<Self> {
        let s0 = cache.read(0)?.to_vec();
        let mut part = 0u64;
        let is_vbr = |b: &[u8]| {
            (b[0] == 0xEB || b[0] == 0xE9) && le16(b, 11) == 512 && b[510] == 0x55 && b[511] == 0xAA
        };
        if !is_vbr(&s0) {
            if s0[510] != 0x55 || s0[511] != 0xAA {
                return Err(-EINVAL);
            }
            // First FAT partition of the MBR table.
            let found = (0..4)
                .map(|k| &s0[446 + 16 * k..462 + 16 * k])
                .find(|p| matches!(p[4], 0x0B | 0x0C | 0x0E | 0x06));
            part = le32(found.ok_or(-EINVAL)?, 8) as u64;
        }
        let b = cache.read(part)?.to_vec();
        if !is_vbr(&b) || le16(&b, 17) != 0 || le32(&b, 36) == 0 {
            return Err(-EINVAL); // not FAT32
        }
        let spc = b[13] as u32;
        let reserved = le16(&b, 14) as u64;
        let nfats = b[16] as u32;
        let total = if le16(&b, 19) != 0 {
            le16(&b, 19) as u64
        } else {
            le32(&b, 32) as u64
        };
        let fat_size = le32(&b, 36);
        let root = le32(&b, 44);
        let fsinfo = le16(&b, 48) as u64;
        let data_start = part + reserved + nfats as u64 * fat_size as u64;
        let clusters = ((total - (data_start - part)) / spc as u64) as u32;
        if spc == 0 || !spc.is_power_of_two() || clusters < 16 {
            return Err(-EINVAL);
        }
        let mut v = Self {
            cache,
            spc,
            fat_start: part + reserved,
            fat_size,
            nfats,
            data_start,
            root,
            clusters,
            fsinfo: if fsinfo != 0 && fsinfo != 0xFFFF {
                part + fsinfo
            } else {
                0
            },
            next_free: 2,
            free: None,
        };
        if v.fsinfo != 0 {
            let f = v.cache.read(v.fsinfo)?;
            if le32(f, 0) == 0x4161_5252 && le32(f, 484) == 0x6141_7272 {
                let (fc, nf) = (le32(f, 488), le32(f, 492));
                if fc <= v.clusters {
                    v.free = Some(fc);
                }
                if (2..v.clusters + 2).contains(&nf) {
                    v.next_free = nf;
                }
            }
        }
        Ok(v)
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.spc as u64 * SECTOR as u64
    }

    fn lba(&self, c: u32) -> u64 {
        self.data_start + (c as u64 - 2) * self.spc as u64
    }

    fn check(&self, c: u32) -> Res<()> {
        if c < 2 || c >= self.clusters + 2 {
            return Err(-EIO);
        }
        Ok(())
    }

    pub fn fat_get(&mut self, c: u32) -> Res<u32> {
        self.check(c)?;
        let off = c as u64 * 4;
        let s = self.cache.read(self.fat_start + off / SECTOR as u64)?;
        Ok(le32(s, (off % SECTOR as u64) as usize) & 0x0FFF_FFFF)
    }

    fn fat_set(&mut self, c: u32, v: u32) -> Res<()> {
        self.check(c)?;
        let off = c as u64 * 4;
        for k in 0..self.nfats as u64 {
            let s = self
                .cache
                .write(self.fat_start + k * self.fat_size as u64 + off / SECTOR as u64)?;
            let o = (off % SECTOR as u64) as usize;
            let old = le32(s, o);
            s[o..o + 4].copy_from_slice(&((old & 0xF000_0000) | (v & 0x0FFF_FFFF)).to_le_bytes());
        }
        Ok(())
    }

    fn is_end(v: u32) -> bool {
        v >= 0x0FFF_FFF8 || v < 2
    }

    /// New cluster, chained after `prev` (0: first of a chain).
    pub fn alloc_cluster(&mut self, prev: u32, zero: bool) -> Res<u32> {
        let n = self.clusters;
        for k in 0..n {
            let c = 2 + (self.next_free - 2 + k) % n;
            if self.fat_get(c)? == 0 {
                self.fat_set(c, EOC)?;
                if prev != 0 {
                    self.fat_set(prev, c)?;
                }
                self.next_free = if c + 1 < n + 2 { c + 1 } else { 2 };
                if let Some(f) = &mut self.free {
                    *f = f.saturating_sub(1);
                }
                if zero {
                    let lba = self.lba(c);
                    for s in 0..self.spc as u64 {
                        self.cache.overwrite(lba + s)?.fill(0);
                    }
                }
                return Ok(c);
            }
        }
        Err(-ENOSPC)
    }

    pub fn free_chain(&mut self, mut c: u32) -> Res<()> {
        let mut guard = 0;
        while !Self::is_end(c) && c != BAD && guard <= self.clusters {
            let next = self.fat_get(c)?;
            self.fat_set(c, 0)?;
            if let Some(f) = &mut self.free {
                *f += 1;
            }
            c = next;
            guard += 1;
        }
        Ok(())
    }

    /// Cluster number `n` of the chain starting at `start`.
    fn nth(&mut self, start: u32, n: u32, cur: &mut Cursor) -> Res<Option<u32>> {
        let (mut idx, mut c) = if cur.cluster != 0 && cur.index <= n {
            (cur.index, cur.cluster)
        } else {
            (0, start)
        };
        while idx < n {
            if Self::is_end(c) {
                return Ok(None);
            }
            c = self.fat_get(c)?;
            idx += 1;
        }
        if Self::is_end(c) {
            return Ok(None);
        }
        *cur = Cursor {
            index: n,
            cluster: c,
        };
        Ok(Some(c))
    }

    /// Free and total bytes (counts FAT once if FSInfo ignores it).
    pub fn statfs(&mut self) -> Res<(u64, u64)> {
        if self.free.is_none() {
            let mut n = 0;
            for c in 2..self.clusters + 2 {
                if self.fat_get(c)? == 0 {
                    n += 1;
                }
            }
            self.free = Some(n);
        }
        let cb = self.cluster_bytes();
        Ok((self.free.unwrap() as u64 * cb, self.clusters as u64 * cb))
    }

    pub fn sync(&mut self) -> Res<()> {
        if self.fsinfo != 0 {
            let (free, next) = (self.free.unwrap_or(u32::MAX), self.next_free);
            let f = self.cache.write(self.fsinfo)?;
            if le32(f, 0) == 0x4161_5252 {
                f[488..492].copy_from_slice(&free.to_le_bytes());
                f[492..496].copy_from_slice(&next.to_le_bytes());
            }
        }
        self.cache.flush()
    }

    // ------------------------------------------------------- directories ---

    fn root_entry(&self) -> Entry {
        Entry {
            name: String::from("/"),
            attr: ATTR_DIR,
            cluster: self.root,
            size: 0,
            mtime: 0,
            dir: 0,
            index: 0,
            lfn: 0,
            short: [b' '; 11],
        }
    }

    /// Sector and offset of entry `index` in a directory; `extend`
    /// appends (zeroed) clusters if the directory is too short.
    fn slot(&mut self, dir: u32, index: u32, extend: bool) -> Res<Option<(u64, usize)>> {
        let per = (self.cluster_bytes() / ENTRY as u64) as u32;
        let (n, within) = (index / per, index % per);
        let mut c = dir;
        for _ in 0..n {
            let next = self.fat_get(c)?;
            c = if Self::is_end(next) {
                if !extend {
                    return Ok(None);
                }
                self.alloc_cluster(c, true)?
            } else {
                next
            };
        }
        let byte = within as u64 * ENTRY as u64;
        Ok(Some((
            self.lba(c) + byte / SECTOR as u64,
            (byte % SECTOR as u64) as usize,
        )))
    }

    /// Walk a directory; `f` returns false to stop.
    pub fn list(&mut self, dir: u32, mut f: impl FnMut(&Entry) -> bool) -> Res<()> {
        let mut lfn: Vec<u16> = Vec::new();
        let (mut lfn_sum, mut lfn_count, mut lfn_expect) = (0u8, 0u32, 0u32);
        let mut index = 0u32;
        while let Some((lba, off)) = self.slot(dir, index, false)? {
            let e: [u8; ENTRY] = self.cache.read(lba)?[off..off + ENTRY].try_into().unwrap();
            index += 1;
            match e[0] {
                0x00 => break,
                0xE5 => {
                    lfn_count = 0;
                    continue;
                }
                _ => {}
            }
            if e[11] == ATTR_LFN {
                let ord = (e[0] & 0x1F) as u32;
                if e[0] & 0x40 != 0 {
                    lfn = alloc::vec![0xFFFF; ord as usize * 13];
                    lfn_sum = e[13];
                    lfn_count = 1;
                    lfn_expect = ord;
                } else if lfn_count > 0 && e[13] == lfn_sum {
                    lfn_count += 1;
                } else {
                    lfn_count = 0;
                    continue;
                }
                if ord == 0 || ord as usize * 13 > lfn.len() {
                    lfn_count = 0;
                    continue;
                }
                let base = (ord as usize - 1) * 13;
                let chars = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (k, &o) in chars.iter().enumerate() {
                    lfn[base + k] = le16(&e, o);
                }
                continue;
            }
            if e[11] & ATTR_VOLUME != 0 && e[11] & ATTR_DIR == 0 {
                lfn_count = 0;
                continue;
            }
            let short: [u8; 11] = e[..11].try_into().unwrap();
            let name =
                if lfn_count > 0 && lfn_count == lfn_expect && lfn_checksum(&short) == lfn_sum {
                    let end = lfn
                        .iter()
                        .position(|&c| c == 0 || c == 0xFFFF)
                        .unwrap_or(lfn.len());
                    String::from_utf16_lossy(&lfn[..end])
                } else {
                    short_to_string(&e)
                };
            let used_lfn = if lfn_count > 0 && lfn_count == lfn_expect {
                lfn_count
            } else {
                0
            };
            lfn_count = 0;
            let entry = Entry {
                name,
                attr: e[11],
                cluster: (le16(&e, 20) as u32) << 16 | le16(&e, 26) as u32,
                size: le32(&e, 28),
                mtime: unix_time(le16(&e, 22), le16(&e, 24)),
                dir,
                index: index - 1,
                lfn: used_lfn,
                short,
            };
            if !f(&entry) {
                break;
            }
        }
        Ok(())
    }

    pub fn find(&mut self, dir: u32, name: &str) -> Res<Option<Entry>> {
        let mut found = None;
        self.list(dir, |e| {
            if eq_name(&e.name, name) {
                found = Some(e.clone());
                false
            } else {
                true
            }
        })?;
        Ok(found)
    }

    /// Resolve an absolute path.
    pub fn lookup(&mut self, path: &str) -> Res<Entry> {
        let mut cur = self.root_entry();
        for comp in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            if !cur.is_dir() {
                return Err(-ENOTDIR);
            }
            let mut e = self.find(cur.cluster, comp)?.ok_or(-ENOENT)?;
            if comp == ".." && e.cluster == 0 {
                e = self.root_entry(); // ".." of a root subdir
            }
            cur = e;
        }
        Ok(cur)
    }

    /// Parent directory of a path and last component.
    pub fn parent<'a>(&mut self, path: &'a str) -> Res<(Entry, &'a str)> {
        let path = path.trim_end_matches('/');
        let (dir, name) = match path.rfind('/') {
            Some(p) => (&path[..p], &path[p + 1..]),
            None => ("", path),
        };
        let d = self.lookup(dir)?;
        if !d.is_dir() {
            return Err(-ENOTDIR);
        }
        Ok((d, name))
    }

    /// Create entry `name` in `parent` (empty file or directory).
    pub fn create(
        &mut self,
        parent: &Entry,
        name: &str,
        attr: u8,
        cluster: u32,
        size: u32,
    ) -> Res<Entry> {
        valid_name(name)?;
        if self.find(parent.cluster, name)?.is_some() {
            return Err(-EEXIST);
        }
        let (short, lfn_needed) = match exact_short(name) {
            Some(s) => (s, false),
            None => {
                let mut n = 1;
                loop {
                    let a = alias(name, n);
                    let taken = {
                        let mut t = false;
                        self.list(parent.cluster, |e| {
                            t |= e.short == a;
                            !t
                        })?;
                        t
                    };
                    if !taken {
                        break (a, true);
                    }
                    n += 1;
                    if n > 999_999 {
                        return Err(-EEXIST);
                    }
                }
            }
        };
        let units: Vec<u16> = name.encode_utf16().collect();
        let nlfn = if lfn_needed {
            units.len().div_ceil(13) as u32
        } else {
            0
        };
        let need = nlfn + 1;

        // First run of `need` free entries (or past the end).
        let (mut start, mut run, mut index) = (0u32, 0u32, 0u32);
        loop {
            let (lba, off) = self.slot(parent.cluster, index, true)?.ok_or(-EIO)?;
            let b = self.cache.read(lba)?[off];
            if b == 0x00 || b == 0xE5 {
                if run == 0 {
                    start = index;
                }
                run += 1;
                if run == need {
                    break;
                }
            } else {
                run = 0;
            }
            index += 1;
            if index > 65_535 {
                return Err(-ENOSPC);
            }
        }
        let sum = lfn_checksum(&short);
        for k in 0..nlfn {
            let ord = nlfn - k; // pieces stored last-to-first
            let mut e = [0u8; ENTRY];
            e[0] = ord as u8 | if k == 0 { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            let chars = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
            for (j, &o) in chars.iter().enumerate() {
                let p = (ord as usize - 1) * 13 + j;
                let c = match p.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[p],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xFFFF,
                };
                e[o..o + 2].copy_from_slice(&c.to_le_bytes());
            }
            let (lba, off) = self.slot(parent.cluster, start + k, true)?.ok_or(-EIO)?;
            self.cache.write(lba)?[off..off + ENTRY].copy_from_slice(&e);
        }
        let (time, date) = fat_now();
        let mut e = [0u8; ENTRY];
        e[..11].copy_from_slice(&short);
        e[11] = attr;
        e[14..16].copy_from_slice(&time.to_le_bytes());
        e[16..18].copy_from_slice(&date.to_le_bytes());
        e[18..20].copy_from_slice(&date.to_le_bytes());
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&time.to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        let (lba, off) = self.slot(parent.cluster, start + nlfn, true)?.ok_or(-EIO)?;
        self.cache.write(lba)?[off..off + ENTRY].copy_from_slice(&e);
        Ok(Entry {
            name: String::from(name),
            attr,
            cluster,
            size,
            mtime: unix_time(time, date),
            dir: parent.cluster,
            index: start + nlfn,
            lfn: nlfn,
            short,
        })
    }

    /// Rewrite size, first cluster and modification date.
    pub fn update(&mut self, e: &Entry) -> Res<()> {
        if e.dir == 0 {
            return Ok(()); // root: no entry
        }
        let (time, date) = fat_now();
        let (lba, off) = self.slot(e.dir, e.index, false)?.ok_or(-EIO)?;
        let s = &mut self.cache.write(lba)?[off..off + ENTRY];
        s[20..22].copy_from_slice(&((e.cluster >> 16) as u16).to_le_bytes());
        s[22..24].copy_from_slice(&time.to_le_bytes());
        s[24..26].copy_from_slice(&date.to_le_bytes());
        s[18..20].copy_from_slice(&date.to_le_bytes());
        s[26..28].copy_from_slice(&(e.cluster as u16).to_le_bytes());
        s[28..32].copy_from_slice(&e.size.to_le_bytes());
        Ok(())
    }

    /// Clear the entry (and its LFN pieces) without freeing clusters.
    fn remove_entry(&mut self, e: &Entry) -> Res<()> {
        for k in e.index - e.lfn..=e.index {
            let (lba, off) = self.slot(e.dir, k, false)?.ok_or(-EIO)?;
            self.cache.write(lba)?[off] = 0xE5;
        }
        Ok(())
    }

    pub fn mkdir(&mut self, path: &str) -> Res<()> {
        let (parent, name) = self.parent(path)?;
        let c = self.alloc_cluster(0, true)?;
        let e = match self.create(&parent, name, ATTR_DIR, c, 0) {
            Ok(e) => e,
            Err(err) => {
                self.free_chain(c)?;
                return Err(err);
            }
        };
        let (time, date) = fat_now();
        let lba = self.lba(c);
        let s = self.cache.write(lba)?;
        for (k, (nm, cl)) in [
            (*b".          ", c),
            (
                *b"..         ",
                if parent.dir == 0 { 0 } else { parent.cluster },
            ),
        ]
        .iter()
        .enumerate()
        {
            let o = k * ENTRY;
            s[o..o + 11].copy_from_slice(nm);
            s[o + 11] = ATTR_DIR;
            s[o + 14..o + 16].copy_from_slice(&time.to_le_bytes());
            s[o + 16..o + 18].copy_from_slice(&date.to_le_bytes());
            s[o + 20..o + 22].copy_from_slice(&((cl >> 16) as u16).to_le_bytes());
            s[o + 22..o + 24].copy_from_slice(&time.to_le_bytes());
            s[o + 24..o + 26].copy_from_slice(&date.to_le_bytes());
            s[o + 26..o + 28].copy_from_slice(&(*cl as u16).to_le_bytes());
        }
        let _ = e;
        Ok(())
    }

    pub fn unlink(&mut self, path: &str, open: impl Fn(&Entry) -> bool) -> Res<()> {
        let e = self.lookup(path)?;
        if e.is_dir() {
            return Err(-EISDIR);
        }
        if open(&e) {
            return Err(-EBUSY);
        }
        self.remove_entry(&e)?;
        self.free_chain(e.cluster)
    }

    pub fn rmdir(&mut self, path: &str) -> Res<()> {
        let e = self.lookup(path)?;
        if !e.is_dir() {
            return Err(-ENOTDIR);
        }
        if e.dir == 0 {
            return Err(-EBUSY); // the root
        }
        let mut empty = true;
        self.list(e.cluster, |x| {
            empty = x.name == "." || x.name == "..";
            empty
        })?;
        if !empty {
            return Err(-ENOTEMPTY);
        }
        self.remove_entry(&e)?;
        self.free_chain(e.cluster)
    }

    pub fn rename(&mut self, from: &str, to: &str, open: impl Fn(&Entry) -> bool) -> Res<()> {
        let e = self.lookup(from)?;
        if e.dir == 0 {
            return Err(-EBUSY);
        }
        let (parent, name) = self.parent(to)?;
        if let Some(old) = self.find(parent.cluster, name)? {
            if old.dir == e.dir && old.index == e.index {
                return Ok(()); // same entry
            }
            if old.is_dir() || e.is_dir() {
                return Err(-EEXIST);
            }
            if open(&old) {
                return Err(-EBUSY);
            }
            self.remove_entry(&old)?;
            self.free_chain(old.cluster)?;
        }
        let moved = self.create(&parent, name, e.attr, e.cluster, e.size)?;
        // The old entry may have moved within the same directory:
        // look it up by exact position.
        self.remove_entry(&e)?;
        if e.is_dir() && e.dir != parent.cluster {
            let lba = self.lba(e.cluster);
            let dotdot = if parent.dir == 0 { 0 } else { parent.cluster };
            let s = self.cache.write(lba)?;
            s[ENTRY + 20..ENTRY + 22].copy_from_slice(&((dotdot >> 16) as u16).to_le_bytes());
            s[ENTRY + 26..ENTRY + 28].copy_from_slice(&(dotdot as u16).to_le_bytes());
        }
        let _ = moved;
        Ok(())
    }

    // -------------------------------------------------------- files ---

    pub fn read(&mut self, e: &Entry, cur: &mut Cursor, pos: u64, out: &mut [u8]) -> Res<usize> {
        if pos >= e.size as u64 {
            return Ok(0);
        }
        let n = out.len().min((e.size as u64 - pos) as usize);
        let cb = self.cluster_bytes();
        let mut done = 0;
        while done < n {
            let p = pos + done as u64;
            let c = self.nth(e.cluster, (p / cb) as u32, cur)?.ok_or(-EIO)?;
            let in_c = p % cb;
            let lba = self.lba(c) + in_c / SECTOR as u64;
            let o = (in_c % SECTOR as u64) as usize;
            let k = (SECTOR - o).min(n - done);
            out[done..done + k].copy_from_slice(&self.cache.read(lba)?[o..o + k]);
            done += k;
        }
        Ok(n)
    }

    /// Write at `pos`, extending the file (zeros between old end and
    /// `pos`). Updates the directory entry.
    pub fn write(&mut self, e: &mut Entry, cur: &mut Cursor, pos: u64, data: &[u8]) -> Res<usize> {
        if e.is_dir() {
            return Err(-EISDIR);
        }
        let end = pos
            .checked_add(data.len() as u64)
            .filter(|&x| x <= u32::MAX as u64)
            .ok_or(-EINVAL)?;
        if pos > e.size as u64 {
            let gap = alloc::vec![0u8; (pos - e.size as u64).min(1 << 20) as usize];
            let mut p = e.size as u64;
            while p < pos {
                let k = (pos - p).min(gap.len() as u64) as usize;
                self.write(e, cur, p, &gap[..k])?;
                p += k as u64;
            }
        }
        let cb = self.cluster_bytes();
        self.ensure_clusters(e, cur, end.div_ceil(cb) as u32)?;
        let mut done = 0;
        while done < data.len() {
            let p = pos + done as u64;
            let c = self.nth(e.cluster, (p / cb) as u32, cur)?.ok_or(-EIO)?;
            let in_c = p % cb;
            let lba = self.lba(c) + in_c / SECTOR as u64;
            let o = (in_c % SECTOR as u64) as usize;
            let k = (SECTOR - o).min(data.len() - done);
            let s = if k == SECTOR {
                self.cache.overwrite(lba)?
            } else {
                self.cache.write(lba)?
            };
            s[o..o + k].copy_from_slice(&data[done..done + k]);
            done += k;
        }
        if end > e.size as u64 {
            e.size = end as u32;
        }
        self.update(e)?;
        Ok(data.len())
    }

    /// At least `n` clusters in the file chain.
    fn ensure_clusters(&mut self, e: &mut Entry, cur: &mut Cursor, n: u32) -> Res<()> {
        if n == 0 {
            return Ok(());
        }
        if e.cluster == 0 {
            e.cluster = self.alloc_cluster(0, false)?;
            *cur = Cursor::default();
        }
        let mut last;
        let mut have = 1;
        let mut c = e.cluster;
        loop {
            let next = self.fat_get(c)?;
            if Self::is_end(next) {
                last = c;
                break;
            }
            c = next;
            have += 1;
            if have >= n {
                return Ok(());
            }
        }
        while have < n {
            last = self.alloc_cluster(last, false)?;
            have += 1;
        }
        Ok(())
    }

    pub fn truncate(&mut self, e: &mut Entry, cur: &mut Cursor, size: u64) -> Res<()> {
        if size > e.size as u64 {
            // Extend = write zeros up to the new size.
            let zeros = alloc::vec![0u8; 4096];
            let mut p = e.size as u64;
            while p < size {
                let k = (size - p).min(4096) as usize;
                self.write(e, cur, p, &zeros[..k])?;
                p += k as u64;
            }
            return Ok(());
        }
        let cb = self.cluster_bytes();
        let keep = size.div_ceil(cb) as u32;
        if keep == 0 {
            if e.cluster != 0 {
                self.free_chain(e.cluster)?;
                e.cluster = 0;
            }
        } else if let Some(last) = self.nth(e.cluster, keep - 1, cur)? {
            let rest = self.fat_get(last)?;
            if !Self::is_end(rest) {
                self.fat_set(last, EOC)?;
                self.free_chain(rest)?;
            }
        }
        *cur = Cursor::default();
        e.size = size as u32;
        self.update(e)
    }

    pub fn default_attr() -> u8 {
        ATTR_ARCHIVE
    }
}

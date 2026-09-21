//! Cache de secteurs du volume, en ecriture differee.
//!
//! Tout acces au support passe par ici. Un defaut de cache lit d'un coup
//! plusieurs secteurs consecutifs (lecture anticipee : les fichiers et les
//! dossiers FAT sont le plus souvent contigus). Les secteurs modifies ne sont
//! ecrits qu'au `flush` (fermeture d'un fichier, `sync`) ou a leur eviction,
//! regroupes en ecritures multi-blocs.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use libnx::abi::proto::blk;
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;

pub const SECTOR: usize = 512;
/// Secteurs lus d'avance sur un defaut de cache.
const READAHEAD: usize = 64;
/// Taille du tampon partage avec le pilote (256 KiB = 512 blocs).
pub const IO_SECTORS: usize = 512;

pub struct Cache {
    blk: u32,
    io: SharedMemory,
    total: u64,
    data: Vec<u8>,
    tags: Vec<u64>,
    dirty: Vec<bool>,
    used: Vec<bool>,
    map: BTreeMap<u64, usize>,
    hand: usize,
}

const EMPTY: u64 = u64::MAX;

impl Cache {
    pub fn new(blk: u32, io: SharedMemory, total: u64, slots: usize) -> Self {
        Self {
            blk,
            io,
            total,
            data: vec![0; slots * SECTOR],
            tags: vec![EMPTY; slots],
            dirty: vec![false; slots],
            used: vec![false; slots],
            map: BTreeMap::new(),
            hand: 0,
        }
    }

    fn io(&self, op: u32, lba: u64, n: usize) -> Result<(), i64> {
        let mut m = Msg::new(op, 3, 0);
        m.mr[0] = lba;
        m.mr[1] = n as u64;
        m.mr[2] = 0;
        let r = ipc::call(self.blk, &m)?;
        if r.label() != 0 {
            return Err(-(r.label() as i64));
        }
        Ok(())
    }

    /// Emplacement libre (algorithme de l'horloge), apres ecriture eventuelle.
    fn victim(&mut self) -> Result<usize, i64> {
        loop {
            let s = self.hand;
            self.hand = (self.hand + 1) % self.tags.len();
            if self.used[s] {
                self.used[s] = false;
                continue;
            }
            if self.dirty[s] {
                self.flush()?;
            }
            if self.tags[s] != EMPTY {
                self.map.remove(&self.tags[s]);
                self.tags[s] = EMPTY;
            }
            return Ok(s);
        }
    }

    fn slot(&mut self, lba: u64) -> Result<usize, i64> {
        if let Some(&s) = self.map.get(&lba) {
            self.used[s] = true;
            return Ok(s);
        }
        if lba >= self.total {
            return Err(-libnx::abi::err::EIO);
        }
        // Lecture anticipee jusqu'au prochain secteur deja en cache.
        let mut n = 1;
        while n < READAHEAD && lba + (n as u64) < self.total && !self.map.contains_key(&(lba + n as u64)) {
            n += 1;
        }
        self.io(blk::READ, lba, n)?;
        let mut first = 0;
        for k in 0..n {
            let s = self.victim()?;
            let src = &self.io.as_slice()[k * SECTOR..(k + 1) * SECTOR];
            self.data[s * SECTOR..(s + 1) * SECTOR].copy_from_slice(src);
            self.tags[s] = lba + k as u64;
            self.map.insert(lba + k as u64, s);
            self.used[s] = k == 0;
            if k == 0 {
                first = s;
            }
        }
        Ok(first)
    }

    pub fn read(&mut self, lba: u64) -> Result<&[u8], i64> {
        let s = self.slot(lba)?;
        Ok(&self.data[s * SECTOR..(s + 1) * SECTOR])
    }

    pub fn write(&mut self, lba: u64) -> Result<&mut [u8], i64> {
        let s = self.slot(lba)?;
        self.dirty[s] = true;
        Ok(&mut self.data[s * SECTOR..(s + 1) * SECTOR])
    }

    /// Secteur entierement reecrit : inutile de le lire d'abord.
    pub fn overwrite(&mut self, lba: u64) -> Result<&mut [u8], i64> {
        let s = match self.map.get(&lba) {
            Some(&s) => s,
            None => {
                let s = self.victim()?;
                self.tags[s] = lba;
                self.map.insert(lba, s);
                s
            }
        };
        self.used[s] = true;
        self.dirty[s] = true;
        Ok(&mut self.data[s * SECTOR..(s + 1) * SECTOR])
    }

    /// Ecrit tous les secteurs modifies, par suites contigues.
    pub fn flush(&mut self) -> Result<(), i64> {
        let dirty: Vec<(u64, usize)> =
            self.map.iter().filter(|(_, &s)| self.dirty[s]).map(|(&l, &s)| (l, s)).collect();
        let mut i = 0;
        while i < dirty.len() {
            let start = dirty[i].0;
            let mut n = 0;
            while i + n < dirty.len() && dirty[i + n].0 == start + n as u64 && n < IO_SECTORS {
                let s = dirty[i + n].1;
                self.io.as_mut_slice()[n * SECTOR..(n + 1) * SECTOR].copy_from_slice(&self.data[s * SECTOR..(s + 1) * SECTOR]);
                n += 1;
            }
            self.io(blk::WRITE, start, n)?;
            for k in 0..n {
                self.dirty[dirty[i + k].1] = false;
            }
            i += n;
        }
        Ok(())
    }
}

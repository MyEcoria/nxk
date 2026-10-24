//! Volume sector cache, write-back, with readahead on misses.
//! Dirty sectors flush on `flush`/eviction in contiguous runs.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use libnx::abi::proto::blk;
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;

use crate::clock::Clock;

pub const SECTOR: usize = 512;
/// Sectors read ahead on a cache miss.
const READAHEAD: usize = 64;
/// Size of the buffer shared with the driver (256 KiB = 512 blocks).
pub const IO_SECTORS: usize = 512;

pub struct Cache {
    blk: u32,
    io: SharedMemory,
    total: u64,
    data: Vec<u8>,
    tags: Vec<u64>,
    dirty: Vec<bool>,
    clock: Clock,
    map: BTreeMap<u64, usize>,
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
            clock: Clock::new(slots),
            map: BTreeMap::new(),
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

    /// Free slot (clock algorithm), other than `taken`, flushing if needed.
    fn victim(&mut self, taken: &[usize]) -> Result<usize, i64> {
        let s = self.clock.next(taken);
        if self.dirty[s] {
            self.flush()?;
        }
        if self.tags[s] != EMPTY {
            self.map.remove(&self.tags[s]);
            self.tags[s] = EMPTY;
        }
        Ok(s)
    }

    fn slot(&mut self, lba: u64) -> Result<usize, i64> {
        if let Some(&s) = self.map.get(&lba) {
            self.clock.set_used(s, true);
            return Ok(s);
        }
        if lba >= self.total {
            return Err(-libnx::abi::err::EIO);
        }
        // Readahead up to the next already-cached sector.
        let mut n = 1;
        while n < READAHEAD
            && lba + (n as u64) < self.total
            && !self.map.contains_key(&(lba + n as u64))
        {
            n += 1;
        }
        // Free the slots first: a flush in `victim` reuses the I/O buffer.
        let mut slots = Vec::with_capacity(n);
        for _ in 0..n {
            slots.push(self.victim(&slots)?);
        }
        self.io(blk::READ, lba, n)?;
        for (k, &s) in slots.iter().enumerate() {
            let src = &self.io.as_slice()[k * SECTOR..(k + 1) * SECTOR];
            self.data[s * SECTOR..(s + 1) * SECTOR].copy_from_slice(src);
            self.tags[s] = lba + k as u64;
            self.map.insert(lba + k as u64, s);
            self.clock.set_used(s, k == 0);
        }
        let first = slots[0];
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

    /// Fully overwritten sector: no need to read it first.
    pub fn overwrite(&mut self, lba: u64) -> Result<&mut [u8], i64> {
        let s = match self.map.get(&lba) {
            Some(&s) => s,
            None => {
                let s = self.victim(&[])?;
                self.tags[s] = lba;
                self.map.insert(lba, s);
                s
            }
        };
        self.clock.set_used(s, true);
        self.dirty[s] = true;
        Ok(&mut self.data[s * SECTOR..(s + 1) * SECTOR])
    }

    /// Write all dirty sectors, in contiguous runs.
    pub fn flush(&mut self) -> Result<(), i64> {
        let dirty: Vec<(u64, usize)> = self
            .map
            .iter()
            .filter(|(_, &s)| self.dirty[s])
            .map(|(&l, &s)| (l, s))
            .collect();
        let mut i = 0;
        while i < dirty.len() {
            let start = dirty[i].0;
            let mut n = 0;
            while i + n < dirty.len() && dirty[i + n].0 == start + n as u64 && n < IO_SECTORS {
                let s = dirty[i + n].1;
                self.io.as_mut_slice()[n * SECTOR..(n + 1) * SECTOR]
                    .copy_from_slice(&self.data[s * SECTOR..(s + 1) * SECTOR]);
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

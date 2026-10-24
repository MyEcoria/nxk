//! Second-chance (clock) choice of the cache slot to reuse. No hardware or
//! IPC in here: `rustc --edition 2021 --test user/fs/src/clock.rs` runs
//! its tests on the PC.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

pub struct Clock {
    used: Vec<bool>,
    hand: usize,
}

impl Clock {
    pub fn new(slots: usize) -> Self {
        Self {
            used: vec![false; slots],
            hand: 0,
        }
    }

    pub fn set_used(&mut self, slot: usize, used: bool) {
        self.used[slot] = used;
    }

    /// Next slot to reuse, skipping those in `taken`: a batch picks its
    /// slots before filling any, and a scan that wraps (every other slot
    /// used) would return a slot already picked for the batch.
    pub fn next(&mut self, taken: &[usize]) -> usize {
        debug_assert!(taken.len() < self.used.len());
        loop {
            let s = self.hand;
            self.hand = (self.hand + 1) % self.used.len();
            if taken.contains(&s) {
                continue;
            }
            if self.used[s] {
                self.used[s] = false;
                continue;
            }
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const SLOTS: usize = 2048;
    const READAHEAD: usize = 64;

    /// The cache's slot bookkeeping over a clock, holding a sector number
    /// per slot to see which data a lookup gets.
    struct Model {
        clock: Clock,
        tag: Vec<Option<u64>>,
        data: Vec<u64>,
        map: BTreeMap<u64, usize>,
    }

    impl Model {
        fn new() -> Self {
            Self {
                clock: Clock::new(SLOTS),
                tag: vec![None; SLOTS],
                data: vec![u64::MAX; SLOTS],
                map: BTreeMap::new(),
            }
        }

        fn read(&mut self, lba: u64) -> u64 {
            if let Some(&s) = self.map.get(&lba) {
                self.clock.set_used(s, true);
                return self.data[s];
            }
            let mut n = 1;
            while n < READAHEAD && !self.map.contains_key(&(lba + n as u64)) {
                n += 1;
            }
            let mut slots = Vec::new();
            for _ in 0..n {
                let s = self.clock.next(&slots);
                if let Some(t) = self.tag[s].take() {
                    self.map.remove(&t);
                }
                slots.push(s);
            }
            for (k, &s) in slots.iter().enumerate() {
                self.data[s] = lba + k as u64;
                self.tag[s] = Some(lba + k as u64);
                self.map.insert(lba + k as u64, s);
                self.clock.set_used(s, k == 0);
            }
            self.data[slots[0]]
        }
    }

    /// A file read sector by sector with a FAT sector read per cluster,
    /// again and again: every lookup must give its own sector.
    #[test]
    fn sequential_file_with_fat_reads() {
        let mut m = Model::new();
        for pass in 0..4 {
            for i in 0..34_000u64 {
                let lba = 8192 + i;
                if i % 64 == 0 {
                    let fat = 100 + i / 64 / 128;
                    assert_eq!(m.read(fat), fat, "FAT sector, pass {pass}");
                }
                assert_eq!(m.read(lba), lba, "pass {pass}, sector {i}");
            }
        }
    }

    #[test]
    fn a_batch_never_gets_a_slot_twice() {
        // All slots used but the one the hand stands on.
        let mut c = Clock::new(SLOTS);
        for s in 1..SLOTS {
            c.set_used(s, true);
        }
        let mut taken = Vec::new();
        for _ in 0..READAHEAD {
            let s = c.next(&taken);
            assert!(!taken.contains(&s), "slot {s} twice");
            taken.push(s);
        }
    }
}

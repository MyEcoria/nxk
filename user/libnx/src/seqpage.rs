//! A page of u64 words one process writes and others map read-only,
//! read without asking the writer (a service's stats). Word 0 counts
//! changes: odd while one is under way, a read retries until it saw the
//! same even value on both sides.

use core::sync::atomic::{fence, AtomicU64, Ordering};

use nxk_abi::layout::PAGE_SIZE;
use nxk_abi::prot;
use nxk_abi::rights::{GRANT, MAP};

use crate::mem::SharedMemory;
use crate::sys;

/// Attempts of a read against a writer that keeps changing the page.
const TRIES: usize = 100;

pub struct SeqPage {
    mem: SharedMemory,
}

impl SeqPage {
    /// A new page, all zero, mapped writable.
    pub fn new() -> Result<Self, i64> {
        Ok(Self {
            mem: SharedMemory::new(PAGE_SIZE as usize, 0)?,
        })
    }

    /// A received page (`handle`), mapped read-only.
    pub fn map(handle: u32) -> Result<Self, i64> {
        Ok(Self {
            mem: SharedMemory::map(handle, PAGE_SIZE as usize, prot::READ)?,
        })
    }

    /// A handle to give a reader: map (read-only) and pass on.
    pub fn share(&self) -> Result<u32, i64> {
        sys::handle_dup(self.mem.handle(), MAP | GRANT, 0)
    }

    fn word(&self, i: usize) -> &AtomicU64 {
        assert!(i < PAGE_SIZE as usize / 8);
        unsafe { &*(self.mem.as_ptr() as *const AtomicU64).add(i) }
    }

    /// Words (index > 0, value) changed as one, by the only writer.
    pub fn write(&self, words: &[(usize, u64)]) {
        let seq = self.word(0);
        let s = seq.load(Ordering::Relaxed);
        seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        for &(i, v) in words {
            self.word(i).store(v, Ordering::Relaxed);
        }
        seq.store(s + 2, Ordering::Release);
    }

    /// The first `N` words between two changes; none if the writer
    /// kept changing them (or died halfway through a change).
    pub fn read<const N: usize>(&self) -> Option<[u64; N]> {
        let seq = self.word(0);
        for _ in 0..TRIES {
            let s = seq.load(Ordering::Acquire);
            if s & 1 == 0 {
                let w = core::array::from_fn(|i| self.word(i).load(Ordering::Relaxed));
                fence(Ordering::Acquire);
                if seq.load(Ordering::Relaxed) == s {
                    return Some(w);
                }
            }
            core::hint::spin_loop();
        }
        None
    }
}

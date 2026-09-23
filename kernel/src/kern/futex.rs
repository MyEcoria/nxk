//! Futex on user words, keyed by physical address; hashed FIFO waits.

use nxk_abi::err::{EAGAIN, EFAULT, EINVAL, ETIMEDOUT};

use crate::kern::bkl::BklCell;
use crate::kern::sched::{self, Timeout};
use crate::kern::thread::{self, Queue, State};
use crate::mm::uaccess;

const BUCKETS: usize = 256;

static TABLE: BklCell<[Queue; BUCKETS]> = BklCell::new([const { Queue::new() }; BUCKETS]);

#[inline]
fn bucket(pa: u64) -> usize {
    ((pa >> 2).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize
}

fn key(uaddr: u64) -> Result<u64, i64> {
    if uaddr % 4 != 0 {
        return Err(-EINVAL);
    }
    uaccess::translate_populate(uaddr, false).ok_or(-EFAULT)
}

/// Park `ti` if the word still equals `val`. `Ok(None)`: parked.
pub fn wait(ti: u16, uaddr: u64, val: u32, tmo: Timeout) -> Result<Option<u64>, i64> {
    let pa = key(uaddr)?;
    // Aligned 32-bit read: atomic against EL0 writes.
    let cur = unsafe { core::ptr::read_volatile(pa as *const u32) };
    if cur != val {
        return Err(-EAGAIN);
    }
    if tmo == Timeout::Now {
        return Err(-ETIMEDOUT);
    }
    let b = bucket(pa);
    let t = thread::get(ti);
    t.state = State::BlockedFutex;
    t.futex_key = pa;
    t.blocked_on = b as u16;
    TABLE.get()[b].push(ti);
    tmo.arm(ti);
    Ok(None)
}

/// Wake up to `n` waiters on the word; returns their count.
pub fn wake(uaddr: u64, n: u64) -> Result<u64, i64> {
    let pa = key(uaddr)?;
    let q = &mut TABLE.get()[bucket(pa)];
    let mut woken = 0;
    while woken < n {
        match q.take(|x| thread::get(x).futex_key == pa) {
            Some(w) => {
                sched::wake(w, 0);
                woken += 1;
            }
            None => break,
        }
    }
    Ok(woken)
}

pub fn remove_waiter(b: u16, ti: u16) {
    if (b as usize) < BUCKETS {
        TABLE.get()[b as usize].remove(ti);
    }
}

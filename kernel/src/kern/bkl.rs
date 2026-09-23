//! Global kernel lock (seL4 SMP style): held across kernel entries.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use nxk_abi::err::ESRCH;

use crate::arch;
use crate::kern::percpu;
use crate::kern::thread::{self, State, NIL};
use crate::sync::SpinLock;

static LOCK: SpinLock<()> = SpinLock::new(());
static OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);

#[inline]
pub fn acquire() {
    LOCK.raw_lock();
    OWNER.store(arch::core_id(), Ordering::Relaxed);
}

#[inline]
pub fn release() {
    OWNER.store(usize::MAX, Ordering::Relaxed);
    LOCK.raw_unlock();
}

#[inline]
pub fn held() -> bool {
    OWNER.load(Ordering::Relaxed) == arch::core_id()
}

/// Pages of long work (zeroing, cache maintenance) between two
/// `breathe` calls: about 50 us.
pub const BREATHE_PAGES: u64 = 64;

/// Long work lets the other cores in: the lock is dropped and retaken
/// (the work's pages must be private to the caller meanwhile).
/// `Err(-ESRCH)`: the calling thread was killed and must stop.
pub fn breathe() -> Result<(), i64> {
    release();
    acquire();
    let cur = percpu::this().current;
    if cur != NIL && thread::get(cur).state == State::Zombie {
        return Err(-ESRCH);
    }
    Ok(())
}

/// Data guarded by the global lock: only valid to touch while held.
pub struct BklCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for BklCell<T> {}

impl<T> BklCell<T> {
    pub const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }

    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        debug_assert!(held(), "kernel state touched without the global lock");
        unsafe { &mut *self.0.get() }
    }
}

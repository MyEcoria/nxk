//! Global kernel lock (seL4 SMP style): held across kernel entries.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch;
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

//! Fair starvation-free ticket lock; `wfe`/`sev` while waiting.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;

#[repr(C, align(64))]
pub struct SpinLock<T> {
    next: AtomicU32,
    owner: AtomicU32,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            owner: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.owner.load(Ordering::Acquire) != ticket {
            arch::wfe();
        }
        SpinLockGuard { lock: self }
    }

    /// Raw take, for locks released on another path than the taker (kernel
    /// global lock).
    #[inline]
    pub fn raw_lock(&self) {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.owner.load(Ordering::Acquire) != ticket {
            arch::wfe();
        }
    }

    #[inline]
    pub fn raw_unlock(&self) {
        self.owner.fetch_add(1, Ordering::Release);
        arch::sev();
    }

    /// Lock-free access, when no concurrent access is possible (single-core
    /// boot, MMU off: see `console`).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_unlocked(&self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }

    /// Lock with interrupts masked: required once an interrupt handler may
    /// take the same lock.
    #[inline]
    pub fn lock_irqsafe(&self) -> IrqGuard<'_, T> {
        let flags = arch::irq_save();
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.owner.load(Ordering::Acquire) != ticket {
            arch::wfe();
        }
        IrqGuard { lock: self, flags }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.owner.fetch_add(1, Ordering::Release);
        arch::sev();
    }
}

/// Unlock-then-restore-IRQ guard: order avoids handler deadlock.
pub struct IrqGuard<'a, T> {
    lock: &'a SpinLock<T>,
    flags: u64,
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.owner.fetch_add(1, Ordering::Release);
        arch::sev();
        arch::irq_restore(self.flags);
    }
}

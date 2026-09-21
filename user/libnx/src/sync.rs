//! Synchronisation en espace utilisateur.
//!
//! `Mutex` et `Condvar` reposent sur le futex du noyau : sans contention, tout
//! se fait par operations atomiques, sans appel systeme ; un thread qui doit
//! attendre dort dans le noyau au lieu de tourner. `SpinLock` reste pour les
//! sections tres courtes (il cede le coeur si le detenteur tarde).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::sys;

// ------------------------------------------------------------ SpinLock ---

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        let mut spins = 0u32;
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins > 64 {
                sys::yield_now();
                spins = 0;
            } else {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

// --------------------------------------------------------------- Mutex ---

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
/// Verrouille, et au moins un thread dort peut-etre dans le noyau.
const CONTENDED: u32 = 2;

/// Verrou a trois etats (Drepper, « Futexes are tricky ») : le deverrouillage
/// n'appelle le noyau que si quelqu'un attend.
pub struct RawMutex {
    state: AtomicU32,
}

impl RawMutex {
    pub const fn new() -> Self {
        Self { state: AtomicU32::new(UNLOCKED) }
    }

    #[inline]
    pub fn try_lock(&self) -> bool {
        self.state.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    #[inline]
    pub fn lock(&self) {
        if !self.try_lock() {
            self.lock_contended();
        }
    }

    #[cold]
    fn lock_contended(&self) {
        // Courte attente active : le detenteur tourne peut-etre sur un autre
        // coeur et va rendre le verrou tout de suite.
        for _ in 0..100 {
            if self.state.load(Ordering::Relaxed) == UNLOCKED && self.try_lock() {
                return;
            }
            core::hint::spin_loop();
        }
        while self.state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
            let _ = sys::futex_wait(&self.state, CONTENDED, 0);
        }
    }

    #[inline]
    pub fn unlock(&self) {
        if self.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            sys::futex_wake(&self.state, 1);
        }
    }
}

impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Mutex<T> {
    raw: RawMutex,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(v: T) -> Self {
        Self { raw: RawMutex::new(), data: UnsafeCell::new(v) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.raw.lock();
        MutexGuard { m: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.raw.try_lock().then_some(MutexGuard { m: self })
    }
}

pub struct MutexGuard<'a, T> {
    m: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.m.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.m.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.m.raw.unlock();
    }
}

// ------------------------------------------------------------- Condvar ---

/// Variable de condition : un compteur d'evenements sur lequel on attend.
pub struct Condvar {
    seq: AtomicU32,
}

impl Condvar {
    pub const fn new() -> Self {
        Self { seq: AtomicU32::new(0) }
    }

    /// Rend le verrou, attend un signal (ou `timeout_ns`, 0 = sans limite),
    /// puis reprend le verrou. Vrai si le delai a expire.
    pub fn wait_timeout<'a, T>(&self, g: MutexGuard<'a, T>, timeout_ns: u64) -> (MutexGuard<'a, T>, bool) {
        let seq = self.seq.load(Ordering::Relaxed);
        let m = g.m;
        drop(g);
        let r = sys::futex_wait(&self.seq, seq, timeout_ns);
        (m.lock(), r == Err(-nxk_abi::err::ETIMEDOUT))
    }

    pub fn wait<'a, T>(&self, g: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait_timeout(g, 0).0
    }

    pub fn notify_one(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        sys::futex_wake(&self.seq, 1);
    }

    pub fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        sys::futex_wake(&self.seq, u64::MAX);
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

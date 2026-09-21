//! Verrou a ticket : equitable et sans famine, contrairement a un simple
//! `compare_exchange` en boucle. `wfe`/`sev` evitent de bruler du courant et
//! du temps CPU pendant l'attente, ce qui compte aussi en emulation TCG.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;

#[repr(C, align(64))] // une ligne de cache A57 : pas de faux partage
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

    /// Prise sans garde, pour les verrous relaches sur un autre chemin
    /// d'execution que celui qui les a pris (verrou global du noyau).
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

    /// Verrouille en masquant les interruptions : indispensable des qu'un
    /// gestionnaire d'interruption peut prendre le meme verrou.
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

/// Garde combinee : relache le verrou **puis** restaure les interruptions.
/// L'ordre importe : reactiver les IRQ avant de rendre le verrou ouvrirait une
/// fenetre d'interblocage si le gestionnaire prend le meme verrou.
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

//! Verrou global du noyau.
//!
//! Tout l'etat du noyau (objets, files, tables de pages) est protege par un
//! verrou unique, pris a chaque entree dans le noyau et rendu juste avant le
//! retour en EL0. Le noyau n'est jamais interrompu (IRQ masquees en EL1) et ses
//! chemins durent quelques microsecondes : pour un micro-noyau, un verrou global
//! se montre aussi performant qu'un verrouillage fin, sans risque d'inversion
//! d'ordre entre verrous. C'est le choix de seL4 en multiprocesseur.

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

/// Donnee protegee par le verrou global : l'acces n'est valide que verrou pris.
pub struct BklCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for BklCell<T> {}

impl<T> BklCell<T> {
    pub const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }

    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        debug_assert!(held(), "etat noyau touche sans le verrou global");
        unsafe { &mut *self.0.get() }
    }
}

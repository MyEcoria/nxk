//! Futex du noyau (cles par adresse physique) : base de `Mutex`, `Condvar`,
//! `RwLock`, `Once` et du parking des threads.

use super::abi::err::ETIMEDOUT;
use super::syscall;
use crate::sync::atomic::Atomic;
use crate::time::Duration;

/// An atomic for use as a futex that is at least 32-bits but may be larger
pub type Futex = Atomic<Primitive>;
/// Must be the underlying type of Futex
pub type Primitive = u32;

/// An atomic for use as a futex that is at least 8-bits but may be larger.
pub type SmallFutex = Atomic<SmallPrimitive>;
/// Must be the underlying type of SmallFutex
pub type SmallPrimitive = u32;

/// Attend un `futex_wake`, sauf si le futex ne vaut deja plus `expected`.
///
/// Rend false si le delai a expire, true dans tous les autres cas.
pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let r = syscall::futex_wait(futex, expected, super::timeout_ns(timeout));
    r != -ETIMEDOUT
}

/// Reveille un thread en attente ; rend true si un thread a ete reveille.
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    syscall::futex_wake(futex, 1) > 0
}

/// Reveille tous les threads en attente.
pub fn futex_wake_all(futex: &Atomic<u32>) {
    syscall::futex_wake(futex, u32::MAX as u64);
}

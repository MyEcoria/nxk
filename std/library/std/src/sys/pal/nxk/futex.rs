//! Kernel futex (physical-address keys): backing for `Mutex`, `Condvar`,
//! `RwLock`, `Once` and thread parking.

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

/// Wait for `futex_wake` unless changed since `expected`; false on timeout.
pub fn futex_wait(futex: &Atomic<u32>, expected: u32, timeout: Option<Duration>) -> bool {
    let r = syscall::futex_wait(futex, expected, super::timeout_ns(timeout));
    r != -ETIMEDOUT
}

/// Wake one waiter; true if one was woken.
pub fn futex_wake(futex: &Atomic<u32>) -> bool {
    syscall::futex_wake(futex, 1) > 0
}

/// Wake all waiters.
pub fn futex_wake_all(futex: &Atomic<u32>) {
    syscall::futex_wake(futex, u32::MAX as u64);
}

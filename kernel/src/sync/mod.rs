pub mod spinlock;

#[allow(unused_imports)]
pub use spinlock::{IrqGuard, SpinLock, SpinLockGuard};

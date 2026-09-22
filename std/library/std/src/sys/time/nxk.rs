//! `Instant`: EL0 generic-timer counter scaled to ns (no syscall, vDSO-style).
//! `SystemTime`: kernel wall clock (`clock` fast path), set from the RTC by the power service.

use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sys::pal::syscall;
use crate::time::Duration;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

/// ns = ticks * MULT >> 32 (0: not computed yet).
static MULT: AtomicU64 = AtomicU64::new(0);

#[inline]
fn ticks() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[cold]
fn init_mult() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack, preserves_flags)) };
    let m = ((1_000_000_000u128 << 32) / f.max(1) as u128) as u64;
    MULT.store(m, Ordering::Relaxed);
    m
}

/// Monotonic time in ns since boot.
#[inline]
pub fn monotonic_ns() -> u64 {
    let mut m = MULT.load(Ordering::Relaxed);
    if m == 0 {
        m = init_mult();
    }
    ((ticks() as u128 * m as u128) >> 32) as u64
}

impl Instant {
    pub fn now() -> Instant {
        Instant(Duration::from_nanos(monotonic_ns()))
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }

    /// Nanoseconds since boot.
    pub fn as_nanos(&self) -> u128 {
        self.0.as_nanos()
    }
}

impl SystemTime {
    pub const MAX: SystemTime = SystemTime(Duration::MAX);

    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    pub fn now() -> SystemTime {
        SystemTime(Duration::from_nanos(syscall::realtime_ns()))
    }

    /// Date in seconds since 1970 (file timestamps).
    pub fn from_secs(s: u64) -> SystemTime {
        SystemTime(Duration::from_secs(s))
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_sub(*other)?))
    }
}

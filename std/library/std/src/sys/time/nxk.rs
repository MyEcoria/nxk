//! `Instant`: EL0 generic-timer counter scaled to ns (no syscall, vDSO-style).
//! `SystemTime`: kernel wall clock (`clock` fast path), set from the RTC by the power service.

use crate::sys::pal::syscall;
use crate::time::Duration;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

/// Monotonic time in ns since boot (`abi::user`, shared with libnx).
pub use crate::sys::pal::abi::user::monotonic_ns;

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

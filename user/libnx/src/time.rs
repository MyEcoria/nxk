//! Time: direct hardware counter reads, no syscall (the kernel
//! opens `CNTVCT_EL0` to EL0, like the Linux vDSO).

use crate::sys;

pub use nxk_abi::user::{counter as ticks, counter_freq as freq, ticks_to_ns};

#[inline]
pub fn now_ns() -> u64 {
    nxk_abi::user::monotonic_ns()
}

/// Wall time (ns since 1970, UTC), kept by the kernel.
pub fn realtime_ns() -> u64 {
    sys::realtime_ns()
}

pub fn sleep_ms(ms: u64) {
    sys::sleep_ns(ms * 1_000_000);
}

pub fn sleep_us(us: u64) {
    sys::sleep_ns(us * 1000);
}

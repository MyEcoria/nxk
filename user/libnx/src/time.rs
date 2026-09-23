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

/// Busy-wait budget before `poll` starts sleeping: most hardware
/// answers by then.
const SPIN_NS: u64 = 20_000;

/// Poll `done` for at most `timeout_us`: spin for the first 20 us, then
/// sleep a quarter of the time waited so far (1 us to 1 ms), so a long
/// wait leaves the core to others at a bounded (25%) delay cost.
pub fn poll(timeout_us: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = now_ns();
    let end = start + timeout_us * 1000;
    loop {
        if done() {
            return true;
        }
        let now = now_ns();
        if now > end {
            // Descheduled past the deadline: one last look.
            return done();
        }
        let waited = now - start;
        if waited < SPIN_NS {
            core::hint::spin_loop();
        } else {
            sys::sleep_ns((waited / 4).clamp(1_000, 1_000_000).min(end - now));
        }
    }
}

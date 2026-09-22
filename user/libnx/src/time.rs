//! Time: direct hardware counter reads, no syscall (the kernel
//! opens `CNTVCT_EL0` to EL0, like the Linux vDSO).

use core::arch::asm;

use crate::sys;

#[inline(always)]
pub fn ticks() -> u64 {
    let v: u64;
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline(always)]
pub fn freq() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
    v
}

#[inline]
pub fn ticks_to_ns(t: u64) -> u64 {
    ((t as u128 * 1_000_000_000) / freq() as u128) as u64
}

pub fn now_ns() -> u64 {
    ticks_to_ns(ticks())
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

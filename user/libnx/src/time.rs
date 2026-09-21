//! Temps : lecture directe du compteur materiel, sans appel systeme (le noyau
//! ouvre `CNTVCT_EL0` a EL0, comme le vDSO de Linux).

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

/// Heure murale (ns depuis 1970, UTC), tenue par le noyau.
pub fn realtime_ns() -> u64 {
    sys::realtime_ns()
}

pub fn sleep_ms(ms: u64) {
    sys::sleep_ns(ms * 1_000_000);
}

pub fn sleep_us(us: u64) {
    sys::sleep_ns(us * 1000);
}

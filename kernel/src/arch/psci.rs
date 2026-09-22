//! PSCI power services via `smc` when started under a monitor.

use core::sync::atomic::{AtomicBool, Ordering};

const VERSION: u64 = 0x8400_0000;
const CPU_ON_64: u64 = 0xC400_0003;
const SYSTEM_OFF: u64 = 0x8400_0008;
const SYSTEM_RESET: u64 = 0x8400_0009;

static PRESENT: AtomicBool = AtomicBool::new(false);

#[inline]
fn smc(fid: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let r: u64;
    unsafe {
        core::arch::asm!("smc #0",
            inlateout("x0") fid => r,
            inlateout("x1") a1 => _,
            inlateout("x2") a2 => _,
            inlateout("x3") a3 => _,
            out("x4") _, out("x5") _, out("x6") _, out("x7") _,
            out("x8") _, out("x9") _, out("x10") _, out("x11") _,
            out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("x16") _, out("x17") _,
            options(nostack))
    };
    r
}

/// Call only with a monitor present (non-EL3 boot): `smc` without EL3 is an
/// undefined instruction. Returns the version.
pub fn probe() -> Option<(u16, u16)> {
    let v = smc(VERSION, 0, 0, 0);
    if v as i64 <= 0 || v >> 32 != 0 {
        return None;
    }
    PRESENT.store(true, Ordering::Relaxed);
    Some(((v >> 16) as u16, v as u16))
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Power on core `mpidr`, starting at physical address `entry`.
pub fn cpu_on(mpidr: u64, entry: u64) -> i64 {
    smc(CPU_ON_64, mpidr, entry, 0) as i64
}

/// Returns only if the monitor refuses (nxk-bl31 has no PMIC access).
pub fn system_off() {
    smc(SYSTEM_OFF, 0, 0, 0);
}

/// Returns only if the monitor refuses.
pub fn system_reset() {
    smc(SYSTEM_RESET, 0, 0, 0);
}

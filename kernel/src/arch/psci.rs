//! PSCI : services d'alimentation d'un moniteur securise (ATF, exosphere...).
//!
//! Quand nxk est lance en EL2 ou EL1 par un moniteur, les registres de
//! gestion d'alimentation (PMC, CAR) peuvent lui etre interdits : il demande
//! alors au moniteur, par `smc`, d'allumer un coeur ou d'arreter la machine.
//! Lance directement en EL3 (sans moniteur), il pilote le materiel lui-meme.

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

/// A appeler seulement si un moniteur est present (demarrage hors EL3) :
/// un `smc` sans EL3 serait une instruction indefinie. Rend la version.
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

/// Allume le coeur `mpidr`, qui demarrera a l'adresse physique `entry`.
pub fn cpu_on(mpidr: u64, entry: u64) -> i64 {
    smc(CPU_ON_64, mpidr, entry, 0) as i64
}

pub fn system_off() -> ! {
    smc(SYSTEM_OFF, 0, 0, 0);
    crate::arch::park()
}

pub fn system_reset() -> ! {
    smc(SYSTEM_RESET, 0, 0, 0);
    crate::arch::park()
}

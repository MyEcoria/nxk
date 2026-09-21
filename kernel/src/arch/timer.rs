//! Timer generique ARM : base de temps et echeances, sans tic periodique.
//!
//! Chaque coeur programme son timer physique EL1 en valeur absolue
//! (`CNTP_CVAL_EL0`) sur la plus proche de ses echeances : fin du quantum en
//! cours (seulement si un thread de meme priorite attend) et premier dormeur
//! ou attente bornee. Sans rien de tout cela, le timer est arrete : un coeur
//! au repos ne se reveille que pour une interruption utile, et un thread seul
//! sur son coeur n'est jamais interrompu pour rien.
//!
//! Precision : celle du compteur (52 ns a 19,2 MHz), pas celle d'un tic.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::arch::{self, gic};
use crate::kern::percpu::{self, PerCpu};
use crate::kern::thread::{self, NIL};
use crate::kern::sched;
use crate::{sysreg_read, sysreg_write};

const CNTP_CTL_ENABLE: u64 = 1 << 0;

static FREQ_HZ: AtomicU64 = AtomicU64::new(0);
/// Heure murale = temps monotone + decalage (ns), regle par le service
/// d'horloge a partir de la RTC.
static REALTIME_OFFSET: AtomicI64 = AtomicI64::new(0);

#[inline(always)]
pub fn counter() -> u64 {
    arch::isb();
    sysreg_read!("cntpct_el0")
}

pub fn freq_hz() -> u64 {
    FREQ_HZ.load(Ordering::Relaxed)
}

pub fn ns_to_ticks(ns: u64) -> u64 {
    ((ns as u128 * freq_hz() as u128) / 1_000_000_000) as u64
}

pub fn ticks_to_ns(t: u64) -> u64 {
    ((t as u128 * 1_000_000_000) / freq_hz().max(1) as u128) as u64
}

pub fn uptime_us() -> u64 {
    ticks_to_ns(counter()) / 1000
}

pub fn monotonic_ns() -> u64 {
    ticks_to_ns(counter())
}

pub fn realtime_ns() -> u64 {
    (monotonic_ns() as i64).wrapping_add(REALTIME_OFFSET.load(Ordering::Relaxed)) as u64
}

pub fn set_realtime(ns: u64) {
    REALTIME_OFFSET.store((ns as i64).wrapping_sub(monotonic_ns() as i64), Ordering::Relaxed);
}

pub fn delay_us(us: u64) {
    let target = counter() + ns_to_ticks(us * 1000);
    while counter() < target {
        core::hint::spin_loop();
    }
}

/// Frequence du compteur, fixee une fois au demarrage.
pub fn init_global() {
    let freq = sysreg_read!("cntfrq_el0");
    let freq = if freq == 0 { 19_200_000 } else { freq };
    FREQ_HZ.store(freq, Ordering::Relaxed);
}

/// Prepare le timer du coeur courant (arrete tant qu'aucune echeance).
pub fn init_cpu() {
    sysreg_write!("cntp_ctl_el0", 0);
    percpu::this().timer_cval = u64::MAX;
    gic::set_priority(gic::IRQ_TIMER, 0x80);
    gic::enable_irq(gic::IRQ_TIMER);
    arch::isb();
}

/// Programme la plus proche echeance du coeur courant, ou arrete le timer.
pub fn reprogram(c: &mut PerCpu) {
    c.timer_dirty = false;
    let mut d = u64::MAX;
    if c.slice_end != 0 {
        d = c.slice_end;
    }
    if c.tmo_head != NIL {
        d = d.min(thread::get(c.tmo_head).wake_at);
    }
    if d == c.timer_cval {
        return;
    }
    crate::trace!("[s{}] timer {} (tete {})", c.id, d, c.tmo_head);
    c.timer_cval = d;
    // Sortie du timer d'abord ramenee a 0 : une echeance deja depassee
    // produira ainsi un front montant. Si le GIC traite la ligne sur front
    // (l'emulateur configure ainsi tous les PPI), une echeance atteinte
    // pendant le traitement de la precedente serait sinon perdue.
    sysreg_write!("cntp_ctl_el0", 0);
    if d != u64::MAX {
        arch::isb();
        sysreg_write!("cntp_cval_el0", d);
        sysreg_write!("cntp_ctl_el0", CNTP_CTL_ENABLE);
    }
}

/// Interruption du timer : fin de quantum et/ou echeances atteintes.
pub fn on_irq() {
    let c = percpu::this();
    let now = counter();
    crate::trace!("[s{}] timer irq now {} cval {}", c.id, now, c.timer_cval);
    if c.slice_end != 0 && now >= c.slice_end {
        c.slice_end = 0;
        c.slice_expired = true;
        c.need_resched = true;
    }
    sched::expire(now);
    // L'echeance atteinte maintient la ligne active : il faut en changer
    // avant de rendre la main.
    c.timer_cval = 0;
    reprogram(c);
}

//! Primitives AArch64 : registres systeme, barrieres, masquage d'interruptions.

pub mod exceptions;
pub mod fpu;
pub mod gic;
pub mod mmu;
pub mod psci;
pub mod smp;
pub mod spectre;
pub mod timer;

use core::arch::asm;

/// Lit un registre systeme.
#[macro_export]
macro_rules! sysreg_read {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

/// Ecrit un registre systeme.
#[macro_export]
macro_rules! sysreg_write {
    ($name:literal, $val:expr) => {{
        let v: u64 = $val;
        unsafe { core::arch::asm!(concat!("msr ", $name, ", {}"), in(reg) v, options(nomem, nostack)) };
    }};
}

#[inline(always)]
pub fn core_id() -> usize {
    (sysreg_read!("mpidr_el1") & 0xFF) as usize
}

#[inline(always)]
pub fn current_el() -> u64 {
    sysreg_read!("CurrentEL") >> 2
}

#[inline(always)]
pub fn wfe() {
    unsafe { asm!("wfe", options(nomem, nostack)) };
}

#[inline(always)]
pub fn wfi() {
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

#[inline(always)]
pub fn sev() {
    unsafe { asm!("sev", options(nomem, nostack)) };
}

#[inline(always)]
pub fn nop() {
    unsafe { asm!("nop", options(nomem, nostack)) };
}

/// Barriere de synchronisation de donnees, domaine interne partageable.
#[inline(always)]
pub fn dsb_ish() {
    unsafe { asm!("dsb ish", options(nostack)) };
}

/// Barriere de donnees complete (peripheriques compris).
#[inline(always)]
pub fn dsb_sy() {
    unsafe { asm!("dsb sy", options(nostack)) };
}

#[inline(always)]
pub fn isb() {
    unsafe { asm!("isb", options(nostack)) };
}

#[inline(always)]
pub fn irq_enable() {
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

#[inline(always)]
pub fn irq_disable() {
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
}

/// Masque les IRQ et renvoie l'etat precedent de DAIF.
#[inline(always)]
pub fn irq_save() -> u64 {
    let flags = sysreg_read!("daif");
    irq_disable();
    flags
}

#[inline(always)]
pub fn irq_restore(flags: u64) {
    sysreg_write!("daif", flags);
}

/// Reglages EL1 propres a chaque coeur, apres activation de la MMU.
pub fn cpu_init() {
    let mut s = sysreg_read!("sctlr_el1");
    s |= (1 << 3)   // SA  : pile EL1 alignee sur 16
        | (1 << 4)  // SA0 : pile EL0 alignee sur 16
        | (1 << 14) // DZE : DC ZVA autorise en EL0 (memset rapide)
        | (1 << 15) // UCT : CTR_EL0 lisible en EL0
        | (1 << 18) // nTWE : WFE autorise en EL0 (verrous utilisateur)
        | (1 << 26); // UCI : maintenance de cache en EL0
    sysreg_write!("sctlr_el1", s);
    // Compteur virtuel lisible en EL0 : l'heure sans appel systeme.
    sysreg_write!("cntkctl_el1", 1 << 1);
    // Verrou « OS » du debogage leve (il l'est au reset a froid) : sans cela,
    // aucune exception de pas a pas ne serait jamais prise.
    sysreg_write!("oslar_el1", 0);
    sysreg_write!("mdscr_el1", 0);
    isb();
    fpu::init_cpu();
}

/// Boucle d'attente definitive (coeur arrete).
pub fn park() -> ! {
    loop {
        unsafe { asm!("msr daifset, #0xF", "wfi", options(nomem, nostack)) };
    }
}

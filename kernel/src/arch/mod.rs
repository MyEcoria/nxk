//! AArch64 primitives: system registers, barriers, interrupt masking.

pub mod exceptions;
pub mod fpu;
pub mod gic;
pub mod mmu;
pub mod psci;
pub mod smp;
pub mod spectre;
pub mod timer;

use core::arch::asm;

/// Read a system register.
#[macro_export]
macro_rules! sysreg_read {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)) };
        v
    }};
}

/// Write a system register.
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

/// Full data barrier (devices included).
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

/// Mask IRQs and return the previous DAIF state.
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

/// Per-core EL1 setup, after MMU enable.
pub fn cpu_init() {
    let mut s = sysreg_read!("sctlr_el1");
    s |= (1 << 3)   // SA  : 16-byte aligned EL1 stack
        | (1 << 4)  // SA0 : 16-byte aligned EL0 stack
        | (1 << 14) // DZE : DC ZVA allowed in EL0 (fast memset)
        | (1 << 15) // UCT : CTR_EL0 readable in EL0
        | (1 << 18) // nTWE : WFE allowed in EL0 (user locks)
        | (1 << 26); // UCI : cache maintenance in EL0
    sysreg_write!("sctlr_el1", s);
    // Virtual counter readable in EL0: time without a syscall.
    sysreg_write!("cntkctl_el1", 1 << 1);
    // Clear the debug OS lock (set at cold reset): otherwise single-step
    // exceptions would never be taken.
    sysreg_write!("oslar_el1", 0);
    sysreg_write!("mdscr_el1", 0);
    isb();
    fpu::init_cpu();
}

/// Definitive wait loop (stopped core).
pub fn park() -> ! {
    loop {
        unsafe { asm!("msr daifset, #0xF", "wfi", options(nomem, nostack)) };
    }
}

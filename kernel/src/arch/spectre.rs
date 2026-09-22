//! Spectre-v2 (CVE-2017-5715): flush predictor on process switch.
//! Needs CPUACTLR_EL1[0] set in EL3 at boot (see `boot.rs`).

#[inline(always)]
pub fn on_process_switch() {
    if cfg!(feature = "spectre-v2") {
        unsafe { core::arch::asm!("ic iallu", "dsb nsh", "isb", options(nostack)) };
    }
}

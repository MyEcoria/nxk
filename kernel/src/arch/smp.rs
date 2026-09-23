//! Secondary CCPLEX boot: SB reset vector, PMC power, CAR reset.
//! Cores restart in EL3 at `_start`, like core 0.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::timer;
use crate::kern::percpu::NR_CPUS;
use crate::mmio::{read32, write32};

// --- System Block: AArch64 reset vector ---
const SB_BASE: usize = 0x6000_C200;
const SB_AA64_RESET_LOW: usize = SB_BASE + 0x30;
const SB_AA64_RESET_HIGH: usize = SB_BASE + 0x34;
const SB_AA64_RST_AARCH64_MODE_EN: u32 = 1;

// --- PMC: power partitions ---
const PMC_BASE: usize = 0x7000_E400;
const PMC_PWRGATE_TOGGLE: usize = PMC_BASE + 0x30;
const PMC_REMOVE_CLAMPING: usize = PMC_BASE + 0x34;
const PMC_PWRGATE_STATUS: usize = PMC_BASE + 0x38;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;

/// PMC partition per core: CE0 for core 0, then CPU1..CPU3.
const CPU_PARTITION: [u32; 4] = [14, 9, 10, 11];

// --- CAR: CPU complex reset lines ---
const CAR_BASE: usize = 0x6000_6000;
const RST_CPUG_CMPLX_CLR: usize = CAR_BASE + 0x454;

/// Reset mask for one core: CPU, DE, WD, DBG, CORE and CX.
const fn cpu_reset_mask(cpu: usize) -> u32 {
    (0x1111 << cpu) | (1 << (16 + cpu)) | (1 << (20 + cpu))
}

/// Cores that reported arrival.
static ONLINE: AtomicUsize = AtomicUsize::new(1);

pub fn mark_online() {
    ONLINE.fetch_add(1, Ordering::Release);
}

pub fn online_count() -> usize {
    ONLINE.load(Ordering::Acquire)
}

fn pwrgate_on(partition: u32) {
    unsafe {
        if read32(PMC_PWRGATE_STATUS) & (1 << partition) == 0 {
            write32(PMC_PWRGATE_TOGGLE, partition | PWRGATE_TOGGLE_START);
            // Synchronous on the emulator; on silicon wait for the ack.
            let mut spins = 0;
            while read32(PMC_PWRGATE_STATUS) & (1 << partition) == 0 && spins < 100_000 {
                spins += 1;
                core::hint::spin_loop();
            }
        }
        write32(PMC_REMOVE_CLAMPING, 1 << partition);
    }
}

/// Start cores 1-3. Returns the cores actually online.
pub fn start_secondaries(entry: usize) -> usize {
    unsafe {
        write32(
            SB_AA64_RESET_LOW,
            (entry as u32) | SB_AA64_RST_AARCH64_MODE_EN,
        );
        write32(SB_AA64_RESET_HIGH, (entry >> 32) as u32);
    }

    for cpu in 1..NR_CPUS {
        let expected = online_count() + 1;

        pwrgate_on(CPU_PARTITION[cpu]);
        unsafe { write32(RST_CPUG_CMPLX_CLR, cpu_reset_mask(cpu)) };

        // Bounded wait: a missing core must not freeze boot.
        let deadline = timer::counter() + timer::freq_hz() / 2;
        while online_count() < expected && timer::counter() < deadline {
            core::hint::spin_loop();
        }
    }

    online_count()
}

/// Start cores 1-3 via PSCI (secure monitor present).
pub fn start_secondaries_psci(entry: usize) -> usize {
    for cpu in 1..NR_CPUS {
        let expected = online_count() + 1;
        let r = crate::arch::psci::cpu_on(cpu as u64, entry as u64);
        if r != 0 {
            crate::kprintln!("[smp] PSCI CPU_ON core {}: error {}", cpu, r);
            continue;
        }
        let deadline = timer::counter() + timer::freq_hz() / 2;
        while online_count() < expected && timer::counter() < deadline {
            core::hint::spin_loop();
        }
    }
    online_count()
}

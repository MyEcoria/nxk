//! Demarrage des coeurs secondaires du CCPLEX.
//!
//! Sur Tegra X1 un coeur A57 ne redemarre pas avec une simple ecriture : il faut
//! (1) publier le vecteur de reset AArch64 dans le bloc SB, (2) rallumer sa
//! partition d'alimentation via le PMC et retirer les verrous de tension, puis
//! (3) relacher ses lignes de reset dans le CAR. Le coeur repart alors en EL3
//! sur `_start`, exactement comme le coeur 0.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::timer;
use crate::mmio::{read32, write32};
use crate::kern::percpu::NR_CPUS;

// --- System Block : vecteur de reset AArch64 ---
const SB_BASE: usize = 0x6000_C200;
const SB_AA64_RESET_LOW: usize = SB_BASE + 0x30;
const SB_AA64_RESET_HIGH: usize = SB_BASE + 0x34;
const SB_AA64_RST_AARCH64_MODE_EN: u32 = 1;

// --- PMC : partitions d'alimentation ---
const PMC_BASE: usize = 0x7000_E400;
const PMC_PWRGATE_TOGGLE: usize = PMC_BASE + 0x30;
const PMC_REMOVE_CLAMPING: usize = PMC_BASE + 0x34;
const PMC_PWRGATE_STATUS: usize = PMC_BASE + 0x38;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;

/// Partition PMC de chaque coeur : CE0 pour le coeur 0, puis CPU1..CPU3.
const CPU_PARTITION: [u32; 4] = [14, 9, 10, 11];

// --- CAR : lignes de reset du complexe CPU ---
const CAR_BASE: usize = 0x6000_6000;
const RST_CPUG_CMPLX_SET: usize = CAR_BASE + 0x450;
const RST_CPUG_CMPLX_CLR: usize = CAR_BASE + 0x454;

/// Masque de reset d'un coeur : CPU, DE, WD, DBG, CORE et CX.
const fn cpu_reset_mask(cpu: usize) -> u32 {
    (0x1111 << cpu) | (1 << (16 + cpu)) | (1 << (20 + cpu))
}

/// Nombre de coeurs ayant signale leur arrivee.
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
            // L'etat bascule de maniere synchrone cote emulateur ; sur silicium
            // il faut attendre l'acquittement.
            let mut spins = 0;
            while read32(PMC_PWRGATE_STATUS) & (1 << partition) == 0 && spins < 100_000 {
                spins += 1;
                core::hint::spin_loop();
            }
        }
        write32(PMC_REMOVE_CLAMPING, 1 << partition);
    }
}

/// Lance les coeurs 1 a 3. Renvoie le nombre de coeurs effectivement en ligne.
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

        // Attente bornee : un coeur absent ne doit pas figer le demarrage.
        let deadline = timer::counter() + timer::freq_hz() / 2; // 500 ms
        while online_count() < expected && timer::counter() < deadline {
            core::hint::spin_loop();
        }
    }

    online_count()
}

/// Lance les coeurs 1 a 3 par PSCI (moniteur securise present).
pub fn start_secondaries_psci(entry: usize) -> usize {
    for cpu in 1..NR_CPUS {
        let expected = online_count() + 1;
        let r = crate::arch::psci::cpu_on(cpu as u64, entry as u64);
        if r != 0 {
            crate::kprintln!("[smp] PSCI CPU_ON coeur {} : erreur {}", cpu, r);
            continue;
        }
        let deadline = timer::counter() + timer::freq_hz() / 2;
        while online_count() < expected && timer::counter() < deadline {
            core::hint::spin_loop();
        }
    }
    online_count()
}

/// Remet un coeur en reset (arret propre, non utilise pour l'instant).
pub fn stop_core(cpu: usize) {
    unsafe { write32(RST_CPUG_CMPLX_SET, cpu_reset_mask(cpu)) };
}

//! GIC-400 (GICv2) du Tegra X1.
//!
//! Distributeur en 0x5004_1000, interface CPU en 0x5004_2000. Les 32 premiers
//! identifiants sont bancarises par coeur (16 SGI + 16 PPI), les SPI commencent
//! a 32. Le timer physique EL1 arrive sur le PPI 30, les IPI d'ordonnancement
//! utilisent la SGI 0.

use crate::arch;
use crate::mmio::{read32, write32};


const GICD_BASE: usize = 0x5004_1000;
const GICC_BASE: usize = 0x5004_2000;

const GICD_CTLR: usize = GICD_BASE + 0x000;
const GICD_TYPER: usize = GICD_BASE + 0x004;
const GICD_IGROUPR: usize = GICD_BASE + 0x080;
const GICD_ISENABLER: usize = GICD_BASE + 0x100;
const GICD_ICENABLER: usize = GICD_BASE + 0x180;
const GICD_ICPENDR: usize = GICD_BASE + 0x280;
const GICD_IPRIORITYR: usize = GICD_BASE + 0x400;
const GICD_ITARGETSR: usize = GICD_BASE + 0x800;
const GICD_ICFGR: usize = GICD_BASE + 0xC00;
const GICD_SGIR: usize = GICD_BASE + 0xF00;

const GICC_CTLR: usize = GICC_BASE + 0x00;
const GICC_PMR: usize = GICC_BASE + 0x04;
const GICC_BPR: usize = GICC_BASE + 0x08;
const GICC_IAR: usize = GICC_BASE + 0x0C;
const GICC_EOIR: usize = GICC_BASE + 0x10;

/// Identifiant rendu par IAR quand il n'y a rien a traiter.
pub const SPURIOUS: u32 = 1023;

pub const IRQ_TIMER: u32 = 30; // PPI 14 : timer physique EL1
pub const SGI_RESCHEDULE: u32 = 0;
pub const SGI_STOP: u32 = 1;


/// Nombre de lignes gerees par le distributeur.
pub fn num_irqs() -> u32 {
    let typer = unsafe { read32(GICD_TYPER) };
    ((typer & 0x1F) + 1) * 32
}

/// Bascule toutes les interruptions en Groupe 1 (monde non securise).
///
/// A executer **en EL3** : les registres `IGROUPR` ne sont accessibles que
/// depuis l'etat securise, et tant qu'une ligne reste en Groupe 0 elle est
/// routee vers le monde securise, donc perdue pour le noyau. C'est exactement
/// ce que fait un moniteur securise (ATF, secmon) avant de rendre la main.
///
/// Le coeur 0 configure le distributeur global ; chaque coeur configure ses
/// 32 lignes bancarisees (SGI et PPI, dont le timer).
#[no_mangle]
pub extern "C" fn nxk_secure_gic_init(core: usize) {
    unsafe {
        if core == 0 {
            write32(GICD_CTLR, 0);
            let lines = num_irqs().min(1020);
            for i in (0..lines).step_by(32) {
                write32(GICD_IGROUPR + (i as usize / 32) * 4, 0xFFFF_FFFF);
            }
            // EnableGrp0 | EnableGrp1
            write32(GICD_CTLR, 0b11);
        }
        // Lignes bancarisees du coeur courant.
        write32(GICD_IGROUPR, 0xFFFF_FFFF);
        // Vue securisee de l'interface CPU : Groupe 0 en FIQ, Groupe 1 en IRQ.
        write32(GICC_CTLR, (1 << 0) | (1 << 1) | (1 << 3));
        write32(GICC_PMR, 0xFF);
    }
    crate::arch::dsb_sy();
}

/// Initialise le distributeur. A n'appeler que depuis le coeur de demarrage.
pub fn init_dist() {
    unsafe {
        write32(GICD_CTLR, 0);

        let lines = num_irqs().min(256);
        // SPI : tout desactive, non en attente, priorite moyenne, groupe 0.
        for i in (32..lines).step_by(32) {
            let n = (i / 32) as usize;
            write32(GICD_ICENABLER + n * 4, 0xFFFF_FFFF);
            write32(GICD_ICPENDR + n * 4, 0xFFFF_FFFF);
        }
        for i in (32..lines).step_by(4) {
            write32(GICD_IPRIORITYR + i as usize, 0xA0A0_A0A0);
            write32(GICD_ITARGETSR + i as usize, 0x0101_0101); // coeur 0 par defaut
        }
        for i in (32..lines).step_by(16) {
            write32(GICD_ICFGR + (i as usize / 16) * 4, 0); // declenchement par niveau
        }

        // Vue non securisee : bit 0 = EnableGrp1.
        write32(GICD_CTLR, 1);
    }
    arch::dsb_sy();
}

/// Initialise l'interface CPU du coeur courant (a appeler sur chaque coeur).
pub fn init_cpu() {
    unsafe {
        // SGI et PPI : priorite haute, actives selon besoin.
        write32(GICD_ICENABLER, 0xFFFF_FFFF);
        write32(GICD_ICPENDR, 0xFFFF_FFFF);
        for i in (0..32).step_by(4) {
            write32(GICD_IPRIORITYR + i, 0x8080_8080);
        }
        write32(GICD_ISENABLER, 0x0000_FFFF); // les 16 SGI

        write32(GICC_PMR, 0xF0); // accepte toutes les priorites utiles
        write32(GICC_BPR, 0x03);
        write32(GICC_CTLR, 1);
    }
    arch::dsb_sy();
}

pub fn enable_irq(irq: u32) {
    let (n, bit) = ((irq / 32) as usize, irq % 32);
    unsafe { write32(GICD_ISENABLER + n * 4, 1 << bit) };
    arch::dsb_sy();
}

pub fn disable_irq(irq: u32) {
    let (n, bit) = ((irq / 32) as usize, irq % 32);
    unsafe { write32(GICD_ICENABLER + n * 4, 1 << bit) };
    arch::dsb_sy();
}

pub fn set_priority(irq: u32, prio: u8) {
    unsafe {
        let addr = GICD_IPRIORITYR + (irq as usize & !3);
        let shift = (irq as usize & 3) * 8;
        let v = read32(addr);
        write32(addr, (v & !(0xFF << shift)) | ((prio as u32) << shift));
    }
}

/// Route une ligne SPI vers un masque de coeurs.
pub fn set_target(irq: u32, cpu_mask: u8) {
    unsafe {
        let addr = GICD_ITARGETSR + (irq as usize & !3);
        let shift = (irq as usize & 3) * 8;
        let v = read32(addr);
        write32(addr, (v & !(0xFF << shift)) | ((cpu_mask as u32) << shift));
    }
}


/// Envoie une interruption logicielle a un ensemble de coeurs.
pub fn send_sgi(cpu_mask: u8, sgi: u32) {
    arch::dsb_sy();
    unsafe { write32(GICD_SGIR, ((cpu_mask as u32) << 16) | (sgi & 0xF)) };
}

/// Prend en charge l'interruption la plus prioritaire (lecture de IAR).
#[inline]
pub fn ack() -> u32 {
    unsafe { read32(GICC_IAR) }
}

/// Signale la fin de traitement (valeur de IAR complete, source SGI comprise).
#[inline]
pub fn eoi(iar: u32) {
    unsafe { write32(GICC_EOIR, iar) };
}


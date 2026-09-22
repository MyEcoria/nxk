//! Tables de pages AArch64 (granule 4 KiB, VA sur 39 bits).
//!
//! Mapping identite : l'adresse virtuelle vaut l'adresse physique. Suffisant
//! pour un noyau qui n'expose pas encore d'espace utilisateur, et cela evite
//! toute translation a la main dans les pilotes.
//!
//! Decoupage :
//!   0x0000_0000 - 0x8000_0000  peripheriques (Device-nGnRE, non executable)
//!   0x8000_0000 - fin de DRAM  DRAM (Normal WB, partageable interne)
//!
//! La taille de la DRAM (4, 6 ou 8 GiB) est lue dans le controleur memoire :
//! seule la memoire presente est projetee, pour qu'aucun acces speculatif ne
//! vise une adresse sans memoire derriere.
//!
//! L'image du noyau est decoupee en pages de 4 KiB pour donner a chaque section
//! ses droits propres : texte en RX lecture seule, rodata en RO non executable,
//! donnees et piles en RW non executable. Le reste de la DRAM est en blocs de
//! 2 MiB ou 1 GiB pour limiter le nombre d'entrees et les defauts de TLB.

use crate::arch;
use crate::{sysreg_read, sysreg_write};

// --- Indices MAIR ---
pub const ATTR_DEVICE_NGNRE: u64 = 0;
pub const ATTR_NORMAL_WB: u64 = 1;
pub const ATTR_NORMAL_NC: u64 = 2;
const MAIR_VALUE: u64 = 0x04 | (0xFF << 8) | (0x44 << 16);

// --- Bits de descripteur ---
pub const VALID: u64 = 1 << 0;
pub const TABLE: u64 = 1 << 1; // avec VALID : descripteur de table (L0-L2) ou page (L3)
pub const AF: u64 = 1 << 10;
pub const SH_INNER: u64 = 0b11 << 8;
pub const AP_RW_EL1: u64 = 0b00 << 6;
pub const AP_RO_EL1: u64 = 0b10 << 6;
pub const PXN: u64 = 1 << 53;
pub const UXN: u64 = 1 << 54;

pub const fn attr(idx: u64) -> u64 {
    idx << 2
}

pub const PAGE_SIZE: usize = 4096;
pub const ENTRIES: usize = 512;
pub const L1_SIZE: usize = 1 << 30; // 1 GiB par entree de niveau 1
pub const L2_SIZE: usize = 2 << 20; // 2 MiB par entree de niveau 2

pub const DRAM_BASE: usize = 0x8000_0000;
/// MC_EMEM_CFG : taille de la DRAM en Mo (bits 13:0), ecrite par le chargeur
/// d'amorcage au moment d'initialiser la SDRAM.
const MC_EMEM_CFG: usize = 0x7001_9050;

static mut DRAM_END: usize = DRAM_BASE + L1_SIZE;

#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

impl Table {
    const fn zeroed() -> Self {
        Self([0; ENTRIES])
    }
    #[inline]
    fn addr(&self) -> u64 {
        self.0.as_ptr() as u64
    }
}

// Niveau 1 : couvre 512 GiB. Seules les premieres entrees servent.
static mut L1: Table = Table::zeroed();
// Niveau 2 pour les deux premiers gigaoctets (peripheriques).
static mut L2_MMIO: [Table; 2] = [Table::zeroed(), Table::zeroed()];
// Niveau 2 pour le premier gigaoctet de DRAM.
static mut L2_DRAM0: Table = Table::zeroed();
// Niveau 3 pour les 2 premiers MiB de DRAM : l'image du noyau.
static mut L3_KERNEL: Table = Table::zeroed();
// Niveau 2 pour les gigaoctets incomplets ou troues par une zone reservee.
const L2_POOL: usize = 4;
static mut L2_DRAM_POOL: [Table; L2_POOL] = [const { Table::zeroed() }; L2_POOL];

/// Zones de DRAM reservees par un moniteur securise ou un chargeur
/// (carveouts du controleur memoire), alignees sur 2 MiB. Ni allouees, ni
/// meme projetees : un acces speculatif non securise y provoquerait une
/// erreur asynchrone, et le controleur y ignore en silence les ecritures du
/// CPU qu'il refuse.
///
/// TZ-DRAM, carveout securise, VPR et les cinq carveouts generalises
/// (GSC1-5). Sur l'entree L4T, hekate place sous la TZ-DRAM le firmware BPMP
/// en cours d'execution et les zones du GPU (0xffd00000..0xffe00000,
/// inaccessibles au CPU) : voir `launch_l4t` dans refs/hekate.
pub const MAX_HOLES: usize = 8;

pub fn detect_holes() -> [(u64, u64); MAX_HOLES] {
    const MC: usize = 0x7001_9000;
    let rd = |off: usize| unsafe { core::ptr::read_volatile((MC + off) as *const u32) } as u64;
    let mut zones = [(0u64, 0u64); MAX_HOLES];
    // TZ-DRAM : MC_SECURITY_CFG0 (base, Mo), CFG3 (bits 33:32), CFG1 (taille, Mo).
    zones[0] = (
        ((rd(0x9BC) & 3) << 32) | (rd(0x70) & 0xFFF0_0000),
        (rd(0x74) & 0x3FFF) << 20,
    );
    // Carveout securise : MC_SEC_CARVEOUT_BOM / ADR_HI / SIZE_MB.
    zones[1] = (
        ((rd(0x9D4) & 3) << 32) | (rd(0x670) & 0xFFF0_0000),
        (rd(0x674) & 0xFFF) << 20,
    );
    // VPR : MC_VIDEO_PROTECT_BOM / BOM_ADR_HI / SIZE_MB.
    zones[2] = (
        ((rd(0x978) & 3) << 32) | (rd(0x648) & 0xFFF0_0000),
        (rd(0x64C) & 0x3FFF) << 20,
    );
    // GSC n : MC_SECURITY_CARVEOUTn_BOM / BOM_HI / SIZE_128KB, pas de 0x50.
    for n in 0..5 {
        let r = 0xC0C + n * 0x50;
        zones[3 + n] = (
            ((rd(r + 4) & 3) << 32) | (rd(r) & !0x1_FFFF),
            (rd(r + 8) & 0xFFF) << 17,
        );
    }
    let mut holes = [(0u64, 0u64); MAX_HOLES];
    for (h, (b, sz)) in holes.iter_mut().zip(zones) {
        if sz != 0 {
            *h = round_hole(b, sz);
        }
    }
    holes
}

/// Zone reservee [base, base+taille) arrondie aux blocs de 2 MiB englobants.
pub fn round_hole(base: u64, size: u64) -> (u64, u64) {
    let m = L2_SIZE as u64 - 1;
    (base & !m, (base + size + m) & !m)
}

fn in_hole(holes: &[(u64, u64)], pa: u64) -> bool {
    holes.iter().any(|&(b, e)| pa >= b && pa < e)
}

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __kernel_end: u8;
}

#[inline]
fn sym(s: &'static u8) -> usize {
    s as *const u8 as usize
}

/// Origine de la taille de DRAM retenue.
pub enum DramSource {
    /// MC_EMEM_CFG, confirme par sondage.
    Controller,
    /// MC_EMEM_CFG annoncait plus que la memoire effectivement presente
    /// (emulateur : le registre garde sa valeur de reset, 8 GiB).
    Probed { announced_mb: usize },
}

/// Taille de la DRAM. A appeler MMU coupee (acces non caches, sans
/// speculation).
///
/// Sur la console, le chargeur d'amorcage programme MC_EMEM_CFG avec la
/// taille exacte : le sondage ne touche alors que de la memoire presente. Un
/// emulateur peut laisser la valeur de reset ; chaque gigaoctet annonce est
/// donc verifie par ecriture/relecture (valeur d'origine restauree).
pub fn detect_dram() -> (usize, DramSource) {
    let mb = unsafe { core::ptr::read_volatile(MC_EMEM_CFG as *const u32) } as usize & 0x3FFF;
    // Registre vierge : on sonde jusqu'au maximum du Tegra X1.
    let announced = if mb == 0 { 8192 } else { mb.clamp(1024, 8192) & !1 };
    let announced_bytes = announced << 20;
    let mut size = L1_SIZE; // le premier gigaoctet porte le noyau : present
    while size < announced_bytes {
        let probe = (size + L2_SIZE).min(announced_bytes) - L2_SIZE;
        if !probe_word(DRAM_BASE + probe) {
            break;
        }
        size = (size + L1_SIZE).min(announced_bytes);
    }
    if size == announced_bytes && mb != 0 {
        (size, DramSource::Controller)
    } else {
        (size, DramSource::Probed { announced_mb: mb })
    }
}

fn probe_word(pa: usize) -> bool {
    let p = pa as *mut u64;
    let pattern = 0x6E78_6B5F_6472_616D ^ pa as u64;
    unsafe {
        let old = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, pattern);
        let a = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, !pattern);
        let b = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, old);
        a == pattern && b == !pattern
    }
}

/// Fin (exclue) de la DRAM projetee.
pub fn dram_end() -> u64 {
    unsafe { DRAM_END as u64 }
}

/// Construit les tables. A n'executer qu'une fois, depuis le coeur 0.
pub unsafe fn build_tables(dram_size: usize, holes: &[(u64, u64)]) {
    let l1 = unsafe { &mut *core::ptr::addr_of_mut!(L1) };
    let l2_mmio = unsafe { &mut *core::ptr::addr_of_mut!(L2_MMIO) };
    let l2_dram = unsafe { &mut *core::ptr::addr_of_mut!(L2_DRAM0) };
    let l3 = unsafe { &mut *core::ptr::addr_of_mut!(L3_KERNEL) };

    // --- 0 - 2 GiB : registres des peripheriques, en blocs de 2 MiB ---
    for (i, table) in l2_mmio.iter_mut().enumerate() {
        for (j, e) in table.0.iter_mut().enumerate() {
            let pa = (i * L1_SIZE + j * L2_SIZE) as u64;
            *e = pa | VALID | AF | attr(ATTR_DEVICE_NGNRE) | AP_RW_EL1 | PXN | UXN;
        }
        l1.0[i] = table.addr() | VALID | TABLE;
    }

    // --- 1er GiB de DRAM : blocs de 2 MiB, sauf les 2 premiers MiB ---
    for (j, e) in l2_dram.0.iter_mut().enumerate() {
        let pa = (DRAM_BASE + j * L2_SIZE) as u64;
        *e = if in_hole(holes, pa) { 0 } else { pa | VALID | AF | SH_INNER | attr(ATTR_NORMAL_WB) | AP_RW_EL1 | PXN | UXN };
    }

    // --- Image du noyau : pages de 4 KiB avec droits par section ---
    let text_start = sym(unsafe { &__text_start });
    let text_end = sym(unsafe { &__text_end });
    let rodata_start = sym(unsafe { &__rodata_start });
    let rodata_end = sym(unsafe { &__rodata_end });
    let kernel_end = sym(unsafe { &__kernel_end });
    // Seul le code doit tenir dans les 2 premiers MiB (pages de 4 KiB avec
    // droit d'execution). Au-dela, donnees et .bss tombent dans les blocs de
    // 2 MiB en lecture/ecriture non executables, ce qui leur convient.
    let _ = kernel_end;
    assert!(
        text_end <= DRAM_BASE + L2_SIZE,
        "code noyau > 2 MiB : etendre L3_KERNEL"
    );

    for (j, e) in l3.0.iter_mut().enumerate() {
        let va = DRAM_BASE + j * PAGE_SIZE;
        let base = va as u64 | VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_WB) | UXN;
        *e = if va >= text_start && va < text_end {
            base | AP_RO_EL1 // code : lecture seule, executable en EL1
        } else if va >= rodata_start && va < rodata_end {
            base | AP_RO_EL1 | PXN
        } else {
            base | AP_RW_EL1 | PXN // donnees, bss, piles, et DRAM libre
        };
    }
    l2_dram.0[0] = l3.addr() | VALID | TABLE;
    l1.0[DRAM_BASE / L1_SIZE] = l2_dram.addr() | VALID | TABLE;

    // --- Reste de la DRAM : blocs de 1 GiB ; blocs de 2 MiB pour un
    // gigaoctet incomplet ou contenant une zone reservee ---
    let end = DRAM_BASE + dram_size;
    unsafe { DRAM_END = end };
    let normal = VALID | AF | SH_INNER | attr(ATTR_NORMAL_WB) | AP_RW_EL1 | PXN | UXN;
    let pool = unsafe { &mut *core::ptr::addr_of_mut!(L2_DRAM_POOL) };
    let mut used = 0;
    let mut pa = DRAM_BASE + L1_SIZE;
    while pa < end {
        let whole = pa + L1_SIZE <= end;
        let holed = holes.iter().any(|&(b, e)| b < (pa + L1_SIZE) as u64 && e > pa as u64);
        if whole && !holed {
            l1.0[pa / L1_SIZE] = pa as u64 | normal;
        } else {
            assert!(used < L2_POOL, "trop de gigaoctets troues");
            let t = &mut pool[used];
            used += 1;
            for (j, e) in t.0.iter_mut().enumerate() {
                let b = (pa + j * L2_SIZE) as u64;
                *e = if b < end as u64 && !in_hole(holes, b) { b | normal } else { 0 };
            }
            l1.0[pa / L1_SIZE] = t.addr() | VALID | TABLE;
        }
        pa += L1_SIZE;
    }

    arch::dsb_sy();
}

/// Active la MMU et les caches sur le coeur courant.
pub unsafe fn enable() {
    let l1_addr = unsafe { (*core::ptr::addr_of!(L1)).addr() };

    sysreg_write!("mair_el1", MAIR_VALUE);

    // IPS pris dans ID_AA64MMFR0_EL1 pour coller au materiel.
    let pa_range = sysreg_read!("id_aa64mmfr0_el1") & 0xF;
    let tcr: u64 = (25 << 0)        // T0SZ : VA sur 39 bits
        | (0b00 << 14)              // TG0  : granule 4 KiB
        | (0b11 << 12)              // SH0  : partageable interne
        | (0b01 << 10)              // ORGN0: WB WA
        | (0b01 << 8)               // IRGN0: WB WA
        | (1 << 23)                 // EPD1 : pas de parcours via TTBR1
        | (pa_range << 32)          // IPS
        | (asid16() << 36)          // AS : ASID sur 16 bits
        | (25 << 16);               // T1SZ (inactif, valeur legale)
    sysreg_write!("tcr_el1", tcr);
    sysreg_write!("ttbr0_el1", l1_addr);

    arch::isb();
    unsafe { core::arch::asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack)) };

    // SCTLR_EL1 : MMU (M) + cache donnees (C) + cache instructions (I)
    let mut sctlr = sysreg_read!("sctlr_el1");
    sctlr |= (1 << 0) | (1 << 2) | (1 << 12);
    sctlr &= !(1 << 1); // A : pas de verification d'alignement stricte
    sysreg_write!("sctlr_el1", sctlr);
    arch::isb();
}

/// Nombre d'entrees de niveau 1 reservees au noyau (vue identite 0-16 GiB,
/// assez pour 8 GiB de DRAM), recopiees dans chaque espace d'adressage
/// utilisateur. `nxk_abi::layout::USER_MIN` commence juste apres.
pub const KERNEL_L1_ENTRIES: usize = 16;
const _: () = assert!((KERNEL_L1_ENTRIES as u64) << 30 == nxk_abi::layout::USER_MIN);

/// ASID sur 16 bits si le coeur les gere (Cortex-A57 : oui).
fn asid16() -> u64 {
    u64::from((sysreg_read!("id_aa64mmfr0_el1") >> 4) & 0xF == 2)
}

/// Adresse physique de la table de niveau 1 du noyau.
pub fn boot_l1() -> u64 {
    unsafe { (*core::ptr::addr_of!(L1)).addr() }
}

/// Valeur de TTBR0_EL1 du noyau seul (ASID 0), utilisee par les coeurs au repos.
pub fn kernel_ttbr() -> u64 {
    boot_l1()
}

/// Invalide toute la TLB du coeur courant.
pub fn flush_tlb() {
    unsafe { core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb", options(nostack)) };
}

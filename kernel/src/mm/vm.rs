//! Espaces d'adressage utilisateur.
//!
//! Chaque processus a sa propre table de niveau 1. Ses 8 premieres entrees sont
//! celles du noyau (vue identite de la memoire, marquees globales et reservees a
//! EL1) : elles pointent vers les memes tables de niveau 2, partagees par tous.
//! Le reste appartient au processus, avec des entrees non globales etiquetees
//! par son ASID. Changer de processus revient a ecrire TTBR0_EL1 : aucune purge
//! de TLB, les traductions des differents processus y cohabitent.

use nxk_abi::err::{EEXIST, ENOMEM};
use nxk_abi::prot;

use crate::arch::mmu::{
    attr, AF, ATTR_DEVICE_NGNRE, ATTR_NORMAL_NC, ATTR_NORMAL_WB, ENTRIES, KERNEL_L1_ENTRIES, PXN, SH_INNER,
    TABLE, UXN, VALID,
};
use crate::mm::pmm;

const NG: u64 = 1 << 11;
const AP_RW_ALL: u64 = 0b01 << 6;
const AP_RO_ALL: u64 = 0b11 << 6;
/// Bit logiciel (ignore par le materiel) : page possedee par le processus,
/// rendue a l'allocateur quand l'espace d'adressage disparait.
pub const SW_OWNED: u64 = 1 << 55;
/// Bit logiciel d'une entree invalide : page reservee, allouee au premier
/// acces. Le materiel ignore tous les bits d'une entree dont VALID vaut 0.
pub const SW_LAZY: u64 = 1 << 56;
/// Entree rendue inaccessible par `mem_protect` (droits nuls) : VALID a 0,
/// mais l'adresse physique et les attributs restent, le contenu est garde.
pub const SW_NOACCESS: u64 = 1 << 57;
/// Page d'un objet de memoire partagee : jamais liberee par une projection.
pub const SW_SHM: u64 = 1 << 58;
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Champs de droits d'une entree : AP[2:1], PXN, UXN.
const PERM_MASK: u64 = (0b11 << 6) | PXN | UXN;

type Table = [u64; ENTRIES];

#[inline(always)]
fn table(pa: u64) -> &'static mut Table {
    unsafe { &mut *(pa as *mut Table) }
}

/// Attributs d'une page utilisateur. W^X est garanti par l'appelant.
pub fn user_flags(p: u64) -> u64 {
    VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_WB) | NG | perm_bits(p)
}

/// Page utilisateur non cachee (Normal-NC), pour les tampons DMA.
pub fn user_flags_nc(p: u64) -> u64 {
    VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_NC) | NG | perm_bits(p)
}

fn perm_bits(p: u64) -> u64 {
    let mut f = PXN;
    f |= if p & prot::WRITE != 0 { AP_RW_ALL } else { AP_RO_ALL };
    if p & prot::EXEC == 0 {
        f |= UXN;
    }
    f
}

/// Entree de registres de peripherique ?
pub fn is_device(pte: u64) -> bool {
    pte & VALID != 0 && (pte >> 2) & 7 == ATTR_DEVICE_NGNRE
}

/// Page physique possedee par le processus derriere cette entree (a rendre
/// a l'allocateur quand elle disparait).
pub fn owned_pa(pte: u64) -> Option<u64> {
    let present = pte & VALID != 0 || (pte & SW_NOACCESS != 0 && pte & SW_LAZY == 0);
    (present && pte & SW_OWNED != 0).then_some(pte & ADDR_MASK)
}

/// Attributs d'une page de registres de peripherique.
pub fn device_flags() -> u64 {
    VALID | TABLE | AF | attr(ATTR_DEVICE_NGNRE) | NG | AP_RW_ALL | PXN | UXN
}

/// Nouvelle table de niveau 1, deja pourvue des entrees du noyau.
pub fn create() -> Option<u64> {
    let l1 = pmm::alloc_zeroed(0)?;
    let boot = table(crate::arch::mmu::boot_l1());
    table(l1)[..KERNEL_L1_ENTRIES].copy_from_slice(&boot[..KERNEL_L1_ENTRIES]);
    Some(l1)
}

#[inline(always)]
fn indices(va: u64) -> (usize, usize, usize) {
    (
        ((va >> 30) & 0x1FF) as usize,
        ((va >> 21) & 0x1FF) as usize,
        ((va >> 12) & 0x1FF) as usize,
    )
}

fn next_level(entry: &mut u64, create: bool) -> Option<u64> {
    if *entry & VALID != 0 {
        return Some(*entry & ADDR_MASK);
    }
    if !create {
        return None;
    }
    let t = pmm::alloc_zeroed(0)?;
    *entry = t | VALID | TABLE;
    Some(t)
}

/// Entree de niveau 3 couvrant `va`, en creant les tables intermediaires.
fn leaf(l1: u64, va: u64, create: bool) -> Option<&'static mut u64> {
    let (i1, i2, i3) = indices(va);
    debug_assert!(i1 >= KERNEL_L1_ENTRIES, "adresse dans la zone du noyau");
    let l2 = next_level(&mut table(l1)[i1], create)?;
    let l3 = next_level(&mut table(l2)[i2], create)?;
    Some(&mut table(l3)[i3])
}

/// Projette une page. Echoue si l'adresse est deja occupee.
pub fn map(l1: u64, va: u64, pa: u64, flags: u64) -> Result<(), i64> {
    let e = leaf(l1, va, true).ok_or(-ENOMEM)?;
    if *e & (VALID | SW_LAZY | SW_NOACCESS) != 0 {
        return Err(-EEXIST);
    }
    *e = (pa & ADDR_MASK) | flags;
    Ok(())
}

/// Reserve une page sans l'allouer : la premiere faute la peuplera.
pub fn reserve(l1: u64, va: u64, flags: u64) -> Result<(), i64> {
    let e = leaf(l1, va, true).ok_or(-ENOMEM)?;
    if *e & (VALID | SW_LAZY | SW_NOACCESS) != 0 {
        return Err(-EEXIST);
    }
    *e = (flags & !VALID & !ADDR_MASK) | SW_LAZY;
    Ok(())
}

/// Peuple une page reservee (faute de traduction sur `va`). Rend vrai si la
/// faute est resolue ; faux si l'adresse n'etait pas reservee.
pub fn populate(l1: u64, va: u64) -> Result<bool, i64> {
    let e = match leaf(l1, va, false) {
        Some(e) => e,
        None => return Ok(false),
    };
    if *e & VALID != 0 || *e & SW_LAZY == 0 || *e & SW_NOACCESS != 0 {
        return Ok(false);
    }
    let pa = pmm::alloc_zeroed(0).ok_or(-ENOMEM)?;
    if *e & UXN == 0 {
        // Page executable : aucune instruction perimee de ce cadre physique
        // ne doit rester dans le cache d'instructions.
        sync_icache(pa, 4096);
    }
    // L'entree etait invalide : rien a purger en TLB, une barriere suffit.
    *e = (*e & !SW_LAZY) | pa | VALID;
    publish();
    Ok(true)
}

/// Entree existante (projetee, reservee ou inaccessible) pour `va`.
pub fn is_mapped(l1: u64, va: u64) -> bool {
    lookup(l1, va) & (VALID | SW_LAZY | SW_NOACCESS) != 0
}

/// Change les droits d'une page deja projetee ou reservee. `p` = 0 : aucun
/// acces, contenu conserve. Rend l'ancienne entree (0 si rien n'etait la).
/// La TLB n'est pas purgee : a l'appelant (une purge par page ou par ASID).
pub fn protect(l1: u64, va: u64, p: u64) -> u64 {
    let Some(e) = leaf(l1, va, false) else { return 0 };
    let old = *e;
    if old & (VALID | SW_LAZY | SW_NOACCESS) == 0 {
        return 0;
    }
    let mut new = (old & !PERM_MASK & !SW_NOACCESS) | perm_bits(p);
    if p == 0 {
        new = (new & !VALID) | SW_NOACCESS;
    } else if new & SW_LAZY == 0 {
        new |= VALID;
    }
    if new & VALID != 0 && old & UXN != 0 && new & UXN == 0 {
        sync_icache(new & ADDR_MASK, 4096); // devient executable
    }
    *e = new;
    old
}

/// Purge la traduction d'une page de l'espace `asid` sur tous les coeurs.
pub fn flush_page(asid: u16, va: u64) {
    let arg = ((asid as u64) << 48) | ((va >> 12) & 0xFFF_FFFF_FFFF);
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vae1is, {}", "dsb ish", "isb", in(reg) arg, options(nostack))
    };
}

/// Rend visibles au parcours materiel les ecritures de tables precedentes.
#[inline]
pub fn publish() {
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
}

/// Retire une page ; renvoie l'ancienne entree. La TLB est purgee sur tous les
/// coeurs par une seule instruction diffusee (pas d'IPI necessaire).
pub fn unmap(l1: u64, asid: u16, va: u64) -> Option<u64> {
    let e = leaf(l1, va, false)?;
    let old = *e;
    if old & VALID == 0 {
        if old & (SW_LAZY | SW_NOACCESS) != 0 {
            *e = 0; // entree invalide : rien a purger en TLB
            return Some(old);
        }
        return None;
    }
    *e = 0;
    let arg = ((asid as u64) << 48) | ((va >> 12) & 0xFFF_FFFF_FFFF);
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vae1is, {}", "dsb ish", "isb", in(reg) arg, options(nostack))
    };
    Some(old)
}

/// Entree courante pour `va` (0 si absente).
pub fn lookup(l1: u64, va: u64) -> u64 {
    leaf(l1, va, false).map(|e| *e).unwrap_or(0)
}

pub fn pte_addr(pte: u64) -> u64 {
    pte & ADDR_MASK
}

/// Libere les pages possedees et toutes les tables, puis purge l'ASID.
pub fn destroy(l1: u64, asid: u16) -> u64 {
    let mut freed = 0;
    for i1 in KERNEL_L1_ENTRIES..ENTRIES {
        let e1 = table(l1)[i1];
        if e1 & VALID == 0 {
            continue;
        }
        let l2 = e1 & ADDR_MASK;
        for i2 in 0..ENTRIES {
            let e2 = table(l2)[i2];
            if e2 & VALID == 0 {
                continue;
            }
            let l3 = e2 & ADDR_MASK;
            for i3 in 0..ENTRIES {
                if let Some(pa) = owned_pa(table(l3)[i3]) {
                    pmm::free(pa, 0);
                    freed += 1;
                }
            }
            pmm::free(l3, 0);
        }
        pmm::free(l2, 0);
    }
    pmm::free(l1, 0);
    flush_asid(asid);
    freed
}

pub fn flush_asid(asid: u16) {
    let arg = (asid as u64) << 48;
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi aside1is, {}", "dsb ish", "isb", in(reg) arg, options(nostack))
    };
}

/// Rend du code fraichement ecrit executable : nettoie le cache de donnees
/// jusqu'au point d'unification puis invalide le cache d'instructions.
pub fn sync_icache(pa: u64, len: u64) {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let dline = 4u64 << ((ctr >> 16) & 0xF);
    let mut a = pa & !(dline - 1);
    while a < pa + len {
        unsafe { core::arch::asm!("dc cvau, {}", in(reg) a, options(nostack)) };
        a += dline;
    }
    unsafe { core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack)) };
}

//! Per-process L1 tables sharing kernel entries; ASID-tagged, no flush.
//! W^X is the caller's job; TLB flushes stay with the caller.

use nxk_abi::err::{EEXIST, ENOMEM};
use nxk_abi::prot;

use crate::arch::mmu::{
    attr, AF, ATTR_DEVICE_NGNRE, ATTR_NORMAL_NC, ATTR_NORMAL_WB, ENTRIES, KERNEL_L1_ENTRIES, PXN,
    SH_INNER, TABLE, UXN, VALID,
};
use crate::mm::pmm;

const NG: u64 = 1 << 11;
const AP_RW_ALL: u64 = 0b01 << 6;
const AP_RO_ALL: u64 = 0b11 << 6;
/// Software bit (hardware-ignored): process-owned page, returned to the
/// allocator when the address space dies.
pub const SW_OWNED: u64 = 1 << 55;
/// Software bit of an invalid entry: reserved page, allocated on first
/// touch. Hardware ignores all bits of an entry with VALID clear.
pub const SW_LAZY: u64 = 1 << 56;
/// Entry made inaccessible by `mem_protect` (no rights): VALID clear, but
/// physical address and attributes kept, contents preserved.
pub const SW_NOACCESS: u64 = 1 << 57;
/// Shared-object page: never freed by a mapping.
pub const SW_SHM: u64 = 1 << 58;
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Rights fields of an entry: AP[2:1], PXN, UXN.
const PERM_MASK: u64 = (0b11 << 6) | PXN | UXN;

type Table = [u64; ENTRIES];

#[inline(always)]
fn table(pa: u64) -> &'static mut Table {
    unsafe { &mut *(pa as *mut Table) }
}

/// User page attributes. W^X is the caller's job.
pub fn user_flags(p: u64) -> u64 {
    VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_WB) | NG | perm_bits(p)
}

/// Uncached user page (Normal-NC), for DMA buffers.
pub fn user_flags_nc(p: u64) -> u64 {
    VALID | TABLE | AF | SH_INNER | attr(ATTR_NORMAL_NC) | NG | perm_bits(p)
}

fn perm_bits(p: u64) -> u64 {
    let mut f = PXN;
    f |= if p & prot::WRITE != 0 {
        AP_RW_ALL
    } else {
        AP_RO_ALL
    };
    if p & prot::EXEC == 0 {
        f |= UXN;
    }
    f
}

/// Device-register entry?
pub fn is_device(pte: u64) -> bool {
    pte & VALID != 0 && (pte >> 2) & 7 == ATTR_DEVICE_NGNRE
}

/// Process-owned physical page behind this entry (to hand back when it goes
/// away).
pub fn owned_pa(pte: u64) -> Option<u64> {
    let present = pte & VALID != 0 || (pte & SW_NOACCESS != 0 && pte & SW_LAZY == 0);
    (present && pte & SW_OWNED != 0).then_some(pte & ADDR_MASK)
}

/// Device-register page attributes, read-only unless `write`.
pub fn device_flags(write: bool) -> u64 {
    let ap = if write { AP_RW_ALL } else { AP_RO_ALL };
    VALID | TABLE | AF | attr(ATTR_DEVICE_NGNRE) | NG | ap | PXN | UXN
}

/// New L1 table, prefilled with the kernel entries.
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

/// L3 entry covering `va`, creating intermediate tables.
fn leaf(l1: u64, va: u64, create: bool) -> Option<&'static mut u64> {
    let (i1, i2, i3) = indices(va);
    debug_assert!(i1 >= KERNEL_L1_ENTRIES, "address in the kernel area");
    let l2 = next_level(&mut table(l1)[i1], create)?;
    let l3 = next_level(&mut table(l2)[i2], create)?;
    Some(&mut table(l3)[i3])
}

/// Map a page. Fails if the address is taken.
pub fn map(l1: u64, va: u64, pa: u64, flags: u64) -> Result<(), i64> {
    let e = leaf(l1, va, true).ok_or(-ENOMEM)?;
    if *e & (VALID | SW_LAZY | SW_NOACCESS) != 0 {
        return Err(-EEXIST);
    }
    *e = (pa & ADDR_MASK) | flags;
    Ok(())
}

/// Reserve a page without allocating: first fault populates it.
pub fn reserve(l1: u64, va: u64, flags: u64) -> Result<(), i64> {
    let e = leaf(l1, va, true).ok_or(-ENOMEM)?;
    if *e & (VALID | SW_LAZY | SW_NOACCESS) != 0 {
        return Err(-EEXIST);
    }
    *e = (flags & !VALID & !ADDR_MASK) | SW_LAZY;
    Ok(())
}

/// Populate a reserved page (translation fault on `va`). True if the fault
/// is resolved; false if the address was not reserved.
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
        // Executable page: no stale instruction of this frame may linger in
        // the instruction cache.
        sync_icache(pa, 4096);
    }
    // The entry was invalid: nothing to flush, one barrier is enough.
    *e = (*e & !SW_LAZY) | pa | VALID;
    publish();
    Ok(true)
}

/// Existing entry (mapped, reserved or no-access) for `va`.
pub fn is_mapped(l1: u64, va: u64) -> bool {
    lookup(l1, va) & (VALID | SW_LAZY | SW_NOACCESS) != 0
}

/// Change page rights (`p` = 0 keeps contents, drops access).
/// Returns old entry; caller flushes TLB (per page or per ASID).
pub fn protect(l1: u64, va: u64, p: u64) -> u64 {
    let Some(e) = leaf(l1, va, false) else {
        return 0;
    };
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
        sync_icache(new & ADDR_MASK, 4096);
    }
    *e = new;
    old
}

/// Flush one page translation of `asid` space on all cores.
pub fn flush_page(asid: u16, va: u64) {
    let arg = ((asid as u64) << 48) | ((va >> 12) & 0xFFF_FFFF_FFFF);
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vae1is, {}", "dsb ish", "isb", in(reg) arg, options(nostack))
    };
}

/// Make earlier table writes visible to the hardware walker.
#[inline]
pub fn publish() {
    unsafe { core::arch::asm!("dsb ishst", options(nostack)) };
}

/// Unmap a page; return the old entry. The TLB flushes on all cores via one
/// broadcast instruction (no IPI needed).
pub fn unmap(l1: u64, asid: u16, va: u64) -> Option<u64> {
    let old = clear(l1, va)?;
    if old & VALID != 0 {
        flush_page(asid, va);
    }
    Some(old)
}

/// Remove the entry at `va` (mapped or reserved) with no TLB
/// maintenance: the caller flushes before reusing what it pointed to.
pub fn clear(l1: u64, va: u64) -> Option<u64> {
    let e = leaf(l1, va, false)?;
    let old = *e;
    if old & (VALID | SW_LAZY | SW_NOACCESS) == 0 {
        return None;
    }
    *e = 0;
    Some(old)
}

/// Was this entry live (possibly cached in a TLB)?
pub fn is_valid(pte: u64) -> bool {
    pte & VALID != 0
}

/// Current entry for `va` (0 if missing).
pub fn lookup(l1: u64, va: u64) -> u64 {
    leaf(l1, va, false).map(|e| *e).unwrap_or(0)
}

pub fn pte_addr(pte: u64) -> u64 {
    pte & ADDR_MASK
}

/// Free owned pages and all tables, then flush the ASID.
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

/// Make freshly written code executable: clean the data cache to the point
/// of unification then invalidate the instruction cache.
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

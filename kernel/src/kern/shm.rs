//! Shared-memory objects mapped into processes; DMA capable.
//! Each mapping holds a ref; CONTIGUOUS suits IOMMU-less devices.

use nxk_abi::err::{EACCES, EINVAL, ENOMEM};
use nxk_abi::layout::PAGE_SIZE;
use nxk_abi::{prot, shm as flags};

use crate::kern::bkl::{self, BklCell};
use crate::kern::objtab::Table;
use crate::kern::process;
use crate::kern::thread::NIL;
use crate::mm::pmm::clean_inval;
use crate::mm::{pmm, vm};

/// Max non-contiguous object size (1 GiB).
const MAX_PAGES: u64 = 1 << 18;

pub struct Shm {
    pub used: bool,
    pub free_link: u16,
    pub gen: u16,
    /// Capabilities and mappings.
    pub refs: u32,
    pub pages: u32,
    pub order: u8,
    pub flags: u64,
    /// Contiguous: block address; else: page address array.
    pub base: u64,
}

/// One mapping of an object in a process.
pub struct Mapping {
    pub free_link: u16,
    pub next: u16,
    pub shm: u16,
    pub va: u64,
    pub pages: u32,
    /// Pages still mapped; at zero the mapping goes away.
    pub mapped: u32,
    /// Max rights (the capability's at map time).
    pub max_prot: u64,
}

fn s_reset(x: &mut Shm) {
    x.used = false;
    x.refs = 0;
}

fn s_link(x: &mut Shm) -> &mut u16 {
    &mut x.free_link
}

fn m_reset(m: &mut Mapping) {
    m.next = NIL;
    m.shm = NIL;
}

fn m_link(m: &mut Mapping) -> &mut u16 {
    &mut m.free_link
}

static SHMS: BklCell<Table<Shm>> = BklCell::new(Table::new(s_reset, s_link));
static MAPS: BklCell<Table<Mapping>> = BklCell::new(Table::new(m_reset, m_link));

#[inline(always)]
fn s(i: u16) -> &'static mut Shm {
    SHMS.get().get(i)
}

pub fn alive(i: u16, gen: u16) -> bool {
    (i as usize) < SHMS.get().capacity() && s(i).used && s(i).gen == gen
}

/// Create a `len`-byte object (page-rounded).
pub fn create(len: u64, fl: u64) -> Result<(u16, u16), i64> {
    if len == 0 || fl & !(flags::CONTIGUOUS | flags::UNCACHED | flags::DMA32) != 0 {
        return Err(-EINVAL);
    }
    let pages = pmm::align_up(len, PAGE_SIZE) / PAGE_SIZE;
    // Not zeroed here: `fill` does it, letting other cores in.
    let alloc = |order: usize| {
        if fl & flags::DMA32 != 0 {
            pmm::alloc_below(order, 1 << 32)
        } else {
            pmm::alloc(order)
        }
    };
    let (base, order) = if fl & flags::CONTIGUOUS != 0 {
        let order = pmm::order_for(len);
        if order >= pmm::MAX_ORDER {
            return Err(-EINVAL);
        }
        (alloc(order).ok_or(-ENOMEM)?, order)
    } else {
        if pages > MAX_PAGES {
            return Err(-EINVAL);
        }
        // Page array: every entry is written by `fill`.
        let order = pmm::order_for(pages * 8);
        (pmm::alloc(order).ok_or(-ENOMEM)?, order)
    };
    let mut ready = 0;
    if let Err(e) = fill(base, fl, pages, &mut ready, alloc) {
        release_pages(base, order, fl, ready as u32);
        return Err(e);
    }
    let Some(i) = SHMS.get().alloc() else {
        release_pages(base, order, fl, pages as u32);
        return Err(-ENOMEM);
    };
    let x = s(i);
    x.used = true;
    x.refs = 0;
    x.pages = pages as u32;
    x.order = order as u8;
    x.flags = fl;
    x.base = base;
    Ok((i, x.gen))
}

/// Zero the object's pages (allocating them one by one if not
/// contiguous), cleaned to memory if uncached. The global lock is let
/// go every `BREATHE_PAGES`: nothing else sees the object yet.
/// `ready`: pages allocated so far (non-contiguous).
fn fill(
    base: u64,
    fl: u64,
    pages: u64,
    ready: &mut u64,
    alloc: impl Fn(usize) -> Option<u64>,
) -> Result<(), i64> {
    for k in 0..pages {
        if k > 0 && k % bkl::BREATHE_PAGES == 0 {
            bkl::breathe()?;
        }
        let pa = if fl & flags::CONTIGUOUS != 0 {
            base + k * PAGE_SIZE
        } else {
            let pa = alloc(0).ok_or(-ENOMEM)?;
            unsafe { *(base as *mut u64).add(k as usize) = pa };
            *ready = k + 1;
            pa
        };
        pmm::zero(pa, PAGE_SIZE);
        if fl & flags::UNCACHED != 0 {
            clean_inval(pa, PAGE_SIZE);
        }
    }
    Ok(())
}

fn release_pages(base: u64, order: usize, fl: u64, pages: u32) {
    if fl & flags::CONTIGUOUS == 0 {
        for k in 0..pages as usize {
            pmm::free(unsafe { *(base as *const u64).add(k) }, 0);
        }
    }
    pmm::free(base, order);
}

/// Physical address of page `k`.
#[inline]
pub fn page_pa(i: u16, k: u32) -> u64 {
    let x = s(i);
    if x.flags & flags::CONTIGUOUS != 0 {
        x.base + k as u64 * PAGE_SIZE
    } else {
        unsafe { *(x.base as *const u64).add(k as usize) }
    }
}

pub fn size(i: u16) -> u64 {
    s(i).pages as u64 * PAGE_SIZE
}

pub fn incref(i: u16) {
    s(i).refs += 1;
}

pub fn decref(i: u16) {
    let x = s(i);
    x.refs -= 1;
    if x.refs == 0 {
        release_pages(x.base, x.order as usize, x.flags, x.pages);
        x.used = false;
        x.gen = x.gen.wrapping_add(1);
        SHMS.get().free(i);
    }
}

/// Physical address of an object byte (DMA setup).
pub fn paddr(i: u16, offset: u64) -> Result<u64, i64> {
    if offset >= size(i) {
        return Err(-EINVAL);
    }
    Ok(page_pa(i, (offset / PAGE_SIZE) as u32) + offset % PAGE_SIZE)
}

/// Map the whole object at `va` in process `pi`.
pub fn map(pi: u16, i: u16, va: u64, p: u64, max_prot: u64) -> Result<(), i64> {
    let x = s(i);
    let pr = process::get(pi);
    let base_flags = if x.flags & flags::UNCACHED != 0 {
        vm::user_flags_nc(p)
    } else {
        vm::user_flags(p)
    };
    let fl = base_flags | vm::SW_SHM;
    let m = MAPS.get().alloc().ok_or(-ENOMEM)?;
    for k in 0..x.pages {
        let pa = page_pa(i, k);
        if let Err(e) = vm::map(pr.l1, va + k as u64 * PAGE_SIZE, pa, fl) {
            for j in 0..k {
                vm::unmap(pr.l1, pr.asid, va + j as u64 * PAGE_SIZE);
            }
            MAPS.get().free(m);
            return Err(e);
        }
        if p & prot::EXEC != 0 {
            vm::sync_icache(pa, PAGE_SIZE);
        }
    }
    vm::publish();
    let mp = MAPS.get().get(m);
    mp.shm = i;
    mp.va = va;
    mp.pages = x.pages;
    mp.mapped = x.pages;
    mp.max_prot = max_prot;
    mp.next = pr.shm_head;
    pr.shm_head = m;
    x.refs += 1;
    Ok(())
}

/// Mapping of process `pi` containing `va`.
fn find(pi: u16, va: u64) -> Option<(u16, u16)> {
    let mut prev = NIL;
    let mut cur = process::get(pi).shm_head;
    while cur != NIL {
        let m = MAPS.get().get(cur);
        if va >= m.va && va < m.va + m.pages as u64 * PAGE_SIZE {
            return Some((prev, cur));
        }
        prev = cur;
        cur = m.next;
    }
    None
}

/// Max rights of a shared page mapped at `va`.
pub fn max_prot(pi: u16, va: u64) -> u64 {
    find(pi, va)
        .map(|(_, m)| MAPS.get().get(m).max_prot)
        .unwrap_or(0)
}

/// A shared page was just removed from `pi`'s space.
pub fn page_unmapped(pi: u16, va: u64) {
    let Some((prev, cur)) = find(pi, va) else {
        return;
    };
    let m = MAPS.get().get(cur);
    m.mapped -= 1;
    if m.mapped == 0 {
        let next = m.next;
        if prev == NIL {
            process::get(pi).shm_head = next;
        } else {
            MAPS.get().get(prev).next = next;
        }
        let obj = m.shm;
        MAPS.get().free(cur);
        decref(obj);
    }
}

/// Process exit: page tables already destroyed, still must drop the
/// mappings' references.
pub fn drop_mappings(pi: u16) {
    let p = process::get(pi);
    while p.shm_head != NIL {
        let cur = p.shm_head;
        let m = MAPS.get().get(cur);
        p.shm_head = m.next;
        let obj = m.shm;
        MAPS.get().free(cur);
        decref(obj);
    }
}

/// Requested rights within the capability?
pub fn check_prot(p: u64, cap_rights: u16) -> Result<u64, i64> {
    let mut max = prot::READ | prot::EXEC;
    if cap_rights & nxk_abi::rights::WRITE != 0 {
        max |= prot::WRITE;
    }
    if p & !max != 0 {
        return Err(-EACCES);
    }
    Ok(max)
}

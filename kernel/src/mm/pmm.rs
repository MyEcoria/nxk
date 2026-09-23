//! Buddy page allocator (4 KiB-4 MiB blocks), one metadata bit per page.

use core::ptr;

use crate::kern::bkl::BklCell;

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_ORDER: usize = 11;
const MAX_BLOCK: u64 = PAGE_SIZE << (MAX_ORDER - 1);

/// Free-block header, stored in the block.
#[repr(C)]
struct FreeBlock {
    next: u64,
    prev: u64,
    order: u64,
}

pub struct Pmm {
    /// Physical address of index page 0 (MAX_BLOCK-aligned).
    base: u64,
    nframes: usize,
    /// One bit per page: start of a free block.
    meta: *mut u64,
    heads: [u64; MAX_ORDER],
    pub free_pages: u64,
    pub total_pages: u64,
    pub kernel_pages: u64,
}

pub static PMM: BklCell<Pmm> = BklCell::new(Pmm {
    base: 0,
    nframes: 0,
    meta: ptr::null_mut(),
    heads: [0; MAX_ORDER],
    free_pages: 0,
    total_pages: 0,
    kernel_pages: 0,
});

#[inline(always)]
fn block(pa: u64) -> *mut FreeBlock {
    pa as *mut FreeBlock
}

impl Pmm {
    #[inline(always)]
    fn idx(&self, pa: u64) -> usize {
        ((pa - self.base) / PAGE_SIZE) as usize
    }

    #[inline(always)]
    fn is_free_head(&self, pa: u64) -> bool {
        let i = self.idx(pa);
        unsafe { *self.meta.add(i / 64) & (1 << (i % 64)) != 0 }
    }

    #[inline(always)]
    fn set_free_head(&mut self, pa: u64, on: bool) {
        let i = self.idx(pa);
        unsafe {
            let w = &mut *self.meta.add(i / 64);
            if on {
                *w |= 1 << (i % 64);
            } else {
                *w &= !(1 << (i % 64));
            }
        }
    }

    fn push(&mut self, pa: u64, order: usize) {
        let head = self.heads[order];
        unsafe {
            (*block(pa)).next = head;
            (*block(pa)).prev = 0;
            (*block(pa)).order = order as u64;
            if head != 0 {
                (*block(head)).prev = pa;
            }
        }
        self.heads[order] = pa;
        self.set_free_head(pa, true);
    }

    fn unlink(&mut self, pa: u64, order: usize) {
        unsafe {
            let b = &*block(pa);
            if b.prev != 0 {
                (*block(b.prev)).next = b.next;
            } else {
                self.heads[order] = b.next;
            }
            if b.next != 0 {
                (*block(b.next)).prev = b.prev;
            }
        }
        self.set_free_head(pa, false);
    }

    /// Allocate 2^order contiguous pages, aligned to their size.
    pub fn alloc(&mut self, order: usize) -> Option<u64> {
        let mut o = order;
        while o < MAX_ORDER && self.heads[o] == 0 {
            o += 1;
        }
        if o >= MAX_ORDER {
            return None;
        }
        let pa = self.heads[o];
        self.unlink(pa, o);
        // Split down to the wanted size; upper halves go back to the lists.
        while o > order {
            o -= 1;
            self.push(pa + (PAGE_SIZE << o), o);
        }
        self.free_pages -= 1 << order;
        Some(pa)
    }

    /// Like `alloc`, but only a block below `limit`: the free lists are
    /// walked (rare allocation, for 32-bit DMA buffers).
    pub fn alloc_below(&mut self, order: usize, limit: u64) -> Option<u64> {
        for o in order..MAX_ORDER {
            let mut pa = self.heads[o];
            while pa != 0 {
                if pa + (PAGE_SIZE << o) <= limit {
                    self.unlink(pa, o);
                    let mut k = o;
                    while k > order {
                        k -= 1;
                        self.push(pa + (PAGE_SIZE << k), k);
                    }
                    self.free_pages -= 1 << order;
                    return Some(pa);
                }
                pa = unsafe { (*block(pa)).next };
            }
        }
        None
    }

    /// Give a block back and merge it with free buddies.
    pub fn free(&mut self, mut pa: u64, mut order: usize) {
        debug_assert!(pa >= self.base && self.idx(pa) < self.nframes);
        self.free_pages += 1 << order;
        while order < MAX_ORDER - 1 {
            let buddy = self.base + (((pa - self.base) / PAGE_SIZE) ^ (1 << order)) * PAGE_SIZE;
            if self.idx(buddy) >= self.nframes
                || !self.is_free_head(buddy)
                || unsafe { (*block(buddy)).order } != order as u64
            {
                break;
            }
            self.unlink(buddy, order);
            pa = pa.min(buddy);
            order += 1;
        }
        self.push(pa, order);
    }
}

/// Make `[start, end)` available, after reserving the metadata array there,
/// minus `holes` (secure-monitor reserved memory, 2 MiB-aligned).
pub fn init(start: u64, end: u64, holes: &[(u64, u64)]) {
    let p = PMM.get();
    let base = start & !(MAX_BLOCK - 1);
    let nframes = ((end - base) / PAGE_SIZE) as usize;

    // Metadata at the start of the free range, one bit per indexed page.
    let meta_pa = align_up(start, PAGE_SIZE);
    let meta_len = align_up((nframes as u64).div_ceil(8), PAGE_SIZE);
    unsafe { ptr::write_bytes(meta_pa as *mut u8, 0, meta_len as usize) };

    p.base = base;
    p.nframes = nframes;
    p.meta = meta_pa as *mut u64;
    p.total_pages = (end - base) / PAGE_SIZE;
    p.kernel_pages = (meta_pa + meta_len - base) / PAGE_SIZE;

    // Cut each free range into maximal correctly-aligned blocks.
    let mut from = meta_pa + meta_len;
    loop {
        // Next hole past `from`.
        let next = holes
            .iter()
            .filter(|h| h.1 > from)
            .map(|h| h.0.max(from))
            .min()
            .unwrap_or(end)
            .min(end);
        add_range(p, from, next);
        if next >= end {
            break;
        }
        let hole_end = holes
            .iter()
            .filter(|h| h.0 <= next && h.1 > next)
            .map(|h| h.1)
            .max()
            .unwrap_or(next);
        p.total_pages -= (hole_end.min(end) - next) / PAGE_SIZE;
        from = hole_end;
        if from >= end {
            break;
        }
    }
}

fn add_range(p: &mut Pmm, mut pa: u64, end: u64) {
    while pa < end {
        let mut order = MAX_ORDER - 1;
        while order > 0 && (pa & ((PAGE_SIZE << order) - 1) != 0 || pa + (PAGE_SIZE << order) > end)
        {
            order -= 1;
        }
        p.push(pa, order);
        p.free_pages += 1 << order;
        pa += PAGE_SIZE << order;
    }
}

pub const fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

/// Minimum order covering `bytes`.
pub fn order_for(bytes: u64) -> usize {
    let pages = align_up(bytes.max(1), PAGE_SIZE) / PAGE_SIZE;
    (64 - (pages - 1).leading_zeros()) as usize
}

pub fn alloc(order: usize) -> Option<u64> {
    PMM.get().alloc(order)
}

pub fn free(pa: u64, order: usize) {
    PMM.get().free(pa, order)
}

/// Block fully below `limit` (32-bit DMA buffers), not zeroed.
pub fn alloc_below(order: usize, limit: u64) -> Option<u64> {
    PMM.get().alloc_below(order, limit)
}

/// Zeroed page, ready to hand to a process.
pub fn alloc_zeroed(order: usize) -> Option<u64> {
    let pa = alloc(order)?;
    zero(pa, PAGE_SIZE << order);
    Some(pa)
}

/// Zero a cache-line-aligned range with `DC ZVA`, filling a whole line per
/// instruction without reading it first.
pub fn zero(pa: u64, len: u64) {
    let dczid: u64;
    unsafe { core::arch::asm!("mrs {}, dczid_el0", out(reg) dczid, options(nomem, nostack)) };
    if dczid & (1 << 4) != 0 {
        unsafe { ptr::write_bytes(pa as *mut u8, 0, len as usize) };
        return;
    }
    let line = 4u64 << (dczid & 0xF);
    let mut a = pa;
    let end = pa + len;
    while a < end {
        unsafe { core::arch::asm!("dc zva, {}", in(reg) a, options(nostack)) };
        a += line;
    }
}

/// Clean+invalidate cache lines over an identity-mapped range, so kernel
/// zeroing reaches memory before any uncached or DMA access.
pub fn clean_inval(pa: u64, len: u64) {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = 4u64 << ((ctr >> 16) & 0xF);
    let mut a = pa & !(line - 1);
    while a < pa + len {
        unsafe { core::arch::asm!("dc civac, {}", in(reg) a, options(nostack)) };
        a += line;
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack)) };
}

pub fn stats() -> (u64, u64, u64) {
    let p = PMM.get();
    (p.total_pages, p.free_pages, p.kernel_pages)
}

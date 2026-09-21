//! Allocateur de pages physiques : systeme buddy.
//!
//! La DRAM libre est decoupee en blocs de 2^n pages (4 KiB a 4 MiB). Allouer
//! prend le plus petit bloc suffisant et le scinde ; liberer refusionne un bloc
//! avec son « copain » tant que celui-ci est libre. Les deux operations sont en
//! O(ordre max), sans parcours de la memoire.
//!
//! Les listes libres sont chainees *dans* les blocs libres eux-memes (le noyau
//! voit toute la DRAM en identite), qui portent aussi leur ordre. Il ne reste
//! qu'un bit de metadonnees par page : « debut d'un bloc libre ». Pour 4 GiB,
//! 128 KiB de metadonnees au lieu de 1 MiB avec un octet par page.

use core::ptr;

use crate::kern::bkl::BklCell;

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_ORDER: usize = 11; // ordres 0..10 : 4 KiB .. 4 MiB
const MAX_BLOCK: u64 = PAGE_SIZE << (MAX_ORDER - 1);

/// En-tete d'un bloc libre, stocke dans le bloc.
#[repr(C)]
struct FreeBlock {
    next: u64,
    prev: u64,
    order: u64,
}

pub struct Pmm {
    /// Adresse physique de la page 0 de l'index (alignee sur MAX_BLOCK).
    base: u64,
    nframes: usize,
    /// Un bit par page : debut d'un bloc libre.
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

    /// Alloue 2^order pages contigues, alignees sur leur taille.
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
        // Scinde jusqu'a la taille demandee ; les moities hautes retournent
        // dans les listes.
        while o > order {
            o -= 1;
            self.push(pa + (PAGE_SIZE << o), o);
        }
        self.free_pages -= 1 << order;
        Some(pa)
    }

    /// Comme `alloc`, mais seulement un bloc situe sous `limit` : les listes
    /// libres sont parcourues (allocation rare, pour les tampons DMA 32 bits).
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

    /// Rend un bloc et le fusionne avec ses copains libres.
    pub fn free(&mut self, mut pa: u64, mut order: usize) {
        debug_assert!(pa >= self.base && self.idx(pa) < self.nframes);
        self.free_pages += 1 << order;
        while order < MAX_ORDER - 1 {
            let buddy = self.base + (((pa - self.base) / PAGE_SIZE) as u64 ^ (1 << order)) * PAGE_SIZE;
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

/// Met la plage `[start, end)` a disposition, apres y avoir reserve le
/// tableau de metadonnees, sauf les zones `holes` (memoire reservee a un
/// moniteur securise, alignee sur 2 MiB).
pub fn init(start: u64, end: u64, holes: &[(u64, u64)]) {
    let p = PMM.get();
    let base = start & !(MAX_BLOCK - 1);
    let nframes = ((end - base) / PAGE_SIZE) as usize;

    // Metadonnees au debut de la zone libre, un bit par page indexee.
    let meta_pa = align_up(start, PAGE_SIZE);
    let meta_len = align_up((nframes as u64).div_ceil(8), PAGE_SIZE);
    unsafe { ptr::write_bytes(meta_pa as *mut u8, 0, meta_len as usize) };

    p.base = base;
    p.nframes = nframes;
    p.meta = meta_pa as *mut u64;
    p.total_pages = (end - base) / PAGE_SIZE;
    p.kernel_pages = (meta_pa + meta_len - base) / PAGE_SIZE;

    // Decoupe chaque zone libre en blocs maximaux correctement alignes.
    let mut from = meta_pa + meta_len;
    loop {
        // Prochain trou au-dela de `from`.
        let next = holes.iter().filter(|h| h.1 > from).map(|h| h.0.max(from)).min().unwrap_or(end).min(end);
        add_range(p, from, next);
        if next >= end {
            break;
        }
        let hole_end = holes.iter().filter(|h| h.0 <= next && h.1 > next).map(|h| h.1).max().unwrap_or(next);
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
        while order > 0 && (pa & ((PAGE_SIZE << order) - 1) != 0 || pa + (PAGE_SIZE << order) > end) {
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

/// Ordre minimal couvrant `bytes`.
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

/// Bloc entierement sous `limit` (tampons DMA 32 bits), mis a zero.
pub fn alloc_zeroed_below(order: usize, limit: u64) -> Option<u64> {
    let pa = PMM.get().alloc_below(order, limit)?;
    zero(pa, PAGE_SIZE << order);
    Some(pa)
}

/// Page mise a zero, prete a etre donnee a un processus.
pub fn alloc_zeroed(order: usize) -> Option<u64> {
    let pa = alloc(order)?;
    zero(pa, PAGE_SIZE << order);
    Some(pa)
}

/// Met a zero une zone alignee en ligne de cache avec `DC ZVA`, qui remplit une
/// ligne entiere par instruction sans la lire au prealable.
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

pub fn stats() -> (u64, u64, u64) {
    let p = PMM.get();
    (p.total_pages, p.free_pages, p.kernel_pages)
}

//! Size-class allocator (36 classes to 16 KiB, 64 KiB runs; O(1), headerless) plus direct-mapped large blocks.
//! Fresh runs cost nothing until touched; freed addresses are reused via `pal::nxk::vm`.

use crate::alloc::{GlobalAlloc, Layout, System};
use crate::cell::UnsafeCell;
use crate::ptr;
use crate::sys::pal::vm::{self, PAGE};
use crate::sys::sync::Mutex;

const SMALL_CLASSES: usize = 8;
const CLASSES: usize = 36;
const MAX_SMALL: usize = 16 * 1024;
const RUN: usize = 64 * 1024;
const CHUNK: usize = 1024 * 1024;

/// Class of a size (1..=MAX_SMALL).
#[inline]
fn class_of(size: usize) -> usize {
    if size <= 128 {
        (size.max(1) + 15) / 16 - 1
    } else {
        // size in ]2^k, 2^(k+1)]: four classes of step 2^(k-2).
        let k = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize;
        SMALL_CLASSES + (k - 7) * 4 + ((size - 1 - (1 << k)) >> (k - 2))
    }
}

#[inline]
fn class_size(c: usize) -> usize {
    if c < SMALL_CLASSES {
        (c + 1) * 16
    } else {
        let k = (c - SMALL_CLASSES) / 4 + 7;
        (1 << k) + (((c - SMALL_CLASSES) % 4 + 1) << (k - 2))
    }
}

/// Effective size of a request: past 16-byte alignment, a power of two
/// (class blocks are aligned to it, runs to the page).
#[inline]
fn effective_size(l: &Layout) -> usize {
    if l.align() <= 16 { l.size() } else { l.size().max(l.align()).next_power_of_two() }
}

#[inline]
fn is_small(l: &Layout) -> bool {
    l.align() <= PAGE && effective_size(l) <= MAX_SMALL
}

struct Heap {
    free: [*mut u8; CLASSES],
    run_cur: [usize; CLASSES],
    run_end: [usize; CLASSES],
    chunk_cur: usize,
    chunk_end: usize,
}

struct Global {
    lock: Mutex,
    heap: UnsafeCell<Heap>,
}

// SAFETY: `heap` only touched under `lock`.
unsafe impl Sync for Global {}

static GLOBAL: Global = Global {
    lock: Mutex::new(),
    heap: UnsafeCell::new(Heap {
        free: [ptr::null_mut(); CLASSES],
        run_cur: [0; CLASSES],
        run_end: [0; CLASSES],
        chunk_cur: 0,
        chunk_end: 0,
    }),
};

/// Block of class `c`; the bool says if it is fresh (hence zeroed).
fn small_alloc(c: usize) -> (*mut u8, bool) {
    let size = class_size(c);
    GLOBAL.lock.lock();
    // SAFETY: exclusive access under the lock.
    let h = unsafe { &mut *GLOBAL.heap.get() };
    let head = h.free[c];
    let r = if !head.is_null() {
        // SAFETY: a free block links to the next.
        h.free[c] = unsafe { *(head as *mut *mut u8) };
        (head, false)
    } else {
        if h.run_cur[c] + size > h.run_end[c] {
            if h.chunk_cur + RUN > h.chunk_end {
                match vm::map(CHUNK) {
                    Some(p) => {
                        h.chunk_cur = p as usize;
                        h.chunk_end = p as usize + CHUNK;
                    }
                    None => {
                        unsafe { GLOBAL.lock.unlock() };
                        return (ptr::null_mut(), false);
                    }
                }
            }
            h.run_cur[c] = h.chunk_cur;
            h.run_end[c] = h.chunk_cur + RUN;
            h.chunk_cur += RUN;
        }
        let p = h.run_cur[c];
        h.run_cur[c] += size;
        (p as *mut u8, true)
    };
    unsafe { GLOBAL.lock.unlock() };
    r
}

fn small_free(p: *mut u8, c: usize) {
    GLOBAL.lock.lock();
    // SAFETY: exclusive access under the lock; the free block holds the link.
    unsafe {
        let h = &mut *GLOBAL.heap.get();
        *(p as *mut *mut u8) = h.free[c];
        h.free[c] = p;
        GLOBAL.lock.unlock();
    }
}

fn large_len(l: &Layout) -> usize {
    l.size().next_multiple_of(PAGE)
}

/// Large block: fresh (zeroed) pages plus a guard page.
fn large_alloc(l: &Layout) -> *mut u8 {
    let len = large_len(l);
    if l.align() <= PAGE {
        return vm::map(len).unwrap_or(ptr::null_mut());
    }
    // Over-page alignment: wider mapping, head and tail returned.
    let Some(total) = len.checked_add(l.align()) else { return ptr::null_mut() };
    let Some(base) = vm::map(total) else { return ptr::null_mut() };
    let base = base as usize;
    let aligned = base.next_multiple_of(l.align());
    let tail = total - (aligned - base) - len;
    // SAFETY: start of the mapping, unseen by anyone.
    unsafe { vm::release(base, aligned - base) };
    if tail > 0 {
        // The page past the block stays free off-list: its guard,
        // freed with it.
        crate::sys::pal::syscall::mem_unmap((aligned + len) as u64, tail as u64);
        vm::record(aligned + len + PAGE, tail);
    }
    aligned as *mut u8
}

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if is_small(&l) { small_alloc(class_of(effective_size(&l))).0 } else { large_alloc(&l) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if is_small(&l) {
            let c = class_of(effective_size(&l));
            let (p, fresh) = small_alloc(c);
            if !p.is_null() && !fresh {
                // SAFETY: `class_size(c)`-byte block.
                unsafe { ptr::write_bytes(p, 0, l.size()) };
            }
            p
        } else {
            large_alloc(&l)
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if is_small(&l) {
            small_free(p, class_of(effective_size(&l)));
        } else {
            // SAFETY: `large_alloc` block, no longer used.
            unsafe { vm::unmap(p, large_len(&l)) };
        }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same requirements as GlobalAlloc::realloc.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, l.align()) };
        match (is_small(&l), is_small(&new)) {
            // Same class: nothing to do.
            (true, true) if class_of(effective_size(&l)) == class_of(effective_size(&new)) => return p,
            (false, false) => {
                let (old_len, new_len) = (large_len(&l), large_len(&new));
                if new_len == old_len {
                    return p;
                }
                if new_len < old_len {
                    // Shrink: tail returned, one guard page kept.
                    let end = p as usize + new_len;
                    crate::sys::pal::syscall::mem_unmap(end as u64, (old_len - new_len) as u64);
                    vm::record(end + PAGE, old_len - new_len);
                    return p;
                }
            }
            _ => {}
        }
        // SAFETY: same requirements as GlobalAlloc::realloc.
        unsafe {
            let q = self.alloc(new);
            if !q.is_null() {
                ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                self.dealloc(p, l);
            }
            q
        }
    }
}

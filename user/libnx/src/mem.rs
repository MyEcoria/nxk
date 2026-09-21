//! Memoire : projections et allocateur global (`Vec`, `String`, `Box`...).
//!
//! Petites tailles : classes en puissances de 2 (16 o a 2 KiB). Chaque page est
//! decoupee entierement dans une seule classe, si bien que tout bloc est aligne
//! sur sa taille. Les blocs liberes forment une liste par classe : allocation
//! et liberation en O(1). Au-dela de 2 KiB, projection directe de pages.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use nxk_abi::layout::PAGE_SIZE;
use nxk_abi::prot;

use crate::sync::Mutex;
use crate::sys;

pub fn map(len: usize, p: u64) -> Result<*mut u8, i64> {
    sys::mem_map(0, len as u64, p).map(|a| a as *mut u8)
}

pub fn unmap(ptr: *mut u8, len: usize) -> Result<(), i64> {
    sys::mem_unmap(ptr as u64, len as u64)
}

pub fn protect(ptr: *mut u8, len: usize, p: u64) -> Result<(), i64> {
    sys::mem_protect(ptr as u64, len as u64, p)
}

/// Memoire partagee : un objet noyau et, eventuellement, sa projection ici.
/// Le handle peut etre transmis a un autre processus (IPC) qui la projette
/// a son tour ; les pages vivent tant qu'un handle ou une projection existe.
pub struct SharedMemory {
    handle: u32,
    len: usize,
    addr: u64,
}

impl SharedMemory {
    /// Nouvel objet de `len` octets, projete en lecture/ecriture.
    pub fn new(len: usize, flags: u64) -> Result<Self, i64> {
        let h = sys::shm_create(len as u64, flags)?;
        Self::map(h, len, prot::READ | prot::WRITE).map_err(|e| {
            let _ = sys::handle_close(h);
            e
        })
    }

    /// Projette un objet recu (dont on connait la taille).
    pub fn map(handle: u32, len: usize, p: u64) -> Result<Self, i64> {
        let addr = sys::shm_map(handle, 0, p)?;
        Ok(Self { handle, len, addr })
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adresse physique d'un octet (tampons DMA, droit DMA requis).
    pub fn paddr(&self, offset: usize) -> Result<u64, i64> {
        sys::shm_paddr(self.handle, offset as u64)
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.addr as *const u8, self.len) }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn as_mut_slice(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.addr as *mut u8, self.len) }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        let len = (self.len as u64).next_multiple_of(PAGE_SIZE);
        let _ = sys::mem_unmap(self.addr, len);
        let _ = sys::handle_close(self.handle);
    }
}

const CLASSES: usize = 8; // 16, 32, ..., 2048
const MIN_SHIFT: usize = 4;
const MAX_SMALL: usize = 2048;
const REFILL: usize = 64 * 1024; // pages demandees au noyau par lot

struct State {
    free: [*mut u8; CLASSES],
    page_cur: usize,
    page_end: usize,
}

unsafe impl Send for State {}

pub struct Heap {
    state: Mutex<State>,
}

impl Heap {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(State { free: [ptr::null_mut(); CLASSES], page_cur: 0, page_end: 0 }),
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn class_of(size: usize) -> usize {
    let s = size.max(1 << MIN_SHIFT).next_power_of_two();
    s.trailing_zeros() as usize - MIN_SHIFT
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let size = l.size().max(l.align());
        if size > MAX_SMALL {
            if l.align() > PAGE_SIZE as usize {
                return ptr::null_mut();
            }
            let len = size.next_multiple_of(PAGE_SIZE as usize);
            return map(len, prot::READ | prot::WRITE).unwrap_or(ptr::null_mut());
        }
        let c = class_of(size);
        let mut st = self.state.lock();
        let head = st.free[c];
        if !head.is_null() {
            st.free[c] = unsafe { *(head as *mut *mut u8) };
            return head;
        }
        // Classe vide : une page neuve, decoupee entierement dans cette classe.
        if st.page_cur == st.page_end {
            match map(REFILL, prot::READ | prot::WRITE) {
                Ok(p) => {
                    st.page_cur = p as usize;
                    st.page_end = p as usize + REFILL;
                }
                Err(_) => return ptr::null_mut(),
            }
        }
        let page = st.page_cur;
        st.page_cur += PAGE_SIZE as usize;
        let bsize = 1usize << (c + MIN_SHIFT);
        let mut off = bsize;
        while off < PAGE_SIZE as usize {
            let b = (page + off) as *mut u8;
            unsafe { *(b as *mut *mut u8) = st.free[c] };
            st.free[c] = b;
            off += bsize;
        }
        page as *mut u8
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let size = l.size().max(l.align());
        if size > MAX_SMALL {
            let _ = unmap(p, size.next_multiple_of(PAGE_SIZE as usize));
            return;
        }
        let c = class_of(size);
        let mut st = self.state.lock();
        unsafe { *(p as *mut *mut u8) = st.free[c] };
        st.free[c] = p;
    }
}

#[global_allocator]
static HEAP: Heap = Heap::new();

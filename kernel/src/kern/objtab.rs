//! Tables d'objets noyau extensibles.
//!
//! Les objets (threads, processus, points de rendez-vous...) sont ranges dans
//! des tranches allouees a la demande par l'allocateur de pages : aucune
//! limite fixee a la compilation, et rien n'est reserve tant que le systeme
//! n'en a pas besoin. Les emplacements libres forment une liste chainee par
//! indice, d'ou une allocation et une liberation en O(1). L'acces par indice
//! reste une simple indirection : tranche puis decalage.

use crate::kern::thread::NIL;
use crate::mm::pmm;

/// Taille visee d'une tranche.
const CHUNK_BYTES: usize = 16 * 1024;
/// Indices sur 16 bits, `NIL` reserve.
pub const MAX_OBJECTS: usize = 0xFFFF;
const MAX_CHUNKS: usize = 1024;

pub struct Table<T: 'static> {
    chunks: [u64; MAX_CHUNKS],
    nchunks: usize,
    shift: u32,
    free_head: u16,
    live: usize,
    /// Met un emplacement neuf dans l'etat « libre ».
    reset: fn(&mut T),
    /// Champ de chainage de la liste libre.
    link: fn(&mut T) -> &mut u16,
}

impl<T: 'static> Table<T> {
    pub const fn new(reset: fn(&mut T), link: fn(&mut T) -> &mut u16) -> Self {
        let size = core::mem::size_of::<T>();
        let mut per = CHUNK_BYTES / size;
        if per == 0 {
            per = 1;
        }
        // Plus grande puissance de 2 <= per.
        let shift = usize::BITS - 1 - per.leading_zeros();
        Self {
            chunks: [0; MAX_CHUNKS],
            nchunks: 0,
            shift,
            free_head: NIL,
            live: 0,
            reset,
            link,
        }
    }

    #[inline(always)]
    fn per_chunk(&self) -> usize {
        1 << self.shift
    }

    /// Emplacements deja crees (bornes d'iteration).
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        (self.nchunks << self.shift).min(MAX_OBJECTS)
    }

    #[inline(always)]
    pub fn live(&self) -> usize {
        self.live
    }

    #[inline(always)]
    pub fn get(&self, i: u16) -> &'static mut T {
        let i = i as usize;
        debug_assert!(i < self.capacity());
        let base = self.chunks[i >> self.shift] as *mut T;
        unsafe { &mut *base.add(i & (self.per_chunk() - 1)) }
    }

    fn grow(&mut self) -> bool {
        if self.nchunks == MAX_CHUNKS || self.capacity() >= MAX_OBJECTS {
            return false;
        }
        let bytes = (self.per_chunk() * core::mem::size_of::<T>()) as u64;
        let pa = match pmm::alloc_zeroed(pmm::order_for(bytes)) {
            Some(pa) => pa,
            None => return false,
        };
        let first = self.nchunks << self.shift;
        self.chunks[self.nchunks] = pa;
        self.nchunks += 1;
        // Enfile les nouveaux emplacements en ordre croissant.
        for k in (0..self.per_chunk()).rev() {
            let idx = first + k;
            if idx >= MAX_OBJECTS {
                continue;
            }
            let obj = self.get(idx as u16);
            (self.reset)(obj);
            *(self.link)(obj) = self.free_head;
            self.free_head = idx as u16;
        }
        true
    }

    /// Reserve un emplacement ; l'objet est dans l'etat laisse par `reset`.
    pub fn alloc(&mut self) -> Option<u16> {
        if self.free_head == NIL && !self.grow() {
            return None;
        }
        let i = self.free_head;
        let obj = self.get(i);
        self.free_head = *(self.link)(obj);
        *(self.link)(obj) = NIL;
        self.live += 1;
        Some(i)
    }

    /// Rend un emplacement (l'appelant a deja remis l'objet a l'etat libre).
    pub fn free(&mut self, i: u16) {
        let obj = self.get(i);
        *(self.link)(obj) = self.free_head;
        self.free_head = i;
        self.live -= 1;
    }
}

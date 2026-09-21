//! Acces MMIO volatils.
//!
//! Toutes les lectures/ecritures de registres passent par ici : le compilateur
//! ne doit ni les reordonner ni les elider. Les barrieres explicites restent a
//! la charge de l'appelant (`dsb`/`isb` dans `arch::barrier`).

use core::ptr::{read_volatile, write_volatile};

#[inline(always)]
pub unsafe fn read32(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline(always)]
pub unsafe fn write32(addr: usize, val: u32) {
    unsafe { write_volatile(addr as *mut u32, val) }
}

#[inline(always)]
pub unsafe fn modify32(addr: usize, clear: u32, set: u32) {
    unsafe {
        let v = read32(addr);
        write32(addr, (v & !clear) | set);
    }
}

/// Bloc de registres 32 bits a espacement configurable (`shift` = log2 du pas).
#[derive(Clone, Copy)]
pub struct RegBlock {
    base: usize,
    shift: u32,
}

impl RegBlock {
    #[inline(always)]
    pub const fn new(base: usize, shift: u32) -> Self {
        Self { base, shift }
    }

    #[inline(always)]
    pub const fn addr(&self, reg: usize) -> usize {
        self.base + (reg << self.shift)
    }

    #[inline(always)]
    pub unsafe fn read(&self, reg: usize) -> u32 {
        unsafe { read32(self.addr(reg)) }
    }

    #[inline(always)]
    pub unsafe fn write(&self, reg: usize, val: u32) {
        unsafe { write32(self.addr(reg), val) }
    }
}

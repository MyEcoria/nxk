//! Stockage local de thread, modele AArch64 « variante 1 ».
//!
//! `TPIDR_EL0` designe un bloc de controle de 16 octets suivi (a
//! l'alignement du segment PT_TLS) de la copie propre au thread de `.tdata`
//! puis de `.tbss` a zero. Le compilateur accede aux variables locales de
//! thread par un simple decalage depuis `TPIDR_EL0` (modele local-exec) :
//! ni noyau ni editeur de liens dynamique n'interviennent.

use super::abi::layout::PAGE_SIZE;
use super::abi::prot;
use super::syscall;
use crate::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const PT_TLS: u32 = 7;

#[repr(C)]
struct Phdr {
    ptype: u32,
    pflags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

// Modele du segment PT_TLS, lu une fois au demarrage.
static IMAGE: AtomicU64 = AtomicU64::new(0);
static FILESZ: AtomicUsize = AtomicUsize::new(0);
static MEMSZ: AtomicUsize = AtomicUsize::new(0);
static ALIGN: AtomicUsize = AtomicUsize::new(16);

/// Decalage des donnees depuis `TPIDR_EL0`.
fn data_offset() -> usize {
    16usize.next_multiple_of(ALIGN.load(Ordering::Relaxed))
}

/// Octets a reserver pour le bloc d'un thread (0 : programme sans TLS).
pub fn block_size() -> usize {
    let m = MEMSZ.load(Ordering::Relaxed);
    if m == 0 { 0 } else { data_offset() + m }
}

/// Alignement exige par le bloc.
pub fn block_align() -> usize {
    ALIGN.load(Ordering::Relaxed)
}

/// Initialise un bloc a `base` (aligne, `block_size()` octets a zero) et
/// rend la valeur de `TPIDR_EL0` correspondante.
///
/// # Safety
/// `base` doit designer une zone inscriptible de `block_size()` octets,
/// deja a zero.
pub unsafe fn init_block(base: *mut u8) -> u64 {
    let n = FILESZ.load(Ordering::Relaxed);
    if n > 0 {
        let src = IMAGE.load(Ordering::Relaxed) as *const u8;
        unsafe { crate::ptr::copy_nonoverlapping(src, base.add(data_offset()), n) };
    }
    base as u64
}

/// Lit l'en-tete PT_TLS et installe le TLS du thread principal.
///
/// # Safety
/// A appeler une seule fois, au demarrage, avant tout acces TLS.
pub unsafe fn init_main() {
    let Some(b) = super::startup_block() else { return };
    for k in 0..b.phnum as usize {
        let ph = unsafe { &*((b.phdr + (k * b.phent as usize) as u64) as *const Phdr) };
        if ph.ptype == PT_TLS && ph.memsz > 0 {
            IMAGE.store(b.load_base + ph.vaddr, Ordering::Relaxed);
            FILESZ.store(ph.filesz as usize, Ordering::Relaxed);
            MEMSZ.store(ph.memsz as usize, Ordering::Relaxed);
            ALIGN.store((ph.align as usize).max(16), Ordering::Relaxed);
        }
    }
    let size = block_size();
    if size == 0 {
        return;
    }
    // Pages fraiches (donc a zero), alignees sur la page.
    let len = (size as u64).next_multiple_of(PAGE_SIZE);
    let r = syscall::mem_map(len, prot::READ | prot::WRITE);
    if r < 0 || block_align() as u64 > PAGE_SIZE {
        super::abort_internal();
    }
    unsafe { set_tp(init_block(r as *mut u8)) };
}

/// # Safety
/// `tp` doit designer un bloc initialise par `init_block`.
pub unsafe fn set_tp(tp: u64) {
    unsafe { core::arch::asm!("msr tpidr_el0, {}", in(reg) tp, options(nomem, nostack, preserves_flags)) };
}

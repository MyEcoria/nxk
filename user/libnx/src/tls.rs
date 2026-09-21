//! Stockage local de thread (TLS) du modele AArch64 « variante 1 ».
//!
//! `TPIDR_EL0` designe un bloc de controle de 16 octets suivi (a l'alignement
//! du segment PT_TLS) de la copie propre au thread du modele `.tdata`, puis
//! de `.tbss` a zero. Le compilateur accede aux variables `#[thread_local]`
//! d'un executable par un simple decalage depuis `TPIDR_EL0` (modele
//! local-exec) : aucune aide du noyau, qui ne fait que sauver le registre.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};

use crate::env;

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

#[derive(Clone, Copy)]
struct Template {
    image: u64,
    filesz: usize,
    memsz: usize,
    align: usize,
}

static mut TEMPLATE: Option<Template> = None;

fn template() -> Option<Template> {
    unsafe { *core::ptr::addr_of!(TEMPLATE) }
}

/// Lit l'en-tete PT_TLS et installe le TLS du thread principal.
pub fn init() {
    let Some(b) = env::startup_block() else { return };
    for k in 0..b.phnum as usize {
        let ph = unsafe { &*((b.phdr + (k * b.phent as usize) as u64) as *const Phdr) };
        if ph.ptype == PT_TLS && ph.memsz > 0 {
            let t = Template {
                image: b.load_base + ph.vaddr,
                filesz: ph.filesz as usize,
                memsz: ph.memsz as usize,
                align: (ph.align as usize).max(16),
            };
            unsafe { TEMPLATE = Some(t) };
        }
    }
    if let Some(tp) = alloc_block() {
        unsafe { set_tp(tp) };
    }
}

fn layout(t: &Template) -> (usize, Layout) {
    let off = 16usize.next_multiple_of(t.align);
    (off, Layout::from_size_align(off + t.memsz, t.align).unwrap())
}

/// Nouveau bloc TLS initialise ; rend la valeur de `TPIDR_EL0` (0 : pas de TLS).
pub fn alloc_block() -> Option<u64> {
    let t = template()?;
    let (off, l) = layout(&t);
    let p = unsafe { alloc_zeroed(l) };
    if p.is_null() {
        return None;
    }
    unsafe { core::ptr::copy_nonoverlapping(t.image as *const u8, p.add(off), t.filesz) };
    Some(p as u64)
}

pub fn free_block(tp: u64) {
    if let (Some(t), true) = (template(), tp != 0) {
        unsafe { dealloc(tp as *mut u8, layout(&t).1) };
    }
}

/// # Safety
/// `tp` doit designer un bloc rendu par `alloc_block`, ou 0.
pub unsafe fn set_tp(tp: u64) {
    unsafe { core::arch::asm!("msr tpidr_el0, {}", in(reg) tp, options(nomem, nostack)) };
}

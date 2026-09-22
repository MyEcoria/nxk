//! AArch64 variant-1 TLS via `TPIDR_EL0` (local-exec model).
//! Block = 16-byte header plus `.tdata`/`.tbss` copy.

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

/// Read the PT_TLS header and install the main thread TLS.
pub fn init() {
    let Some(b) = env::startup_block() else {
        return;
    };
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
    (
        off,
        Layout::from_size_align(off + t.memsz, t.align).unwrap(),
    )
}

/// New initialized TLS block; returns the `TPIDR_EL0` value (0: no TLS).
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
/// `tp` must be a block from `alloc_block`, or 0.
pub unsafe fn set_tp(tp: u64) {
    unsafe { core::arch::asm!("msr tpidr_el0, {}", in(reg) tp, options(nomem, nostack)) };
}

//! AArch64 variant-1 TLS via `TPIDR_EL0` (local-exec model).
//! Block = 16-byte header plus `.tdata`/`.tbss` copy (`startup::Tls`).

use alloc::alloc::{alloc_zeroed, dealloc, Layout};

use nxk_abi::startup::Tls;

use crate::env;

static mut TEMPLATE: Option<Tls> = None;

fn template() -> Option<Tls> {
    unsafe { *core::ptr::addr_of!(TEMPLATE) }
}

/// Read the PT_TLS header and install the main thread TLS.
pub fn init() {
    let Some(t) = env::startup_block().and_then(|b| b.tls()) else {
        return;
    };
    unsafe { TEMPLATE = Some(t) };
    if let Some(tp) = alloc_block() {
        unsafe { set_tp(tp) };
    }
}

fn layout(t: &Tls) -> Layout {
    Layout::from_size_align(t.block_size(), t.align).unwrap()
}

/// New initialized TLS block; returns the `TPIDR_EL0` value (0: no TLS).
pub fn alloc_block() -> Option<u64> {
    let t = template()?;
    let p = unsafe { alloc_zeroed(layout(&t)) };
    if p.is_null() {
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(t.image as *const u8, p.add(t.data_offset()), t.filesz)
    };
    Some(p as u64)
}

pub fn free_block(tp: u64) {
    if let (Some(t), true) = (template(), tp != 0) {
        unsafe { dealloc(tp as *mut u8, layout(&t)) };
    }
}

pub use nxk_abi::user::set_tp;

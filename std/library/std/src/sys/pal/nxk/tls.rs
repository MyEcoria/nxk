//! TLS, AArch64 variant 1: 16-byte block at `TPIDR_EL0`, then `.tdata`/zeroed `.tbss`.
//! Compiler uses plain `TPIDR_EL0` offsets (local-exec); no kernel or dynamic linker involved.

use super::abi::layout::PAGE_SIZE;
use super::abi::prot;
use super::syscall;
use crate::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// PT_TLS template, read once at startup.
static IMAGE: AtomicU64 = AtomicU64::new(0);
static FILESZ: AtomicUsize = AtomicUsize::new(0);
static MEMSZ: AtomicUsize = AtomicUsize::new(0);
static ALIGN: AtomicUsize = AtomicUsize::new(16);

/// Data offset from `TPIDR_EL0`.
fn data_offset() -> usize {
    16usize.next_multiple_of(ALIGN.load(Ordering::Relaxed))
}

/// Bytes to reserve per thread block (0: no TLS).
pub fn block_size() -> usize {
    let m = MEMSZ.load(Ordering::Relaxed);
    if m == 0 { 0 } else { data_offset() + m }
}

/// Alignment required by the block.
pub fn block_align() -> usize {
    ALIGN.load(Ordering::Relaxed)
}

/// Init a block at `base` and return its `TPIDR_EL0`. # Safety: `base` is a zeroed writable `block_size()` zone.
pub unsafe fn init_block(base: *mut u8) -> u64 {
    let n = FILESZ.load(Ordering::Relaxed);
    if n > 0 {
        let src = IMAGE.load(Ordering::Relaxed) as *const u8;
        unsafe { crate::ptr::copy_nonoverlapping(src, base.add(data_offset()), n) };
    }
    base as u64
}

/// Install the main-thread TLS from the PT_TLS header. # Safety: call once at startup, before any TLS use.
pub unsafe fn init_main() {
    let Some(t) = super::startup_block().and_then(|b| b.tls()) else { return };
    IMAGE.store(t.image, Ordering::Relaxed);
    FILESZ.store(t.filesz, Ordering::Relaxed);
    MEMSZ.store(t.memsz, Ordering::Relaxed);
    ALIGN.store(t.align, Ordering::Relaxed);
    let size = block_size();
    if size == 0 {
        return;
    }
    // Fresh (hence zeroed) pages, page-aligned.
    let len = (size as u64).next_multiple_of(PAGE_SIZE);
    let r = syscall::mem_map(len, prot::READ | prot::WRITE);
    if r < 0 || block_align() as u64 > PAGE_SIZE {
        super::abort_internal();
    }
    unsafe { set_tp(init_block(r as *mut u8)) };
}

pub use super::abi::user::set_tp;

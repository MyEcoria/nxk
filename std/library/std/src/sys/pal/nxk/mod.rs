//! nxk platform (Tegra X1): no libc; syscalls (`svc #0`, Linux arm64 convention) + IPC to services.
//! Startup: kernel enters `_start` (flags, launcher arg, startup block); main-thread TLS first, then rustc `main`.

#![forbid(unsafe_op_in_unsafe_fn)]

#[allow(dead_code, unused_imports, unreachable_pub)]
pub mod abi;
pub mod futex;
pub mod ipc;
pub mod svc;
pub mod syscall;
pub mod tls;
pub mod vm;

use crate::io;
use crate::sync::atomic::{AtomicU64, Ordering};

static FLAGS: AtomicU64 = AtomicU64::new(0);
static BLOCK: AtomicU64 = AtomicU64::new(0);

#[cfg(not(test))]
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".globl _start",
    ".type _start, @function",
    "_start:",
    "    mov     x29, #0",
    "    mov     x30, #0",
    "    bl      __nxk_start",
    "    brk     #0",
);

#[cfg(not(test))]
#[unsafe(no_mangle)]
unsafe extern "C" fn __nxk_start(flags: u64, _arg: u64, block: u64) -> ! {
    FLAGS.store(flags, Ordering::Relaxed);
    BLOCK.store(block, Ordering::Relaxed);
    if flags & abi::startup::HAS_CONSOLE != 0 {
        svc::set_console(abi::slots::CONSOLE);
    }
    if flags & abi::startup::HAS_PM != 0 {
        svc::set_pm(abi::slots::PM);
    }
    // SAFETY: first code to run, on the main thread.
    unsafe { tls::init_main() };
    // SAFETY: executable constructors, called once.
    unsafe { run_init_array() };

    unsafe extern "C" {
        fn main(argc: i32, argv: *const *const u8) -> i32;
    }
    // SAFETY: `main` is the C entry rustc generated.
    let code = unsafe { main(0, crate::ptr::null()) };
    syscall::process_exit(code as i64)
}

/// `.init_array` constructors (e.g. `ctor`, `inventory`).
#[cfg(not(test))]
unsafe fn run_init_array() {
    unsafe extern "C" {
        static __init_array_start: [unsafe extern "C" fn(); 0];
        static __init_array_end: [unsafe extern "C" fn(); 0];
    }
    let mut p = (&raw const __init_array_start).cast::<unsafe extern "C" fn()>();
    let end = (&raw const __init_array_end).cast::<unsafe extern "C" fn()>();
    while p < end {
        // SAFETY: filled by the linker.
        unsafe {
            (*p)();
            p = p.add(1);
        }
    }
}

/// Startup flags (`abi::startup::HAS_*`).
pub fn startup_flags() -> u64 {
    FLAGS.load(Ordering::Relaxed)
}

/// Kernel startup block, if valid.
pub fn startup_block() -> Option<&'static abi::startup::Block> {
    let p = BLOCK.load(Ordering::Relaxed);
    if p == 0 {
        return None;
    }
    // SAFETY: the kernel leaves the block atop the main stack,
    // which outlives the process.
    let b = unsafe { &*(p as *const abi::startup::Block) };
    (b.magic == abi::startup::BLOCK_MAGIC).then_some(b)
}

/// Startup-block strings: `argc` args then `envc` vars.
pub fn startup_strings() -> (&'static [u8], usize, usize) {
    match startup_block() {
        Some(b) => {
            // SAFETY: copied by the kernel into the startup block.
            let s = unsafe { crate::slice::from_raw_parts(b.strings as *const u8, b.strings_len as usize) };
            (s, b.argc as usize, b.envc as usize)
        }
        None => (&[], 0, 0),
    }
}

// SAFETY: must be called only once during runtime initialization.
// NOTE: `__nxk_start` already did everything.
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
// NOTE: this is not guaranteed to run, for example when the program aborts.
pub unsafe fn cleanup() {}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

/// Like Unix `abort()`: code 134 (128 + SIGABRT).
pub fn abort_internal() -> ! {
    syscall::process_exit(134)
}

/// Syscall/service result: `-errno` becomes an
/// `io::Error` (Linux codes).
#[inline]
pub fn cvt(r: i64) -> io::Result<u64> {
    if r < 0 { Err(io::Error::from_raw_os_error((-r) as i32)) } else { Ok(r as u64) }
}

/// `Duration` to kernel nanoseconds (0 = infinite, 1 = poll).
pub fn timeout_ns(d: Option<crate::time::Duration>) -> u64 {
    match d {
        None => abi::timeout::INFINITE,
        Some(d) => match u64::try_from(d.as_nanos()) {
            Ok(0) => abi::timeout::POLL,
            Ok(n) => n,
            // Over 584 years: just wait forever.
            Err(_) => abi::timeout::INFINITE,
        },
    }
}

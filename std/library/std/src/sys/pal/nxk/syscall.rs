//! Raw syscalls: `svc #0`, number in x8, args in x0-x5,
//! result (or `-errno`) in x0. The kernel preserves other registers.

use core::arch::asm;

use super::abi::sys::*;
use crate::sync::atomic::AtomicU32;

#[inline(always)]
pub unsafe fn syscall0(nr: u64) -> i64 {
    let r: i64;
    unsafe { asm!("svc #0", lateout("x0") r, in("x8") nr, options(nostack)) };
    r
}

#[inline(always)]
pub unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    let r: i64;
    unsafe { asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x8") nr, options(nostack)) };
    r
}

#[inline(always)]
pub unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x8") nr, options(nostack))
    };
    r
}

#[inline(always)]
pub unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x2") a2, in("x8") nr,
             options(nostack))
    };
    r
}

#[inline(always)]
pub unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x2") a2, in("x3") a3,
             in("x8") nr, options(nostack))
    };
    r
}

pub fn debug_write(b: &[u8]) {
    unsafe { syscall2(DEBUG_WRITE, b.as_ptr() as u64, b.len() as u64) };
}

pub fn yield_now() {
    unsafe { syscall0(YIELD) };
}

pub fn process_exit(code: i64) -> ! {
    unsafe { syscall1(PROCESS_EXIT, code as u64) };
    // The kernel never returns from PROCESS_EXIT.
    loop {
        core::hint::spin_loop();
    }
}

/// Thread exit; the kernel signals `notif` (bits `bits`) once the thread
/// left EL0, so its stack can be freed.
pub fn thread_exit_notify(notif: u32, bits: u64) -> ! {
    unsafe { syscall3(THREAD_EXIT, 0, notif as u64, bits) };
    loop {
        core::hint::spin_loop();
    }
}

pub fn sleep_ns(ns: u64) {
    unsafe { syscall1(SLEEP, ns) };
}

pub fn realtime_ns() -> u64 {
    unsafe { syscall1(CLOCK, super::abi::clock::REALTIME) as u64 }
}

pub fn getrandom(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(64 * 1024) {
        unsafe { syscall2(GETRANDOM, chunk.as_mut_ptr() as u64, chunk.len() as u64) };
    }
}

pub fn getpid(process: u32) -> i64 {
    unsafe { syscall1(GETPID, process as u64) }
}

pub fn gettid() -> u64 {
    unsafe { syscall0(GETTID) as u64 }
}

pub fn handle_close(h: u32) {
    unsafe { syscall1(HANDLE_CLOSE, h as u64) };
}

/// Anonymous mapping (demand-paged); returns the address.
pub fn mem_map(len: u64, prot: u64) -> i64 {
    unsafe { syscall3(MEM_MAP, 0, len, prot) }
}

/// Like `mem_map`, at a fixed address (free range, else -EEXIST).
pub fn mem_map_at(addr: u64, len: u64, prot: u64) -> i64 {
    unsafe { syscall3(MEM_MAP, addr, len, prot) }
}

pub fn mem_unmap(addr: u64, len: u64) -> i64 {
    unsafe { syscall2(MEM_UNMAP, addr, len) }
}

pub fn mem_protect(addr: u64, len: u64, prot: u64) -> i64 {
    unsafe { syscall3(MEM_PROTECT, addr, len, prot) }
}

pub fn thread_create(entry: u64, sp: u64, arg: u64, flags: u64) -> i64 {
    unsafe { syscall4(THREAD_CREATE, entry, sp, arg, flags) }
}

/// Wait while `*addr == val`, up to `timeout` ns (0: infinite).
pub fn futex_wait(addr: &AtomicU32, val: u32, timeout: u64) -> i64 {
    unsafe { syscall3(FUTEX_WAIT, addr.as_ptr() as u64, val as u64, timeout) }
}

/// Wake up to `n` threads; returns how many.
pub fn futex_wake(addr: &AtomicU32, n: u64) -> i64 {
    unsafe { syscall2(FUTEX_WAKE, addr.as_ptr() as u64, n) }
}

pub fn process_wait(h: u32, timeout: u64) -> i64 {
    unsafe { syscall2(PROCESS_WAIT, h as u64, timeout) }
}

pub fn process_kill(h: u32) -> i64 {
    unsafe { syscall1(PROCESS_KILL, h as u64) }
}

/// New endpoint (`abi::endpoint::PIPE`: pipe).
pub fn endpoint_create(flags: u64) -> i64 {
    unsafe { syscall1(ENDPOINT_CREATE, flags) }
}

/// Handle copy with reduced rights.
pub fn handle_dup(h: u32, rights: u16, badge: u64) -> i64 {
    unsafe { syscall3(HANDLE_DUP, h as u64, rights as u64, badge) }
}

pub fn notify_create() -> i64 {
    unsafe { syscall0(NOTIFY_CREATE) }
}

pub fn notify_wait(h: u32, timeout: u64) -> i64 {
    unsafe { syscall2(NOTIFY_WAIT, h as u64, timeout) }
}

pub fn sysinfo(kind: u64, buf: &mut [u8]) -> i64 {
    unsafe { syscall3(SYSINFO, kind, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

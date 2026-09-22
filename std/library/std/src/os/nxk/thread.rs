//! nxk-specific current-thread settings: priority and core.

#![unstable(feature = "nxk_ext", issue = "none")]

use crate::io;
use crate::sys::abi::{sys, thread};
use crate::sys::{cvt, syscall};

/// Lowest priority (highest is 1).
pub const MAX_PRIORITY: u8 = thread::MAX_PRIO;

/// Current thread priority: 1 (highest) to 30; 0 = default (16).
/// A higher-priority thread takes the core as soon as ready.
pub fn set_priority(prio: u8) -> io::Result<()> {
    cvt(unsafe { syscall::syscall3(sys::THREAD_SET, 0, thread::SET_PRIO, prio as u64) }).map(|_| ())
}

/// Pin the current thread to a core (`Some(0..=3)`), or let the kernel
/// place and migrate it (`None`).
pub fn set_affinity(cpu: Option<u8>) -> io::Result<()> {
    let v = cpu.map(|c| c as u64).unwrap_or(thread::ANY_CPU);
    cvt(unsafe { syscall::syscall3(sys::THREAD_SET, 0, thread::SET_AFFINITY, v) }).map(|_| ())
}

/// Core this thread currently runs on.
pub fn current_cpu() -> u32 {
    unsafe { syscall::syscall0(sys::GETCPU) as u32 }
}

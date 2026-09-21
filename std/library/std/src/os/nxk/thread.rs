//! Reglages du thread courant propres a nxk : priorite et coeur.

#![unstable(feature = "nxk_ext", issue = "none")]

use crate::io;
use crate::sys::abi::{sys, thread};
use crate::sys::{cvt, syscall};

/// Plus basse priorite (la plus haute est 1).
pub const MAX_PRIORITY: u8 = thread::MAX_PRIO;

/// Priorite du thread courant : 1 (la plus haute) a 30 ; 0 = defaut (16).
/// Un thread plus prioritaire prend le coeur aussitot pret.
pub fn set_priority(prio: u8) -> io::Result<()> {
    cvt(unsafe { syscall::syscall3(sys::THREAD_SET, 0, thread::SET_PRIO, prio as u64) }).map(|_| ())
}

/// Epingle le thread courant sur un coeur (`Some(0..=3)`), ou laisse le
/// noyau le placer et le deplacer (`None`).
pub fn set_affinity(cpu: Option<u8>) -> io::Result<()> {
    let v = cpu.map(|c| c as u64).unwrap_or(thread::ANY_CPU);
    cvt(unsafe { syscall::syscall3(sys::THREAD_SET, 0, thread::SET_AFFINITY, v) }).map(|_| ())
}

/// Coeur sur lequel le thread s'execute en ce moment.
pub fn current_cpu() -> u32 {
    unsafe { syscall::syscall0(sys::GETCPU) as u32 }
}

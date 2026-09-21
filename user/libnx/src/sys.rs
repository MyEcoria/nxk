//! Appels systeme bruts (convention Linux arm64 : `svc #0`, numero dans x8).
//!
//! Le noyau ne modifie que x0 pour un appel ordinaire : les autres registres
//! sont preserves, d'ou des `asm!` sans clobber superflu.

use core::arch::asm;

use nxk_abi::spawn::SpawnArgs;
use nxk_abi::sys::*;

pub type Result<T> = core::result::Result<T, i64>;

#[inline(always)]
pub fn check(r: i64) -> Result<u64> {
    if r < 0 {
        Err(r)
    } else {
        Ok(r as u64)
    }
}

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
    unsafe { asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x8") nr, options(nostack)) };
    r
}

#[inline(always)]
pub unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x2") a2, in("x8") nr, options(nostack))
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

// ------------------------------------------------------------ enveloppes ---

#[inline(always)]
pub fn nop() {
    unsafe { syscall0(NOP) };
}

pub fn debug_write(b: &[u8]) {
    unsafe { syscall2(DEBUG_WRITE, b.as_ptr() as u64, b.len() as u64) };
}

#[inline(always)]
pub fn yield_now() {
    unsafe { syscall0(YIELD) };
}

pub fn thread_exit(code: i64) -> ! {
    unsafe { syscall3(THREAD_EXIT, code as u64, 0, 0) };
    unreachable!()
}

/// Fin du thread ; le noyau signale `notif` une fois le thread sorti d'EL0.
pub fn thread_exit_notify(code: i64, notif: u32, bits: u64) -> ! {
    unsafe { syscall3(THREAD_EXIT, code as u64, notif as u64, bits) };
    unreachable!()
}

pub fn process_exit(code: i64) -> ! {
    unsafe { syscall1(PROCESS_EXIT, code as u64) };
    unreachable!()
}

pub fn sleep_ns(ns: u64) {
    unsafe { syscall1(SLEEP, ns) };
}

/// Temps monotone (ns depuis le demarrage), via le noyau.
pub fn clock_ns() -> u64 {
    unsafe { syscall1(CLOCK, nxk_abi::clock::MONOTONIC) as u64 }
}

/// Heure murale : ns depuis le 1er janvier 1970 (UTC).
pub fn realtime_ns() -> u64 {
    unsafe { syscall1(CLOCK, nxk_abi::clock::REALTIME) as u64 }
}

/// Redemarre ou eteint la machine (`nxk_abi::power::*`, capacite SysControl).
pub fn system_power(sysctl: u32, cmd: u64) -> Result<()> {
    check(unsafe { syscall2(SYSTEM_POWER, sysctl as u64, cmd) }).map(|_| ())
}

/// Regle l'heure murale (capacite SysControl).
pub fn clock_set_realtime(sysctl: u32, ns: u64) -> Result<()> {
    check(unsafe { syscall3(CLOCK_SET, sysctl as u64, nxk_abi::clock::REALTIME, ns) }).map(|_| ())
}

pub fn getcpu() -> u32 {
    unsafe { syscall0(GETCPU) as u32 }
}

/// Remplit `buf` d'octets aleatoires (generateur du noyau).
pub fn getrandom(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(64 * 1024) {
        unsafe { syscall2(GETRANDOM, chunk.as_mut_ptr() as u64, chunk.len() as u64) };
    }
}

pub fn handle_close(h: u32) -> Result<()> {
    check(unsafe { syscall1(HANDLE_CLOSE, h as u64) }).map(|_| ())
}

pub fn handle_dup(h: u32, rights: u16, badge: u64) -> Result<u32> {
    check(unsafe { syscall3(HANDLE_DUP, h as u64, rights as u64, badge) }).map(|v| v as u32)
}

/// Invalide toutes les autres capacites designant le meme objet ; `h` reste
/// valide (droit CONTROL).
pub fn handle_revoke(h: u32) -> Result<()> {
    check(unsafe { syscall1(HANDLE_REVOKE, h as u64) }).map(|_| ())
}

pub fn mem_map(addr: u64, len: u64, prot: u64) -> Result<u64> {
    check(unsafe { syscall3(MEM_MAP, addr, len, prot) })
}

pub fn mem_unmap(addr: u64, len: u64) -> Result<()> {
    check(unsafe { syscall2(MEM_UNMAP, addr, len) }).map(|_| ())
}

/// Change les droits de pages deja projetees (0 : aucun acces, contenu garde).
pub fn mem_protect(addr: u64, len: u64, prot: u64) -> Result<()> {
    check(unsafe { syscall3(MEM_PROTECT, addr, len, prot) }).map(|_| ())
}

/// Objet de memoire partagee (`nxk_abi::shm::*`).
pub fn shm_create(len: u64, flags: u64) -> Result<u32> {
    check(unsafe { syscall2(SHM_CREATE, len, flags) }).map(|v| v as u32)
}

/// Projette tout l'objet ; `addr` 0 = adresse choisie par le noyau.
pub fn shm_map(h: u32, addr: u64, prot: u64) -> Result<u64> {
    check(unsafe { syscall3(SHM_MAP, h as u64, addr, prot) })
}

/// Adresse physique d'un octet de l'objet (droit DMA).
pub fn shm_paddr(h: u32, offset: u64) -> Result<u64> {
    check(unsafe { syscall2(SHM_PADDR, h as u64, offset) })
}

pub fn mmio_map(h: u32, phys: u64, len: u64) -> Result<u64> {
    check(unsafe { syscall3(MMIO_MAP, h as u64, phys, len) })
}

pub fn mmio_derive(h: u32, phys: u64, len: u64) -> Result<u32> {
    check(unsafe { syscall3(MMIO_DERIVE, h as u64, phys, len) }).map(|v| v as u32)
}

pub fn thread_create(entry: u64, sp: u64, arg: u64, flags: u64) -> Result<u32> {
    check(unsafe { syscall4(THREAD_CREATE, entry, sp, arg, flags) }).map(|v| v as u32)
}

/// Reglage d'un thread (`nxk_abi::thread::SET_*`) ; `h` = 0 : thread courant.
pub fn thread_set(h: u32, what: u64, value: u64) -> Result<()> {
    check(unsafe { syscall3(THREAD_SET, h as u64, what, value) }).map(|_| ())
}

/// Attend tant que `*addr == val`, au plus `timeout_ns` (0 : sans limite).
/// `Err(-EAGAIN)` si la valeur differait deja.
pub fn futex_wait(addr: &core::sync::atomic::AtomicU32, val: u32, timeout_ns: u64) -> Result<()> {
    check(unsafe { syscall3(FUTEX_WAIT, addr.as_ptr() as u64, val as u64, timeout_ns) }).map(|_| ())
}

/// Reveille jusqu'a `n` threads en attente sur `addr` ; rend leur nombre.
pub fn futex_wake(addr: &core::sync::atomic::AtomicU32, n: u64) -> u64 {
    unsafe { syscall2(FUTEX_WAKE, addr.as_ptr() as u64, n) }.max(0) as u64
}

pub fn process_spawn(a: &SpawnArgs) -> Result<u32> {
    check(unsafe { syscall1(PROCESS_SPAWN, a as *const SpawnArgs as u64) }).map(|v| v as u32)
}

/// Attend la fin du processus et rend son code de sortie.
pub fn process_wait(h: u32) -> i64 {
    unsafe { syscall2(PROCESS_WAIT, h as u64, 0) }
}

/// Comme `process_wait`, au plus `timeout_ns` : `Err(-ETIMEDOUT)` sinon.
pub fn process_wait_timeout(h: u32, timeout_ns: u64) -> Result<i64> {
    let r = unsafe { syscall2(PROCESS_WAIT, h as u64, timeout_ns) };
    if r == -nxk_abi::err::ETIMEDOUT || r == -nxk_abi::err::EBADF {
        Err(r)
    } else {
        Ok(r)
    }
}

pub fn process_kill(h: u32) -> Result<()> {
    check(unsafe { syscall1(PROCESS_KILL, h as u64) }).map(|_| ())
}

pub fn endpoint_create() -> Result<u32> {
    check(unsafe { syscall1(ENDPOINT_CREATE, 0) }).map(|v| v as u32)
}

pub fn notify_create() -> Result<u32> {
    check(unsafe { syscall0(NOTIFY_CREATE) }).map(|v| v as u32)
}

pub fn notify_signal(h: u32, bits: u64) -> Result<()> {
    check(unsafe { syscall2(NOTIFY_SIGNAL, h as u64, bits) }).map(|_| ())
}

/// Bloque jusqu'a ce qu'au moins un bit soit signale ; rend les bits.
pub fn notify_wait(h: u32) -> Result<u64> {
    check(unsafe { syscall2(NOTIFY_WAIT, h as u64, 0) })
}

/// Comme `notify_wait`, au plus `timeout_ns`.
pub fn notify_wait_timeout(h: u32, timeout_ns: u64) -> Result<u64> {
    check(unsafe { syscall2(NOTIFY_WAIT, h as u64, timeout_ns) })
}

pub fn notify_bind(h: u32) -> Result<()> {
    check(unsafe { syscall1(NOTIFY_BIND, h as u64) }).map(|_| ())
}

pub fn irq_get(ctl: u32, intid: u32) -> Result<u32> {
    check(unsafe { syscall2(IRQ_GET, ctl as u64, intid as u64) }).map(|v| v as u32)
}

pub fn irq_bind(irq: u32, notif: u32, bits: u64) -> Result<()> {
    check(unsafe { syscall3(IRQ_BIND, irq as u64, notif as u64, bits) }).map(|_| ())
}

pub fn irq_ack(irq: u32) -> Result<()> {
    check(unsafe { syscall1(IRQ_ACK, irq as u64) }).map(|_| ())
}

pub fn sysinfo(kind: u64, buf: &mut [u8]) -> Result<u64> {
    check(unsafe { syscall3(SYSINFO, kind, buf.as_mut_ptr() as u64, buf.len() as u64) })
}

// -------------------------------------------------------------- debogage ---

/// Attache un debogueur (endpoint `ep`, 0 = detacher) au processus `proc_h`.
pub fn debug_attach(proc_h: u32, ep: u32) -> Result<()> {
    check(unsafe { syscall2(DEBUG_ATTACH, proc_h as u64, ep as u64) }).map(|_| ())
}

pub fn debug_regs_read(proc_h: u32, tid: u32) -> Result<nxk_abi::debug::Regs> {
    let mut r = nxk_abi::debug::Regs::default();
    check(unsafe { syscall4(DEBUG_REGS, proc_h as u64, tid as u64, &mut r as *mut _ as u64, 0) })?;
    Ok(r)
}

pub fn debug_regs_write(proc_h: u32, tid: u32, r: &nxk_abi::debug::Regs) -> Result<()> {
    check(unsafe { syscall4(DEBUG_REGS, proc_h as u64, tid as u64, r as *const _ as u64, 1) }).map(|_| ())
}

pub fn debug_mem_read(proc_h: u32, addr: u64, buf: &mut [u8]) -> Result<()> {
    check(unsafe { syscall4(DEBUG_MEM, proc_h as u64, addr, buf.as_mut_ptr() as u64, buf.len() as u64) })
        .map(|_| ())
}

pub fn debug_mem_write(proc_h: u32, addr: u64, data: &[u8]) -> Result<()> {
    let len = data.len() as u64 | 1 << 63;
    check(unsafe { syscall4(DEBUG_MEM, proc_h as u64, addr, data.as_ptr() as u64, len) }).map(|_| ())
}

/// Identifiants des threads du processus ; rend leur nombre total.
pub fn debug_threads(proc_h: u32, buf: &mut [u32]) -> Result<usize> {
    check(unsafe { syscall3(DEBUG_THREADS, proc_h as u64, buf.as_mut_ptr() as u64, buf.len() as u64) })
        .map(|n| n as usize)
}

/// Arrete un thread (`tid`, 0 = tous) du processus debogue.
pub fn debug_stop(proc_h: u32, tid: u32) -> Result<()> {
    check(unsafe { syscall2(DEBUG_STOP, proc_h as u64, tid as u64) }).map(|_| ())
}

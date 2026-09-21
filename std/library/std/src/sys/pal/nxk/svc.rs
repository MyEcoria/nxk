//! Services du systeme : console et gestionnaire de processus (handles recus
//! au demarrage), puis fichiers et reseau (trouves par le service de noms
//! d'init au premier usage, et gardes).

use super::abi::err::{ENOSYS, ETIMEDOUT};
use super::abi::proto::pm;
use super::ipc::{self, Msg};
use crate::io;
use crate::sync::atomic::{AtomicU32, Ordering};

static CONSOLE: AtomicU32 = AtomicU32::new(0);
static PM: AtomicU32 = AtomicU32::new(0);
static FS: AtomicU32 = AtomicU32::new(0);
static NET: AtomicU32 = AtomicU32::new(0);

pub fn set_console(h: u32) {
    CONSOLE.store(h, Ordering::Relaxed);
}

/// Endpoint du serveur console (0 : aucun, sortie par le noyau).
pub fn console() -> u32 {
    CONSOLE.load(Ordering::Relaxed)
}

/// Entree standard et sortie d'erreurs (emplacements fournis par init aux
/// programmes recents ; sinon la console, comme la sortie).
pub fn stdin() -> u32 {
    if super::startup_flags() & super::abi::startup::HAS_STDIO != 0 { super::abi::slots::STDIN } else { console() }
}

pub fn stderr() -> u32 {
    if super::startup_flags() & super::abi::startup::HAS_STDIO != 0 { super::abi::slots::STDERR } else { console() }
}

pub fn set_pm(h: u32) {
    PM.store(h, Ordering::Relaxed);
}

/// Endpoint du gestionnaire de processus (0 : aucun).
pub fn pm() -> u32 {
    PM.load(Ordering::Relaxed)
}

/// Appel a un service : un label de reponse non nul est un errno.
pub fn call(h: u32, m: &Msg) -> io::Result<Msg> {
    let r = ipc::call(h, m).map_err(|e| io::Error::from_raw_os_error((-e) as i32))?;
    if r.label() != 0 {
        return Err(io::Error::from_raw_os_error(r.label() as i32));
    }
    Ok(r)
}

/// Handle d'un service publie, en attendant sa publication au plus
/// `timeout_ns`.
fn lookup_wait(name: &str, timeout_ns: u64) -> io::Result<u32> {
    let h = pm();
    if h == 0 {
        return Err(io::Error::from_raw_os_error(ENOSYS as i32));
    }
    let (m, _) = ipc::pack(pm::LOOKUP_WAIT, name.as_bytes());
    let mut rx = [0u64; 6];
    let (r0, t): (i64, u64);
    unsafe {
        core::arch::asm!("svc #0",
            inlateout("x0") h as i64 => r0,
            inlateout("x1") m.tag => t,
            inlateout("x2") m.mr[0] => rx[0],
            inlateout("x3") m.mr[1] => rx[1],
            inlateout("x4") m.mr[2] => rx[2],
            inlateout("x5") m.mr[3] => rx[3],
            inlateout("x6") m.mr[4] => rx[4],
            inlateout("x7") m.mr[5] => rx[5],
            inlateout("x8") super::abi::sys::IPC_CALL => _,
            inlateout("x9") timeout_ns => _,
            lateout("x10") _,
            options(nostack));
    }
    if r0 < 0 {
        return Err(io::Error::from_raw_os_error((-r0) as i32));
    }
    let label = super::abi::tag::label(t);
    if label != 0 {
        return Err(io::Error::from_raw_os_error(label as i32));
    }
    Ok(ipc::received_handle(0))
}

fn cached(slot: &AtomicU32, name: &str, timeout_ns: u64) -> io::Result<u32> {
    let h = slot.load(Ordering::Relaxed);
    if h != 0 {
        return Ok(h);
    }
    let h = lookup_wait(name, timeout_ns).map_err(|e| {
        if e.raw_os_error() == Some(ETIMEDOUT as i32) {
            io::Error::new(io::ErrorKind::NotFound, "service absent")
        } else {
            e
        }
    })?;
    // Deux threads peuvent chercher en meme temps : un seul handle est garde.
    match slot.compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(h),
        Err(other) => {
            super::syscall::handle_close(h);
            Ok(other)
        }
    }
}

/// Service de fichiers.
pub fn fs() -> io::Result<u32> {
    cached(&FS, "fs", 2_000_000_000)
}

/// Pile reseau.
pub fn net() -> io::Result<u32> {
    cached(&NET, "net", 3_000_000_000)
}

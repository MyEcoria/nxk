//! System services: console + process manager from startup; files/network via init lookup, cached.

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

/// Console server endpoint (0: none, kernel log fallback).
pub fn console() -> u32 {
    CONSOLE.load(Ordering::Relaxed)
}

/// Stdin and stderr (slots passed by init to recent
/// programs; else the console, like stdout).
pub fn stdin() -> u32 {
    if super::startup_flags() & super::abi::startup::HAS_STDIO != 0 { super::abi::slots::STDIN } else { console() }
}

pub fn stderr() -> u32 {
    if super::startup_flags() & super::abi::startup::HAS_STDIO != 0 { super::abi::slots::STDERR } else { console() }
}

pub fn set_pm(h: u32) {
    PM.store(h, Ordering::Relaxed);
}

/// Process manager endpoint (0: none).
pub fn pm() -> u32 {
    PM.load(Ordering::Relaxed)
}

/// Kernel `-errno` or a nonzero reply label (an errno) as `io::Error`.
fn checked(r: Result<Msg, i64>) -> io::Result<Msg> {
    let r = r.map_err(|e| io::Error::from_raw_os_error((-e) as i32))?;
    if r.label() != 0 {
        return Err(io::Error::from_raw_os_error(r.label() as i32));
    }
    Ok(r)
}

/// Service call: a nonzero reply label is an errno.
pub fn call(h: u32, m: &Msg) -> io::Result<Msg> {
    checked(ipc::call(h, m))
}

/// Published service handle, waiting for publication up to
/// `timeout_ns`.
fn lookup_wait(name: &str, timeout_ns: u64) -> io::Result<u32> {
    let h = pm();
    if h == 0 {
        return Err(io::Error::from_raw_os_error(ENOSYS as i32));
    }
    let (m, _) = ipc::pack(pm::LOOKUP_WAIT, name.as_bytes());
    checked(ipc::call_timeout(h, &m, timeout_ns)).map(|_| ipc::received_handle(0))
}

fn cached(slot: &AtomicU32, name: &str, timeout_ns: u64) -> io::Result<u32> {
    let h = slot.load(Ordering::Relaxed);
    if h != 0 {
        return Ok(h);
    }
    let h = lookup_wait(name, timeout_ns).map_err(|e| {
        if e.raw_os_error() == Some(ETIMEDOUT as i32) {
            io::Error::new(io::ErrorKind::NotFound, "service missing")
        } else {
            e
        }
    })?;
    // Two threads may race: only one handle is kept.
    match slot.compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(h),
        Err(other) => {
            super::syscall::handle_close(h);
            Ok(other)
        }
    }
}

/// File service.
pub fn fs() -> io::Result<u32> {
    cached(&FS, "fs", 2_000_000_000)
}

/// Network stack.
pub fn net() -> io::Result<u32> {
    cached(&NET, "net", 3_000_000_000)
}

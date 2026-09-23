//! Name service: services publish to init, programs look them up by
//! name. Replies carry a SEND handle to the service endpoint.

use core::sync::atomic::{AtomicU32, Ordering};

use nxk_abi::err::{ENOSYS, EPIPE};
use nxk_abi::proto::pm;

use crate::ipc::{self, Msg};
use crate::{process, sys};

/// Name request to init, waiting at most `timeout_ns` (0: no limit).
fn call_with_name(
    label: u32,
    name: &str,
    handle: Option<u32>,
    timeout_ns: u64,
) -> Result<Msg, i64> {
    let h = process::pm_handle();
    if h == 0 {
        return Err(-ENOSYS);
    }
    let mut m = ipc::pack(label, name.as_bytes());
    if let Some(ep) = handle {
        ipc::set_send_handle(0, ep);
        m = m.with_handles(1);
    }
    ipc::checked(ipc::call_timeout(h, &m, timeout_ns)?)
}

/// Publish `ep` as `name` (only for init-spawned services).
pub fn register(name: &str, ep: u32) -> Result<(), i64> {
    call_with_name(pm::REGISTER, name, Some(ep), 0).map(|_| ())
}

/// Handle to service `name`, if published.
pub fn lookup(name: &str) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP, name, None, 0).map(|_| ipc::received_handle(0))
}

/// Like `lookup`, waiting until the service is published.
pub fn wait_for(name: &str) -> Result<u32, i64> {
    wait_for_timeout(name, 0)
}

/// Like `wait_for`, at most `timeout_ns` (-ETIMEDOUT past it). A later
/// publication cannot reach this abandoned call.
pub fn wait_for_timeout(name: &str, timeout_ns: u64) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP_WAIT, name, None, timeout_ns).map(|_| ipc::received_handle(0))
}

/// Published service names, space-separated.
pub fn services(buf: &mut [u8]) -> usize {
    let h = process::pm_handle();
    if h == 0 {
        return 0;
    }
    match ipc::call(h, &Msg::new(pm::SERVICES, 0, 0)) {
        Ok(m) => ipc::unpack(&m, buf),
        Err(_) => 0,
    }
}

/// A service looked up on first use and kept. A dead endpoint (the
/// service was restarted) is dropped, so the next call looks it up again.
pub struct Service {
    name: &'static str,
    /// How long the first lookup waits for publication (0: no limit,
    /// `NO_WAIT`: not at all).
    wait_ns: u64,
    handle: AtomicU32,
}

const NO_WAIT: u64 = u64::MAX;

impl Service {
    pub const fn new(name: &'static str, wait_ns: u64) -> Self {
        Self {
            name,
            wait_ns,
            handle: AtomicU32::new(0),
        }
    }

    /// Looked up without waiting: calls fail with `-ENOSYS` until the
    /// service is published.
    pub const fn optional(name: &'static str) -> Self {
        Self::new(name, NO_WAIT)
    }

    /// Use `h` rather than a lookup (init cannot look itself up).
    pub fn set(&self, h: u32) {
        self.handle.store(h, Ordering::Relaxed);
    }

    /// Endpoint handle; `-ENOSYS` if the service never showed up.
    pub fn handle(&self) -> Result<u32, i64> {
        let h = self.handle.load(Ordering::Relaxed);
        if h != 0 {
            return Ok(h);
        }
        let h = match self.wait_ns {
            NO_WAIT => lookup(self.name),
            ns => wait_for_timeout(self.name, ns),
        }
        .map_err(|_| -ENOSYS)?;
        // Two threads may race: only one handle is kept.
        match self
            .handle
            .compare_exchange(0, h, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(h),
            Err(other) => {
                let _ = sys::handle_close(h);
                Ok(other)
            }
        }
    }

    /// Call, at most `timeout_ns` (0: no limit); the reply label checked.
    pub fn call_timeout(&self, m: &Msg, timeout_ns: u64) -> Result<Msg, i64> {
        let h = self.handle()?;
        let r = ipc::call_timeout(h, m, timeout_ns);
        if matches!(r, Err(e) if e == -EPIPE)
            && self
                .handle
                .compare_exchange(h, 0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let _ = sys::handle_close(h);
        }
        ipc::checked(r?)
    }

    pub fn call(&self, m: &Msg) -> Result<Msg, i64> {
        self.call_timeout(m, 0)
    }
}

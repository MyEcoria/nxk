//! Name service: services publish to init, programs look them up by
//! name. Replies carry a SEND handle to the service endpoint.

use nxk_abi::proto::pm;

use crate::{ipc, process};

fn call_with_name(label: u32, name: &str, handle: Option<u32>) -> Result<ipc::Msg, i64> {
    let h = process::pm_handle();
    if h == 0 {
        return Err(-nxk_abi::err::ENOSYS);
    }
    let mut m = ipc::pack(label, name.as_bytes());
    if let Some(ep) = handle {
        ipc::set_send_handle(0, ep);
        m = m.with_handles(1);
    }
    ipc::call_checked(h, &m)
}

/// Publish `ep` as `name` (only for init-spawned services).
pub fn register(name: &str, ep: u32) -> Result<(), i64> {
    call_with_name(pm::REGISTER, name, Some(ep)).map(|_| ())
}

/// Handle to service `name`, if published.
pub fn lookup(name: &str) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP, name, None).map(|_| ipc::received_handle(0))
}

/// Like `lookup`, waiting until the service is published.
pub fn wait_for(name: &str) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP_WAIT, name, None).map(|_| ipc::received_handle(0))
}

/// Like `wait_for`, at most `timeout_ns` (-ETIMEDOUT past it). A later
/// publication cannot reach this abandoned call.
pub fn wait_for_timeout(name: &str, timeout_ns: u64) -> Result<u32, i64> {
    let h = process::pm_handle();
    if h == 0 {
        return Err(-nxk_abi::err::ENOSYS);
    }
    let r = ipc::call_timeout(h, &ipc::pack(pm::LOOKUP_WAIT, name.as_bytes()), timeout_ns)?;
    ipc::checked(r).map(|_| ipc::received_handle(0))
}

/// Published service names, space-separated.
pub fn services(buf: &mut [u8]) -> usize {
    let h = process::pm_handle();
    if h == 0 {
        return 0;
    }
    match ipc::call(h, &ipc::Msg::new(pm::SERVICES, 0, 0)) {
        Ok(m) => ipc::unpack(&m, buf),
        Err(_) => 0,
    }
}

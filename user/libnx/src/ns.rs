//! Service de noms : les services se publient aupres d'init, les programmes
//! les demandent par leur nom (« fs », « net »...). La reponse porte un
//! handle SEND vers l'endpoint du service.

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
    let r = ipc::call(h, &m)?;
    if r.label() != 0 {
        return Err(-(r.label() as i64));
    }
    Ok(r)
}

/// Publie `ep` sous `name` (reserve aux services lances par init).
pub fn register(name: &str, ep: u32) -> Result<(), i64> {
    call_with_name(pm::REGISTER, name, Some(ep)).map(|_| ())
}

/// Handle vers le service `name`, s'il est publie.
pub fn lookup(name: &str) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP, name, None).map(|_| ipc::received_handle(0))
}

/// Comme `lookup`, en attendant que le service soit publie.
pub fn wait_for(name: &str) -> Result<u32, i64> {
    call_with_name(pm::LOOKUP_WAIT, name, None).map(|_| ipc::received_handle(0))
}

/// Comme `wait_for`, au plus `timeout_ns` (-ETIMEDOUT au-dela). Une
/// publication plus tardive ne pourra pas atteindre cet appel abandonne.
pub fn wait_for_timeout(name: &str, timeout_ns: u64) -> Result<u32, i64> {
    let h = process::pm_handle();
    if h == 0 {
        return Err(-nxk_abi::err::ENOSYS);
    }
    let r = ipc::call_timeout(h, &ipc::pack(pm::LOOKUP_WAIT, name.as_bytes()), timeout_ns)?;
    if r.label() != 0 {
        return Err(-(r.label() as i64));
    }
    Ok(ipc::received_handle(0))
}

/// Noms des services publies, separes par des espaces.
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

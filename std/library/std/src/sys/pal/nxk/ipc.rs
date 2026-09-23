//! Sync IPC to services (client): label + words; first 6 in x2-x7, rest in thread UTCB.
//! The calling convention itself lives in `abi::user`, shared with libnx.

use super::abi::sys;
use super::abi::user;

pub use super::abi::user::{
    Msg, bytes_len, header, pack_at, received_handle, set_send_handle, unpack_at, MAX_BYTES,
};

/// Send `m` to endpoint `ep` and wait for the reply. Error: kernel
/// `-errno` (dead endpoint, timeout...).
#[inline]
pub fn call(ep: u32, m: &Msg) -> Result<Msg, i64> {
    user::call(ep, m, 0)
}

/// Like `call`, at most `timeout_ns` overall (server wait plus reply).
#[inline]
pub fn call_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Msg, i64> {
    user::call(ep, m, timeout_ns)
}

/// Wait for a message on `ep` (RECV right). Error: kernel `-errno`
/// (-EPIPE: pipe without writer).
#[inline]
pub fn recv(ep: u32) -> Result<Msg, i64> {
    user::ipc_checked(sys::IPC_RECV, ep as u64, &Msg::default(), 0).map(|r| r.msg)
}

/// Reply to the last message this thread received.
#[inline]
pub fn reply(m: &Msg) -> Result<(), i64> {
    user::ipc_checked(sys::IPC_REPLY, 0, m, 0).map(|_| ())
}

pub fn pack(label: u32, data: &[u8]) -> (Msg, usize) {
    pack_at(label, &[], data)
}

pub fn unpack(m: &Msg, out: &mut [u8]) -> usize {
    unpack_at(m, 0, out)
}

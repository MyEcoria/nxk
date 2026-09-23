//! IPC: synchronous server messages (tag + 6 words in regs).
//! Overflow words go through the thread UTCB (`nxk_abi::user`).

use nxk_abi::sys;
use nxk_abi::user::{self, ipc_checked};

pub use nxk_abi::user::{
    header, received_handle, set_send_handle, unpack_at, Msg, Received, MAX_BYTES,
};

/// Send a request and wait for the reply.
#[inline]
pub fn call(ep: u32, m: &Msg) -> Result<Msg, i64> {
    user::call(ep, m, 0)
}

/// Like `call`, at most `timeout_ns` overall (server wait plus
/// reply): `Err(-ETIMEDOUT)` past it.
#[inline]
pub fn call_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Msg, i64> {
    user::call(ep, m, timeout_ns)
}

#[inline]
pub fn send(ep: u32, m: &Msg) -> Result<(), i64> {
    send_timeout(ep, m, 0)
}

#[inline]
pub fn send_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<(), i64> {
    ipc_checked(sys::IPC_SEND, ep as u64, m, timeout_ns).map(|_| ())
}

#[inline]
pub fn recv(ep: u32) -> Result<Received, i64> {
    recv_timeout(ep, 0)
}

#[inline]
pub fn recv_timeout(ep: u32, timeout_ns: u64) -> Result<Received, i64> {
    ipc_checked(sys::IPC_RECV, ep as u64, &Msg::default(), timeout_ns)
}

#[inline]
pub fn reply(m: &Msg) -> Result<(), i64> {
    ipc_checked(sys::IPC_REPLY, 0, m, 0).map(|_| ())
}

/// Reply to the current client and wait for the next request in one call.
#[inline]
pub fn reply_recv(ep: u32, m: &Msg) -> Result<Received, i64> {
    reply_recv_timeout(ep, m, 0)
}

#[inline]
pub fn reply_recv_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Received, i64> {
    ipc_checked(sys::IPC_REPLY_RECV, ep as u64, m, timeout_ns)
}

/// Deferred reply to a client identified by its token.
#[inline]
pub fn reply_to(token: u64, m: &Msg) -> Result<(), i64> {
    ipc_checked(sys::IPC_REPLY_TO, token, m, 0).map(|_| ())
}

/// Check an error-labelled reply: `Ok(msg)` on label 0, `-errno` otherwise.
#[inline]
pub fn checked(r: Msg) -> Result<Msg, i64> {
    if r.label() != 0 {
        Err(-(r.label() as i64))
    } else {
        Ok(r)
    }
}

/// Call a service endpoint and check the reply label.
#[inline]
pub fn call_checked(ep: u32, m: &Msg) -> Result<Msg, i64> {
    checked(call(ep, m)?)
}

/// Build a byte message (registers then UTCB).
pub fn pack(label: u32, data: &[u8]) -> Msg {
    pack_at(label, &[], data)
}

/// Message of `head` header words plus bytes: one length word then the bytes.
pub fn pack_at(label: u32, head: &[u64], data: &[u8]) -> Msg {
    user::pack_at(label, head, data).0
}

/// Extract the bytes of a `pack`-built message.
pub fn unpack(m: &Msg, out: &mut [u8]) -> usize {
    unpack_at(m, 0, out)
}

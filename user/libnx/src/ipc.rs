//! IPC: synchronous server messages (tag + 6 words in regs).
//! Overflow words go through the thread UTCB.

use core::arch::asm;

use nxk_abi::sys;
use nxk_abi::tag;
use nxk_abi::utcb::{Utcb, MR_WORDS};

#[derive(Clone, Copy, Default, Debug)]
pub struct Msg {
    pub tag: u64,
    pub mr: [u64; 6],
}

impl Msg {
    pub const fn new(label: u32, words: usize, handles: usize) -> Self {
        Self {
            tag: tag::make(label, words, handles),
            mr: [0; 6],
        }
    }
    pub const fn label(&self) -> u32 {
        tag::label(self.tag)
    }
    pub const fn words(&self) -> usize {
        tag::words(self.tag)
    }
    pub const fn handles(&self) -> usize {
        tag::handles(self.tag)
    }
    /// Synthetic message: the thread-bound notification fired.
    pub const fn is_notify(&self) -> bool {
        self.tag & tag::FLAG_NOTIFY != 0
    }

    /// Same message plus `n` handles (set via `set_send_handle`).
    pub const fn with_handles(self, n: usize) -> Self {
        Self {
            tag: tag::make(self.label(), self.words(), n),
            mr: self.mr,
        }
    }

    /// Error reply (positive errno in the label).
    pub const fn error(e: i64) -> Self {
        Self::new(e.unsigned_abs() as u32, 0, 0)
    }
}

/// Received message: contents, sender badge, deferred-reply token,
/// sender pid (set by the kernel; 0 for a notification).
#[derive(Clone, Copy, Default, Debug)]
pub struct Received {
    pub msg: Msg,
    pub badge: u64,
    pub token: u64,
    pub pid: u32,
}

/// Current thread UTCB (address in TPIDRRO_EL0, no syscall).
#[inline(always)]
pub fn utcb() -> &'static mut Utcb {
    let p: u64;
    unsafe { asm!("mrs {}, tpidrro_el0", out(reg) p, options(nomem, nostack)) };
    unsafe { &mut *(p as *mut Utcb) }
}

/// `timeout` (ns, 0 = no limit) travels in x9; the token comes back in x9.
#[inline(always)]
fn raw(nr: u64, x0: u64, m: &Msg, timeout: u64) -> (i64, Received) {
    let (r0, t, a, b, c, d, e, f, badge, token, pid): (
        i64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
    );
    unsafe {
        asm!("svc #0",
            inlateout("x0") x0 as i64 => r0,
            inlateout("x1") m.tag => t,
            inlateout("x2") m.mr[0] => a,
            inlateout("x3") m.mr[1] => b,
            inlateout("x4") m.mr[2] => c,
            inlateout("x5") m.mr[3] => d,
            inlateout("x6") m.mr[4] => e,
            inlateout("x7") m.mr[5] => f,
            inlateout("x8") nr => badge,
            inlateout("x9") timeout => token,
            lateout("x10") pid,
            options(nostack));
    }
    (
        r0,
        Received {
            msg: Msg {
                tag: t,
                mr: [a, b, c, d, e, f],
            },
            badge,
            token,
            pid: pid as u32,
        },
    )
}

#[inline(always)]
fn result(r: (i64, Received)) -> Result<Received, i64> {
    if r.0 < 0 {
        Err(r.0)
    } else {
        Ok(r.1)
    }
}

/// Send a request and wait for the reply.
#[inline]
pub fn call(ep: u32, m: &Msg) -> Result<Msg, i64> {
    result(raw(sys::IPC_CALL, ep as u64, m, 0)).map(|r| r.msg)
}

/// Like `call`, at most `timeout_ns` overall (server wait plus
/// reply): `Err(-ETIMEDOUT)` past it.
#[inline]
pub fn call_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Msg, i64> {
    result(raw(sys::IPC_CALL, ep as u64, m, timeout_ns)).map(|r| r.msg)
}

#[inline]
pub fn send(ep: u32, m: &Msg) -> Result<(), i64> {
    result(raw(sys::IPC_SEND, ep as u64, m, 0)).map(|_| ())
}

#[inline]
pub fn send_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<(), i64> {
    result(raw(sys::IPC_SEND, ep as u64, m, timeout_ns)).map(|_| ())
}

#[inline]
pub fn recv(ep: u32) -> Result<Received, i64> {
    result(raw(sys::IPC_RECV, ep as u64, &Msg::default(), 0))
}

#[inline]
pub fn recv_timeout(ep: u32, timeout_ns: u64) -> Result<Received, i64> {
    result(raw(sys::IPC_RECV, ep as u64, &Msg::default(), timeout_ns))
}

#[inline]
pub fn reply(m: &Msg) -> Result<(), i64> {
    result(raw(sys::IPC_REPLY, 0, m, 0)).map(|_| ())
}

/// Reply to the current client and wait for the next request in one call.
#[inline]
pub fn reply_recv(ep: u32, m: &Msg) -> Result<Received, i64> {
    result(raw(sys::IPC_REPLY_RECV, ep as u64, m, 0))
}

#[inline]
pub fn reply_recv_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Received, i64> {
    result(raw(sys::IPC_REPLY_RECV, ep as u64, m, timeout_ns))
}

/// Deferred reply to a client identified by its token.
#[inline]
pub fn reply_to(token: u64, m: &Msg) -> Result<(), i64> {
    result(raw(sys::IPC_REPLY_TO, token, m, 0)).map(|_| ())
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

/// Bytes per message: word 0 = length, bytes follow.
pub const MAX_BYTES: usize = (MR_WORDS - 1) * 8;

/// Build a byte message (registers then UTCB).
pub fn pack(label: u32, data: &[u8]) -> Msg {
    pack_at(label, &[], data)
}

/// Word `i` of a message (registers or UTCB).
#[inline]
fn word(m: &Msg, i: usize) -> u64 {
    if i < tag::REG_WORDS {
        m.mr[i]
    } else {
        utcb().mr[i]
    }
}

#[inline]
fn set_word(m: &mut Msg, i: usize, v: u64) {
    if i < tag::REG_WORDS {
        m.mr[i] = v;
    } else {
        utcb().mr[i] = v;
    }
}

/// Message of `head` header words plus bytes: one length word then the bytes.
pub fn pack_at(label: u32, head: &[u64], data: &[u8]) -> Msg {
    let first = head.len();
    let n = data.len().min((MR_WORDS - 1 - first) * 8);
    let words = first + 1 + n.div_ceil(8);
    let mut m = Msg::new(label, words, 0);
    for (i, &w) in head.iter().enumerate() {
        set_word(&mut m, i, w);
    }
    set_word(&mut m, first, n as u64);
    for (i, chunk) in data[..n].chunks(8).enumerate() {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        set_word(&mut m, first + 1 + i, u64::from_le_bytes(w));
    }
    m
}

/// Extract the bytes of a `pack`-built message.
pub fn unpack(m: &Msg, out: &mut [u8]) -> usize {
    unpack_at(m, 0, out)
}

/// Bytes of a `pack_at`-built message with `first` header words.
pub fn unpack_at(m: &Msg, first: usize, out: &mut [u8]) -> usize {
    if m.words() <= first {
        return 0;
    }
    let n = (word(m, first) as usize)
        .min(out.len())
        .min((m.words() - first - 1) * 8);
    let mut done = 0;
    let mut wi = first + 1;
    while done < n {
        let b = word(m, wi).to_le_bytes();
        let k = (n - done).min(8);
        out[done..done + k].copy_from_slice(&b[..k]);
        done += k;
        wi += 1;
    }
    n
}

/// Header word `i` of a received message (register or UTCB).
pub fn header(m: &Msg, i: usize) -> u64 {
    word(m, i)
}

/// Handles received with the last message.
pub fn received_handle(k: usize) -> u32 {
    utcb().handles[k]
}

/// Stage handles for the next message send.
pub fn set_send_handle(k: usize, h: u32) {
    utcb().handles[k] = h;
}

//! Sync IPC to services (client): label + words; first 6 in x2-x7, rest in thread UTCB.
//! (UTCB: per-thread page at `TPIDRRO_EL0`, copied by the kernel).

use core::arch::asm;

use super::abi::utcb::{MR_WORDS, Utcb};
use super::abi::{sys, tag};

#[derive(Clone, Copy, Default, Debug)]
pub struct Msg {
    pub tag: u64,
    pub mr: [u64; 6],
}

impl Msg {
    pub const fn new(label: u32, words: usize, handles: usize) -> Self {
        Self { tag: tag::make(label, words, handles), mr: [0; 6] }
    }

    /// Words-only message (at most 6).
    pub fn words(label: u32, w: &[u64]) -> Self {
        let mut m = Self::new(label, w.len(), 0);
        m.mr[..w.len()].copy_from_slice(w);
        m
    }

    pub const fn label(&self) -> u32 {
        tag::label(self.tag)
    }

    pub const fn len(&self) -> usize {
        tag::words(self.tag)
    }
}

#[inline(always)]
fn utcb() -> *mut Utcb {
    let p: u64;
    unsafe { asm!("mrs {}, tpidrro_el0", out(reg) p, options(nomem, nostack, preserves_flags)) };
    p as *mut Utcb
}

/// Send `m` to endpoint `ep` and wait for the reply. Error: kernel
/// `-errno` (dead endpoint, timeout...).
#[inline]
pub fn call(ep: u32, m: &Msg) -> Result<Msg, i64> {
    let (r0, t, a, b, c, d, e, f): (i64, u64, u64, u64, u64, u64, u64, u64);
    unsafe {
        asm!("svc #0",
            inlateout("x0") ep as i64 => r0,
            inlateout("x1") m.tag => t,
            inlateout("x2") m.mr[0] => a,
            inlateout("x3") m.mr[1] => b,
            inlateout("x4") m.mr[2] => c,
            inlateout("x5") m.mr[3] => d,
            inlateout("x6") m.mr[4] => e,
            inlateout("x7") m.mr[5] => f,
            inlateout("x8") sys::IPC_CALL => _,
            inlateout("x9") 0u64 => _,
            lateout("x10") _,
            options(nostack));
    }
    if r0 < 0 { Err(r0) } else { Ok(Msg { tag: t, mr: [a, b, c, d, e, f] }) }
}

/// Wait for a message on `ep` (RECV right). Error: kernel `-errno`
/// (-EPIPE: pipe without writer).
#[inline]
pub fn recv(ep: u32) -> Result<Msg, i64> {
    let (r0, t, a, b, c, d, e, f): (i64, u64, u64, u64, u64, u64, u64, u64);
    unsafe {
        asm!("svc #0",
            inlateout("x0") ep as i64 => r0,
            lateout("x1") t,
            lateout("x2") a,
            lateout("x3") b,
            lateout("x4") c,
            lateout("x5") d,
            lateout("x6") e,
            lateout("x7") f,
            inlateout("x8") sys::IPC_RECV => _,
            inlateout("x9") 0u64 => _,
            lateout("x10") _,
            options(nostack));
    }
    if r0 < 0 { Err(r0) } else { Ok(Msg { tag: t, mr: [a, b, c, d, e, f] }) }
}

/// Reply to the last message this thread received.
#[inline]
pub fn reply(m: &Msg) -> Result<(), i64> {
    let r0: i64;
    unsafe {
        asm!("svc #0",
            inlateout("x0") 0i64 => r0,
            inlateout("x1") m.tag => _,
            inlateout("x2") m.mr[0] => _,
            inlateout("x3") m.mr[1] => _,
            inlateout("x4") m.mr[2] => _,
            inlateout("x5") m.mr[3] => _,
            inlateout("x6") m.mr[4] => _,
            inlateout("x7") m.mr[5] => _,
            inlateout("x8") sys::IPC_REPLY => _,
            inlateout("x9") 0u64 => _,
            lateout("x10") _,
            options(nostack));
    }
    if r0 < 0 { Err(r0) } else { Ok(()) }
}

/// Bytes per message: one length word, then the bytes.
pub const MAX_BYTES: usize = (MR_WORDS - 1) * 8;

#[inline]
fn word(m: &Msg, i: usize) -> u64 {
    if i < tag::REG_WORDS { m.mr[i] } else { unsafe { (*utcb()).mr[i] } }
}

#[inline]
fn set_word(m: &mut Msg, i: usize, v: u64) {
    if i < tag::REG_WORDS {
        m.mr[i] = v;
    } else {
        unsafe { (*utcb()).mr[i] = v };
    }
}

/// Header word `i` of a reply (register or UTCB).
pub fn header(m: &Msg, i: usize) -> u64 {
    word(m, i)
}

/// Message: `head` header words, then a length word and the bytes
/// (truncated to what fits). Also returns the carried byte count.
pub fn pack_at(label: u32, head: &[u64], data: &[u8]) -> (Msg, usize) {
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
    (m, n)
}

pub fn pack(label: u32, data: &[u8]) -> (Msg, usize) {
    pack_at(label, &[], data)
}

/// Bytes of a reply built with `first` header words.
pub fn unpack_at(m: &Msg, first: usize, out: &mut [u8]) -> usize {
    if m.len() <= first {
        return 0;
    }
    let n = (word(m, first) as usize).min(out.len()).min((m.len() - first - 1) * 8);
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

pub fn unpack(m: &Msg, out: &mut [u8]) -> usize {
    unpack_at(m, 0, out)
}

/// Advertised byte length of a reply (no copy).
pub fn bytes_len(m: &Msg, first: usize) -> usize {
    if m.len() <= first { 0 } else { (word(m, first) as usize).min((m.len() - first - 1) * 8) }
}

/// Handle received with the last reply.
pub fn received_handle(k: usize) -> u32 {
    unsafe { (*utcb()).handles[k] }
}

/// Handle to send with the next message (GRANT right needed).
pub fn set_send_handle(k: usize, h: u32) {
    unsafe { (*utcb()).handles[k] = h };
}

impl Msg {
    /// Same message plus `n` handles (set via `set_send_handle`).
    pub const fn with_handles(self, n: usize) -> Self {
        Self { tag: tag::make(self.label(), self.len(), n), mr: self.mr }
    }
}

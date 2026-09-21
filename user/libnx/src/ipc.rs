//! IPC : messages synchrones vers et depuis les serveurs.
//!
//! Un message = une etiquette + 6 mots en registres (chemin rapide) ; au-dela,
//! les mots sont lus et ecrits dans l'UTCB du thread, que le noyau recopie.

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
        Self { tag: tag::make(label, words, handles), mr: [0; 6] }
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
    /// Message synthetique : la notification liee au thread a ete signalee.
    pub const fn is_notify(&self) -> bool {
        self.tag & tag::FLAG_NOTIFY != 0
    }

    /// Meme message, accompagne de `n` handles (poses par `set_send_handle`).
    pub const fn with_handles(self, n: usize) -> Self {
        Self { tag: tag::make(self.label(), self.words(), n), mr: self.mr }
    }

    /// Reponse d'erreur (errno positif dans l'etiquette).
    pub const fn error(e: i64) -> Self {
        Self::new(e.unsigned_abs() as u32, 0, 0)
    }
}

/// Message recu : contenu, insigne de l'emetteur, jeton de reponse differee,
/// pid de l'emetteur (pose par le noyau ; 0 pour une notification).
#[derive(Clone, Copy, Default, Debug)]
pub struct Received {
    pub msg: Msg,
    pub badge: u64,
    pub token: u64,
    pub pid: u32,
}

/// UTCB du thread courant (adresse dans TPIDRRO_EL0, sans appel systeme).
#[inline(always)]
pub fn utcb() -> &'static mut Utcb {
    let p: u64;
    unsafe { asm!("mrs {}, tpidrro_el0", out(reg) p, options(nomem, nostack)) };
    unsafe { &mut *(p as *mut Utcb) }
}

/// `timeout` (ns, 0 = sans limite) voyage dans x9 ; le jeton revient dans x9.
#[inline(always)]
fn raw(nr: u64, x0: u64, m: &Msg, timeout: u64) -> (i64, Received) {
    let (r0, t, a, b, c, d, e, f, badge, token, pid): (i64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64);
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
    (r0, Received { msg: Msg { tag: t, mr: [a, b, c, d, e, f] }, badge, token, pid: pid as u32 })
}

#[inline(always)]
fn result(r: (i64, Received)) -> Result<Received, i64> {
    if r.0 < 0 {
        Err(r.0)
    } else {
        Ok(r.1)
    }
}

/// Envoie une requete et attend la reponse.
#[inline]
pub fn call(ep: u32, m: &Msg) -> Result<Msg, i64> {
    result(raw(sys::IPC_CALL, ep as u64, m, 0)).map(|r| r.msg)
}

/// Comme `call`, au plus `timeout_ns` pour l'ensemble (attente d'un serveur
/// puis de sa reponse) : `Err(-ETIMEDOUT)` au-dela.
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

/// Repond au client courant et attend la requete suivante, en un seul appel.
#[inline]
pub fn reply_recv(ep: u32, m: &Msg) -> Result<Received, i64> {
    result(raw(sys::IPC_REPLY_RECV, ep as u64, m, 0))
}

#[inline]
pub fn reply_recv_timeout(ep: u32, m: &Msg, timeout_ns: u64) -> Result<Received, i64> {
    result(raw(sys::IPC_REPLY_RECV, ep as u64, m, timeout_ns))
}

/// Reponse differee, a un client identifie par son jeton.
#[inline]
pub fn reply_to(token: u64, m: &Msg) -> Result<(), i64> {
    result(raw(sys::IPC_REPLY_TO, token, m, 0)).map(|_| ())
}

/// Octets transportables par message : mot 0 = longueur, octets ensuite.
pub const MAX_BYTES: usize = (MR_WORDS - 1) * 8;

/// Construit un message d'octets (registres puis UTCB).
pub fn pack(label: u32, data: &[u8]) -> Msg {
    pack_at(label, &[], data)
}

/// Mot `i` d'un message (registres ou UTCB).
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

/// Message de mots d'en-tete `head`, suivis d'octets : un mot de longueur
/// puis les octets.
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

/// Extrait les octets d'un message construit par `pack`.
pub fn unpack(m: &Msg, out: &mut [u8]) -> usize {
    unpack_at(m, 0, out)
}

/// Octets d'un message construit par `pack_at` avec `first` mots d'en-tete.
pub fn unpack_at(m: &Msg, first: usize, out: &mut [u8]) -> usize {
    if m.words() <= first {
        return 0;
    }
    let n = (word(m, first) as usize).min(out.len()).min((m.words() - first - 1) * 8);
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

/// Mot d'en-tete `i` d'un message recu (registre ou UTCB).
pub fn header(m: &Msg, i: usize) -> u64 {
    word(m, i)
}

/// Handles recus avec le dernier message.
pub fn received_handle(k: usize) -> u32 {
    utcb().handles[k]
}

/// Prepare l'envoi de handles avec le prochain message.
pub fn set_send_handle(k: usize, h: u32) {
    utcb().handles[k] = h;
}

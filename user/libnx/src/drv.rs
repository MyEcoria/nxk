//! Outils des pilotes : registres projetes, interruptions, boucle de service.

use core::ptr::{read_volatile, write_volatile};

use nxk_abi::rights::{DUP, GRANT, SEND};
use nxk_abi::slots;

use crate::ipc::{self, Msg, Received};
use crate::{ns, sys};

/// Capacite materielle numero `k` recue d'init.
pub const fn dev_slot(k: u32) -> u32 {
    slots::DEV_BASE + k
}

/// Pages de registres d'un peripherique, projetees dans le processus.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: usize,
}

impl Mmio {
    /// Projette `len` octets a partir de l'adresse physique `phys`.
    pub fn map(cap: u32, phys: u64, len: u64) -> Result<Self, i64> {
        Ok(Self { base: sys::mmio_map(cap, phys, len)? as usize })
    }

    /// Sous-bloc a `off` octets (plusieurs controleurs dans une page).
    pub const fn offset(self, off: usize) -> Self {
        Self { base: self.base + off }
    }

    #[inline(always)]
    pub fn r32(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    #[inline(always)]
    pub fn w32(&self, off: usize, v: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }

    #[inline(always)]
    pub fn r16(&self, off: usize) -> u16 {
        unsafe { read_volatile((self.base + off) as *const u16) }
    }

    #[inline(always)]
    pub fn w16(&self, off: usize, v: u16) {
        unsafe { write_volatile((self.base + off) as *mut u16, v) }
    }

    #[inline(always)]
    pub fn r8(&self, off: usize) -> u8 {
        unsafe { read_volatile((self.base + off) as *const u8) }
    }

    #[inline(always)]
    pub fn w8(&self, off: usize, v: u8) {
        unsafe { write_volatile((self.base + off) as *mut u8, v) }
    }

    #[inline]
    pub fn modify(&self, off: usize, clear: u32, set: u32) {
        self.w32(off, (self.r32(off) & !clear) | set);
    }

    /// Attend que `(r32(off) & mask) == val`, au plus `timeout_us`.
    pub fn wait(&self, off: usize, mask: u32, val: u32, timeout_us: u64) -> bool {
        let end = crate::time::now_ns() + timeout_us * 1000;
        loop {
            if self.r32(off) & mask == val {
                return true;
            }
            if crate::time::now_ns() > end {
                return false;
            }
            core::hint::spin_loop();
        }
    }
}

/// Ligne d'interruption recue d'init, livree par une notification.
pub struct Irq {
    cap: u32,
    notif: u32,
}

impl Irq {
    pub fn new(cap: u32) -> Result<Self, i64> {
        let notif = sys::notify_create()?;
        sys::irq_bind(cap, notif, 1)?;
        Ok(Self { cap, notif })
    }

    /// Notification a lier au thread de service (`notify_bind`) pour
    /// recevoir l'interruption comme un message.
    pub fn notification(&self) -> u32 {
        self.notif
    }

    /// Attend l'interruption (0 : sans limite) ; faux si le delai expire.
    pub fn wait(&self, timeout_ns: u64) -> bool {
        sys::notify_wait_timeout(self.notif, timeout_ns).is_ok()
    }

    /// Re-autorise la ligne (masquee par le noyau a chaque declenchement).
    pub fn ack(&self) {
        let _ = sys::irq_ack(self.cap);
    }
}

/// Cree l'endpoint d'un service et le publie sous `name`.
pub fn publish(name: &str) -> Result<u32, i64> {
    let ep = sys::endpoint_create()?;
    let client = sys::handle_dup(ep, SEND | GRANT | DUP, 0)?;
    let r = ns::register(name, client);
    let _ = sys::handle_close(client);
    r.map(|_| ep)
}

/// Boucle de service : `f` repond a chaque requete ; `None` = reponse
/// differee (le gestionnaire a garde `rx.token` pour `ipc::reply_to`).
pub fn serve(ep: u32, mut f: impl FnMut(&Received) -> Option<Msg>) -> ! {
    let mut rx = loop {
        if let Ok(r) = ipc::recv(ep) {
            break r;
        }
    };
    loop {
        rx = match f(&rx) {
            Some(reply) => match ipc::reply_recv(ep, &reply) {
                Ok(r) => r,
                Err(_) => loop {
                    if let Ok(r) = ipc::recv(ep) {
                        break r;
                    }
                },
            },
            None => loop {
                if let Ok(r) = ipc::recv(ep) {
                    break r;
                }
            },
        };
    }
}

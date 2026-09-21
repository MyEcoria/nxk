//! Futex : attente sur un mot de 32 bits de la memoire utilisateur.
//!
//! La base des verrous, variables de condition et `park` de la bibliotheque
//! standard : tant qu'il n'y a pas de contention, tout se passe en EL0 par
//! operations atomiques ; le noyau n'intervient que pour endormir et reveiller.
//!
//! La cle d'attente est l'adresse **physique** du mot : deux processus qui
//! partagent une page se synchronisent donc aussi. Les attentes sont rangees
//! dans une table de hachage de files FIFO ; le verrou global rend atomiques
//! la verification de la valeur et la mise en attente, si bien qu'aucun
//! reveil ne peut se perdre entre les deux.

use nxk_abi::err::{EAGAIN, EFAULT, EINVAL, ETIMEDOUT};

use crate::kern::bkl::BklCell;
use crate::kern::sched::{self, Timeout};
use crate::kern::thread::{self, Queue, State, NIL};
use crate::mm::uaccess;

const BUCKETS: usize = 256;

static TABLE: BklCell<[Queue; BUCKETS]> = BklCell::new([const { Queue::new() }; BUCKETS]);

#[inline]
fn bucket(pa: u64) -> usize {
    ((pa >> 2).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize
}

fn key(uaddr: u64) -> Result<u64, i64> {
    if uaddr % 4 != 0 {
        return Err(-EINVAL);
    }
    uaccess::translate_populate(uaddr, false).ok_or(-EFAULT)
}

/// Bloque `ti` si le mot vaut toujours `val`. `Ok(None)` : bloque.
pub fn wait(ti: u16, uaddr: u64, val: u32, tmo: Timeout) -> Result<Option<u64>, i64> {
    let pa = key(uaddr)?;
    // Lecture alignee de 32 bits : atomique vis-a-vis des ecritures d'EL0.
    let cur = unsafe { core::ptr::read_volatile(pa as *const u32) };
    if cur != val {
        return Err(-EAGAIN);
    }
    if tmo == Timeout::Now {
        return Err(-ETIMEDOUT);
    }
    let b = bucket(pa);
    let t = thread::get(ti);
    t.state = State::BlockedFutex;
    t.futex_key = pa;
    t.blocked_on = b as u16;
    TABLE.get()[b].push(ti);
    if let Timeout::At(d) = tmo {
        sched::arm(ti, d);
    }
    Ok(None)
}

/// Reveille jusqu'a `n` threads en attente sur le mot ; rend leur nombre.
pub fn wake(uaddr: u64, n: u64) -> Result<u64, i64> {
    let pa = key(uaddr)?;
    let q = &mut TABLE.get()[bucket(pa)];
    let mut woken = 0;
    while woken < n {
        match q.take(|x| thread::get(x).futex_key == pa) {
            Some(w) => {
                let t = thread::get(w);
                t.frame.x[0] = 0;
                t.blocked_on = NIL;
                sched::make_ready(w);
                woken += 1;
            }
            None => break,
        }
    }
    Ok(woken)
}

pub fn remove_waiter(b: u16, ti: u16) {
    if (b as usize) < BUCKETS {
        TABLE.get()[b as usize].remove(ti);
    }
}

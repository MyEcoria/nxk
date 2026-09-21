//! Projections anonymes, avec reemploi des adresses virtuelles.
//!
//! Le noyau place les projections `mem_map(0, ...)` par un simple pointeur
//! croissant (une page de garde apres chacune) et ne reprend jamais une
//! adresse. Un programme qui projette et libere sans cesse (grands blocs,
//! piles de threads) finirait par epuiser sa zone : les plages rendues sont
//! donc gardees ici et reprojetees a adresse fixe.
//!
//! Invariant : toute plage de la liste est libre (rien n'y est projete), et
//! chaque projection vivante est suivie d'une page libre qui n'est pas dans la
//! liste (sa page de garde) ; `unmap` la rend avec la projection.

use super::abi::layout::PAGE_SIZE;
use super::abi::prot;
use super::syscall;
use crate::cell::UnsafeCell;
use crate::sys::sync::Mutex;

pub const PAGE: usize = PAGE_SIZE as usize;
const SLOTS: usize = 256;
const RW: u64 = prot::READ | prot::WRITE;

struct Ranges {
    /// Plages libres triees par adresse, jamais adjacentes (fusionnees).
    start: [usize; SLOTS],
    len: [usize; SLOTS],
    n: usize,
}

struct State {
    lock: Mutex,
    ranges: UnsafeCell<Ranges>,
}

// SAFETY: `ranges` n'est touche que sous `lock`.
unsafe impl Sync for State {}

static STATE: State = State {
    lock: Mutex::new(),
    ranges: UnsafeCell::new(Ranges { start: [0; SLOTS], len: [0; SLOTS], n: 0 }),
};

fn with<R>(f: impl FnOnce(&mut Ranges) -> R) -> R {
    STATE.lock.lock();
    // SAFETY: acces exclusif sous le verrou.
    let r = f(unsafe { &mut *STATE.ranges.get() });
    unsafe { STATE.lock.unlock() };
    r
}

/// Retire `need` octets en tete d'une plage libre (premiere qui convient).
fn take(need: usize) -> Option<usize> {
    with(|r| {
        let i = (0..r.n).find(|&i| r.len[i] >= need)?;
        let a = r.start[i];
        if r.len[i] == need {
            r.start.copy_within(i + 1..r.n, i);
            r.len.copy_within(i + 1..r.n, i);
            r.n -= 1;
        } else {
            r.start[i] += need;
            r.len[i] -= need;
        }
        Some(a)
    })
}

/// Rend une plage libre (non projetee) a la liste.
pub fn record(start: usize, len: usize) {
    if len == 0 {
        return;
    }
    with(|r| {
        let i = r.start[..r.n].partition_point(|&s| s < start);
        let joins_prev = i > 0 && r.start[i - 1] + r.len[i - 1] == start;
        let joins_next = i < r.n && start + len == r.start[i];
        match (joins_prev, joins_next) {
            (true, true) => {
                r.len[i - 1] += len + r.len[i];
                r.start.copy_within(i + 1..r.n, i);
                r.len.copy_within(i + 1..r.n, i);
                r.n -= 1;
            }
            (true, false) => r.len[i - 1] += len,
            (false, true) => {
                r.start[i] = start;
                r.len[i] += len;
            }
            (false, false) => {
                // Liste pleine : la plage est abandonnee (seule l'adresse est
                // perdue, les pages sont deja rendues au noyau).
                if r.n < SLOTS {
                    r.start.copy_within(i..r.n, i + 1);
                    r.len.copy_within(i..r.n, i + 1);
                    r.start[i] = start;
                    r.len[i] = len;
                    r.n += 1;
                }
            }
        }
    })
}

/// Projette `len` octets en lecture/ecriture (pages a zero, allouees au
/// premier acces), suivis d'une page de garde.
pub fn map(len: usize) -> Option<*mut u8> {
    let len = len.checked_next_multiple_of(PAGE)?;
    if let Some(a) = take(len.checked_add(PAGE)?) {
        if syscall::mem_map_at(a as u64, len as u64, RW) >= 0 {
            return Some(a as *mut u8);
        }
        // Ne devrait pas arriver : la plage est simplement oubliee.
    }
    let r = syscall::mem_map(len as u64, RW);
    if r < 0 { None } else { Some(r as *mut u8) }
}

/// Libere une projection obtenue par `map` (meme longueur), page de garde
/// comprise.
///
/// # Safety
/// Plus aucun acces a la zone ne doit suivre.
pub unsafe fn unmap(p: *mut u8, len: usize) {
    let len = len.next_multiple_of(PAGE);
    syscall::mem_unmap(p as u64, len as u64);
    record(p as usize, len + PAGE);
}

/// Retire `[start, start + len)` d'une projection et rend la plage.
///
/// # Safety
/// Plus aucun acces a la zone ne doit suivre.
pub unsafe fn release(start: usize, len: usize) {
    if len > 0 {
        syscall::mem_unmap(start as u64, len as u64);
        record(start, len);
    }
}

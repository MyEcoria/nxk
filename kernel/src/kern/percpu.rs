//! Donnees propres a chaque coeur, atteintes via `TPIDR_EL1`.

use core::sync::atomic::AtomicBool;

use crate::arch;
use crate::kern::sched::RunQueue;
use crate::kern::thread::NIL;
use crate::{sysreg_read, sysreg_write};

pub const NR_CPUS: usize = 4;

/// Compteurs d'activite. Les durees sont en unites du compteur materiel,
/// converties en ns seulement quand on les lit.
#[derive(Default, Clone, Copy)]
pub struct CpuStats {
    pub switches: u64,
    pub syscalls: u64,
    pub irqs: u64,
    pub idle_time: u64,
    pub busy_time: u64,
}

#[repr(C, align(64))]
pub struct PerCpu {
    /// Sommet de la pile noyau du coeur. Offset 0 : lu par l'assembleur a
    /// chaque entree depuis EL0.
    pub kstack_top: u64,
    pub id: usize,
    /// Thread logiquement courant (`NIL` = coeur au repos).
    pub current: u16,
    /// Dernier thread rendu a EL0 : son UTCB et ses registres sont en place.
    pub last_run: u16,
    /// Proprietaire actuel des registres FP/SIMD du coeur.
    pub fp_owner: u16,
    /// Echeances (dormeurs, attentes bornees), triees par date croissante.
    pub tmo_head: u16,
    /// Threads a faire migrer par ce coeur (il detient peut-etre leur FP).
    pub mig_head: u16,
    /// Thread dont le temps d'execution est en cours de decompte.
    pub acct_thread: u16,
    pub fp_enabled: bool,
    pub need_resched: bool,
    /// Le quantum du thread courant est epuise.
    pub slice_expired: bool,
    /// Echeances modifiees : le timer doit etre reprogramme.
    pub timer_dirty: bool,
    /// Un coeur au repos attend qu'on lui cede un thread pret.
    pub push_pending: bool,
    /// MDSCR_EL1.SS pose (pas a pas du thread courant).
    pub ss_enabled: bool,
    pub online: bool,
    /// Valeur chargee dans TTBR0_EL1 (evite les ecritures inutiles).
    pub cur_ttbr: u64,
    /// Fin du quantum en cours (0 : pas de concurrent, pas de quantum).
    pub slice_end: u64,
    /// Echeance programmee dans le timer (`u64::MAX` : timer arrete).
    pub timer_cval: u64,
    /// Debut de l'intervalle en cours de decompte.
    pub acct_start: u64,
    pub rq: RunQueue,
    pub stats: CpuStats,
    /// Mode idle-poll : le coeur scrute ce drapeau au lieu de dormir.
    pub polling: AtomicBool,
    pub wake: AtomicBool,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            kstack_top: 0,
            id: 0,
            current: NIL,
            last_run: NIL,
            fp_owner: NIL,
            tmo_head: NIL,
            mig_head: NIL,
            acct_thread: NIL,
            fp_enabled: false,
            need_resched: false,
            slice_expired: false,
            timer_dirty: false,
            push_pending: false,
            ss_enabled: false,
            online: false,
            cur_ttbr: 0,
            slice_end: 0,
            timer_cval: u64::MAX,
            acct_start: 0,
            rq: RunQueue::new(),
            stats: CpuStats {
                switches: 0,
                syscalls: 0,
                irqs: 0,
                idle_time: 0,
                busy_time: 0,
            },
            polling: AtomicBool::new(false),
            wake: AtomicBool::new(false),
        }
    }
}

static mut PERCPU: [PerCpu; NR_CPUS] = [const { PerCpu::new() }; NR_CPUS];

/// Coeur courant, sans lire MPIDR ni indexer : un `mrs` suffit.
#[inline(always)]
pub fn this() -> &'static mut PerCpu {
    let p = sysreg_read!("tpidr_el1") as *mut PerCpu;
    unsafe { &mut *p }
}

#[inline(always)]
pub fn get(id: usize) -> &'static mut PerCpu {
    unsafe { &mut (*core::ptr::addr_of_mut!(PERCPU))[id] }
}

/// Initialise les donnees du coeur courant et publie leur adresse.
pub fn init(id: usize, kstack_top: u64, kernel_ttbr: u64) {
    let c = get(id);
    c.id = id;
    c.kstack_top = kstack_top;
    c.cur_ttbr = kernel_ttbr;
    c.acct_start = crate::arch::timer::counter();
    c.online = true;
    sysreg_write!("tpidr_el1", c as *mut PerCpu as u64);
    arch::isb();
}

pub fn online_count() -> usize {
    (0..NR_CPUS).filter(|&i| get(i).online).count()
}

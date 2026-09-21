//! Threads : blocs de controle (TCB).
//!
//! Le noyau n'a pas de pile par thread. A l'entree depuis EL0, l'assembleur
//! range les registres utilisateur directement dans le `Frame` du TCB courant ;
//! a la sortie, il recharge ceux du thread choisi. Changer de thread ne coute
//! donc qu'un changement d'indice, plus TTBR0 si le processus change.

use crate::kern::bkl::BklCell;
use crate::kern::objtab::Table;

pub const NIL: u16 = u16::MAX;
pub const NR_PRIO: usize = 32;
/// Affinite libre : le noyau choisit (et peut changer) le coeur.
pub const ANY_CPU: u8 = 0xFF;

/// Registres utilisateur. Disposition figee : l'assembleur ecrit ces offsets.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Frame {
    pub x: [u64; 31], // @0
    pub sp: u64,      // @248 SP_EL0
    pub elr: u64,     // @256
    pub spsr: u64,    // @264
    pub tpidr: u64,   // @272 TPIDR_EL0 (TLS utilisateur)
    pub _pad: u64,    // @280
}

pub const FRAME_SIZE: usize = 288;
const _: () = assert!(core::mem::size_of::<Frame>() == FRAME_SIZE);

/// Etat FP/SIMD, sauvegarde paresseusement (voir `arch::fpu`).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FpState {
    pub q: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum State {
    Free = 0,
    Ready,
    Running,
    /// Emetteur en file sur un point de rendez-vous.
    BlockedSend,
    /// Recepteur en attente sur un point de rendez-vous.
    BlockedRecv,
    /// Client ayant envoye un `call`, en attente de la reponse.
    BlockedReply,
    BlockedNotify,
    /// Attend la fin d'un processus.
    BlockedWait,
    /// Attend sur un futex (cle : adresse physique).
    BlockedFutex,
    Sleeping,
    /// Arrete (debogage) en attendant un debogueur.
    Stopped,
    /// Mort, mais en cours d'execution sur un autre coeur : ce coeur le liberera.
    Zombie,
}

#[repr(C, align(64))]
pub struct Tcb {
    pub frame: Frame, // @0 : impose par l'assembleur
    pub fp: FpState,
    pub state: State,
    pub prio: u8,
    pub home: u8,
    /// Present dans une file d'execution.
    pub queued: bool,
    /// Present dans la file d'echeances de son coeur.
    pub timed: bool,
    /// Coeur impose, ou `ANY_CPU`.
    pub affinity: u8,
    pub gen: u16,
    /// Chainage intrusif : un thread n'est jamais dans deux files a la fois.
    pub next: u16,
    pub proc_idx: u16,
    pub utcb_slot: u16,
    /// Client dont on doit porter la reponse (serveur).
    pub reply_to: u16,
    /// Processus serveur dont on attend la reponse (client).
    pub reply_from: u16,
    /// Objet sur lequel le thread est bloque (endpoint, notification, processus).
    pub blocked_on: u16,
    pub bound_notif: u16,
    /// Thread suivant du meme processus.
    pub proc_next: u16,
    /// Serveur qui detient notre reponse (appel en cours).
    pub reply_holder: u16,
    /// File d'echeances du coeur (doublement chainee, triee par `wake_at`).
    pub tq_next: u16,
    pub tq_prev: u16,
    /// File des migrations en attente sur l'ancien coeur.
    pub mig_next: u16,
    pub send_is_call: bool,
    /// Present dans la file de migrations d'un coeur.
    pub mig_queued: bool,
    /// Evenement de debogage en cours : le message au debogueur vient des
    /// champs `fault_*` et sa reponse ne touche pas aux registres.
    pub in_fault: bool,
    pub fault_kind: u8,
    /// Arret demande par le debogueur, pris a la prochaine sortie du noyau.
    pub stop_request: bool,
    /// Executer une seule instruction, puis evenement STEP.
    pub single_step: bool,
    pub tid: u32,
    /// Numero d'appel : un jeton de reponse ne vaut que pour l'appel en cours.
    pub call_seq: u32,
    pub send_badge: u64,
    pub utcb_va: u64,
    pub utcb_pa: u64,
    /// Echeance absolue (compteur) d'un sommeil ou d'une attente bornee.
    pub wake_at: u64,
    pub futex_key: u64,
    /// Temps processeur consomme (unites du compteur).
    pub cpu_time: u64,
    pub fault_esr: u64,
    pub fault_far: u64,
}

impl Tcb {
    fn reset(&mut self, gen: u16) {
        // Tout a zero (registres, etat FP), puis les champs non nuls.
        unsafe { core::ptr::write_bytes(self as *mut Tcb, 0, 1) };
        self.gen = gen;
        self.next = NIL;
        self.proc_idx = NIL;
        self.utcb_slot = NIL;
        self.reply_to = NIL;
        self.reply_from = NIL;
        self.blocked_on = NIL;
        self.bound_notif = NIL;
        self.proc_next = NIL;
        self.reply_holder = NIL;
        self.tq_next = NIL;
        self.tq_prev = NIL;
        self.mig_next = NIL;
        self.affinity = ANY_CPU;
        self.state = State::Free;
    }
}

fn tcb_reset(t: &mut Tcb) {
    t.reset(0);
}

fn tcb_link(t: &mut Tcb) -> &mut u16 {
    &mut t.next
}

pub static TCBS: BklCell<Table<Tcb>> = BklCell::new(Table::new(tcb_reset, tcb_link));
static mut NEXT_TID: u32 = 1;

#[inline(always)]
pub fn get(i: u16) -> &'static mut Tcb {
    TCBS.get().get(i)
}

/// Borne d'iteration sur tous les TCB crees.
#[inline]
pub fn capacity() -> usize {
    TCBS.get().capacity()
}

pub fn init() {}

/// Reserve un TCB libre, remis a zero.
pub fn alloc() -> Option<u16> {
    let i = TCBS.get().alloc()?;
    let t = get(i);
    let gen = t.gen.wrapping_add(1);
    t.reset(gen);
    unsafe {
        t.tid = NEXT_TID;
        NEXT_TID += 1;
    }
    Some(i)
}

pub fn free(i: u16) {
    let t = get(i);
    let gen = t.gen;
    t.reset(gen);
    TCBS.get().free(i);
}

/// Jeton de reponse : designe un appel precis d'un client precis. Il perd sa
/// valeur des que le client disparait ou que cet appel se termine (reponse,
/// delai echu) : une reponse tardive ne peut pas atteindre l'appel suivant.
#[inline]
pub fn token(i: u16) -> u64 {
    let t = get(i);
    ((t.call_seq as u64) << 32) | ((i as u64) << 16) | t.gen as u64
}

#[inline]
pub fn from_token(tok: u64) -> Option<u16> {
    let i = ((tok >> 16) & 0xFFFF) as usize;
    if i >= capacity() {
        return None;
    }
    let t = get(i as u16);
    (t.gen == tok as u16 && t.call_seq == (tok >> 32) as u32 && t.state != State::Free).then_some(i as u16)
}

/// File FIFO intrusive de threads.
#[derive(Clone, Copy)]
pub struct Queue {
    pub head: u16,
    pub tail: u16,
}

impl Queue {
    pub const fn new() -> Self {
        Self { head: NIL, tail: NIL }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head == NIL
    }

    #[inline]
    pub fn push(&mut self, i: u16) {
        get(i).next = NIL;
        if self.tail == NIL {
            self.head = i;
        } else {
            get(self.tail).next = i;
        }
        self.tail = i;
    }

    #[inline]
    pub fn pop(&mut self) -> Option<u16> {
        let i = self.head;
        if i == NIL {
            return None;
        }
        self.head = get(i).next;
        if self.head == NIL {
            self.tail = NIL;
        }
        get(i).next = NIL;
        Some(i)
    }

    /// Retire le premier element satisfaisant `pred`.
    pub fn take(&mut self, mut pred: impl FnMut(u16) -> bool) -> Option<u16> {
        let mut prev = NIL;
        let mut cur = self.head;
        while cur != NIL {
            let next = get(cur).next;
            if pred(cur) {
                if prev == NIL {
                    self.head = next;
                } else {
                    get(prev).next = next;
                }
                if self.tail == cur {
                    self.tail = prev;
                }
                get(cur).next = NIL;
                return Some(cur);
            }
            prev = cur;
            cur = next;
        }
        None
    }

    pub fn remove(&mut self, i: u16) -> bool {
        self.take(|x| x == i).is_some()
    }
}

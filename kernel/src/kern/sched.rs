//! Ordonnanceur evenementiel, sans tic periodique.
//!
//! Chaque entree dans le noyau se termine par `exit_to_user`, qui choisit le
//! thread a reprendre sur ce coeur et recharge ses registres depuis son TCB.
//! Bloquer un thread ne demande donc aucun changement de pile : il suffit de
//! changer son etat, le coeur repart simplement avec quelqu'un d'autre.
//!
//! Selection en O(1) : un bitmap de 32 priorites (0 = la plus haute), une file
//! FIFO par niveau. Un quantum n'est arme que si un thread de meme priorite
//! attend sur le coeur ; sinon le thread courant n'est jamais interrompu.
//!
//! Chaque thread a un coeur d'attache, seul a l'executer : la gestion
//! paresseuse du FP reste exacte avec un proprietaire par coeur. Un thread
//! change de coeur quand on lui impose une affinite, quand il se reveille
//! alors qu'un autre coeur est libre, ou quand un coeur surcharge en cede un a
//! un coeur au repos. Une migration est toujours faite par l'ancien coeur, le
//! seul a pouvoir sauver l'etat FP qu'il detient peut-etre encore.

use core::sync::atomic::Ordering;

use nxk_abi::err::ETIMEDOUT;

use crate::arch::{self, fpu, gic, timer};
use crate::kern::percpu::{self, PerCpu, NR_CPUS};
use crate::kern::thread::{self, State, ANY_CPU, NIL, NR_PRIO};
use crate::kern::{bkl, debug, futex, ipc, notify, process};
use crate::sysreg_write;

/// Quantum : temps d'execution continu accorde avant de ceder la place a un
/// thread pret de meme priorite.
pub const SLICE_NS: u64 = 10_000_000;

pub struct RunQueue {
    bitmap: u32,
    heads: [u16; NR_PRIO],
    tails: [u16; NR_PRIO],
    nr: u32,
}

impl RunQueue {
    pub const fn new() -> Self {
        Self {
            bitmap: 0,
            heads: [NIL; NR_PRIO],
            tails: [NIL; NR_PRIO],
            nr: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, i: u16) {
        let t = thread::get(i);
        if t.queued {
            return;
        }
        t.queued = true;
        t.next = NIL;
        let p = t.prio as usize;
        if self.heads[p] == NIL {
            self.heads[p] = i;
        } else {
            thread::get(self.tails[p]).next = i;
        }
        self.tails[p] = i;
        self.bitmap |= 1 << p;
        self.nr += 1;
    }

    #[inline]
    pub fn pop(&mut self) -> Option<u16> {
        if self.bitmap == 0 {
            return None;
        }
        let p = self.bitmap.trailing_zeros() as usize;
        let i = self.heads[p];
        let t = thread::get(i);
        self.heads[p] = t.next;
        if self.heads[p] == NIL {
            self.tails[p] = NIL;
            self.bitmap &= !(1 << p);
        }
        t.next = NIL;
        t.queued = false;
        self.nr -= 1;
        Some(i)
    }

    #[inline]
    pub fn best_prio(&self) -> Option<u8> {
        (self.bitmap != 0).then(|| self.bitmap.trailing_zeros() as u8)
    }

    pub fn remove(&mut self, i: u16) -> bool {
        let t = thread::get(i);
        if !t.queued {
            return false;
        }
        let p = t.prio as usize;
        let mut prev = NIL;
        let mut cur = self.heads[p];
        while cur != NIL {
            let next = thread::get(cur).next;
            if cur == i {
                if prev == NIL {
                    self.heads[p] = next;
                } else {
                    thread::get(prev).next = next;
                }
                if self.tails[p] == i {
                    self.tails[p] = prev;
                }
                if self.heads[p] == NIL {
                    self.bitmap &= !(1 << p);
                }
                t.next = NIL;
                t.queued = false;
                self.nr -= 1;
                return true;
            }
            prev = cur;
            cur = next;
        }
        false
    }

    /// Premier thread pret sans affinite, de la priorite la plus haute.
    fn movable(&self) -> Option<u16> {
        let mut bits = self.bitmap;
        while bits != 0 {
            let p = bits.trailing_zeros() as usize;
            let mut cur = self.heads[p];
            while cur != NIL {
                if thread::get(cur).affinity == ANY_CPU {
                    return Some(cur);
                }
                cur = thread::get(cur).next;
            }
            bits &= !(1 << p);
        }
        None
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.nr
    }
}

/// Envoie une IPI : le coeur cible repasse par le noyau et reevalue son choix.
#[inline]
pub fn kick(cpu: usize) {
    gic::send_sgi(1 << cpu, gic::SGI_RESCHEDULE);
}

/// Echeance d'une attente bornee.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Timeout {
    Never,
    /// Deja echue : l'appel echoue au lieu de bloquer.
    Now,
    /// Valeur absolue du compteur.
    At(u64),
}

impl Timeout {
    /// Delai relatif en ns ; 0 = sans limite.
    pub fn from_ns(ns: u64) -> Self {
        if ns == 0 {
            return Timeout::Never;
        }
        match timer::ns_to_ticks(ns) {
            0 => Timeout::Now,
            t => Timeout::At(timer::counter().saturating_add(t)),
        }
    }

    /// Arme l'echeance du thread qui vient de se bloquer.
    #[inline]
    pub fn arm(self, ti: u16) {
        if let Timeout::At(d) = self {
            arm(ti, d);
        }
    }
}

/// Un coeur libre : en ligne, sans thread courant ni thread pret.
fn idle_core(except: usize) -> Option<usize> {
    (0..NR_CPUS).find(|&k| {
        let c = percpu::get(k);
        k != except && c.online && c.current == NIL && c.rq.len() == 0
    })
}

#[macro_export]
macro_rules! trace {
    ($($a:tt)*) => {
        if cfg!(feature = "trace-sched") {
            crate::kprintln!($($a)*);
        }
    };
}

/// Rend un thread executable sur son coeur d'attache.
pub fn make_ready(i: u16) {
    let t = thread::get(i);
    trace!("[s{}] ready tid {} home {} state {:?}", percpu::this().id, t.tid, t.home, t.state);
    if t.timed {
        disarm(i);
    }
    t.state = State::Ready;

    // Reveil sur un coeur occupe alors qu'un autre est libre : le thread
    // change de coeur, sauf si son etat FP vit encore dans l'ancien.
    let home = t.home as usize;
    let hc = percpu::get(home);
    if t.affinity == ANY_CPU
        && hc.fp_owner != i
        && hc.current != NIL
        && thread::get(hc.current).prio <= t.prio
    {
        if let Some(k) = idle_core(home) {
            t.home = k as u8;
        }
    }

    let home = t.home as usize;
    let prio = t.prio;
    let c = percpu::get(home);
    c.rq.push(i);

    let (preempt, slice) = if c.current == NIL {
        (true, false)
    } else {
        let cp = thread::get(c.current).prio;
        (prio < cp, prio == cp && c.slice_end == 0)
    };
    if home == percpu::this().id {
        // Le quantum eventuel sera arme a la sortie du noyau.
        if preempt {
            c.need_resched = true;
        }
    } else if preempt || slice {
        // Un concurrent de meme priorite arrive : le coeur doit armer son
        // quantum, il faut donc qu'il repasse par le noyau.
        c.need_resched |= preempt;
        if cfg!(feature = "idle-poll") && c.polling.load(Ordering::Acquire) {
            c.wake.store(true, Ordering::Release); // il scrute : pas d'IPI
        } else {
            kick(home);
        }
    }
}

/// Commutation directe (IPC) : `i` devient le thread courant de ce coeur sans
/// passer par la file d'execution. L'ancien courant est deja bloque.
#[inline]
pub fn switch_to(i: u16) {
    let t = thread::get(i);
    if t.timed {
        disarm(i);
    }
    t.state = State::Running;
    percpu::this().current = i;
}

/// Abandon volontaire : cede la place aux threads de meme priorite.
pub fn yield_current(_i: u16) {
    let c = percpu::this();
    c.slice_expired = true;
    c.need_resched = true;
}

/// Retire un thread de toute file ou il attend (fin de processus, delai).
pub fn detach(i: u16) {
    let t = thread::get(i);
    if t.timed {
        disarm(i);
    }
    match t.state {
        State::Ready => {
            percpu::get(t.home as usize).rq.remove(i);
        }
        State::BlockedSend | State::BlockedRecv => ipc::remove_waiter(t.blocked_on, i),
        State::BlockedReply => ipc::abandon_reply(i),
        State::BlockedNotify => notify::remove_waiter(t.blocked_on, i),
        State::BlockedWait => {
            process::get(t.blocked_on).waiters.remove(i);
        }
        State::BlockedFutex => futex::remove_waiter(t.blocked_on, i),
        _ => {}
    }
    t.blocked_on = NIL;
}

/// Coeur le moins charge, avec rotation pour departager les ex aequo.
pub fn least_loaded() -> usize {
    static mut ROT: usize = 0;
    let start = unsafe {
        ROT = (ROT + 1) % NR_CPUS;
        ROT
    };
    let mut best = start;
    let mut best_load = u32::MAX;
    for k in 0..NR_CPUS {
        let id = (start + k) % NR_CPUS;
        let c = percpu::get(id);
        if !c.online {
            continue;
        }
        let load = c.rq.len() + u32::from(c.current != NIL);
        if load < best_load {
            best_load = load;
            best = id;
        }
    }
    best
}

// ------------------------------------------------------------- echeances ---

/// Inscrit `i` dans la file d'echeances (triee) de son coeur d'attache.
pub fn arm(i: u16, deadline: u64) {
    let t = thread::get(i);
    if t.timed {
        disarm(i);
    }
    t.wake_at = deadline;
    t.timed = true;
    let home = t.home as usize;
    let c = percpu::get(home);
    trace!("[s{}] arm tid {} home {} d {} tete {}", percpu::this().id, t.tid, home, deadline, c.tmo_head);
    let mut prev = NIL;
    let mut cur = c.tmo_head;
    while cur != NIL && thread::get(cur).wake_at <= deadline {
        prev = cur;
        cur = thread::get(cur).tq_next;
    }
    t.tq_prev = prev;
    t.tq_next = cur;
    if cur != NIL {
        thread::get(cur).tq_prev = i;
    }
    if prev == NIL {
        // Nouvelle echeance la plus proche : le timer du coeur doit suivre.
        c.tmo_head = i;
        c.timer_dirty = true;
        if home != percpu::this().id {
            kick(home);
        }
    } else {
        thread::get(prev).tq_next = i;
    }
}

/// Retire l'echeance de `i`. Le timer peut garder l'ancienne date : il se
/// declenchera pour rien, une fois, et sera reprogramme.
pub fn disarm(i: u16) {
    let t = thread::get(i);
    if !t.timed {
        return;
    }
    t.timed = false;
    let c = percpu::get(t.home as usize);
    trace!("[s{}] disarm tid {} home {} prev {} next {}", percpu::this().id, t.tid, t.home, t.tq_prev, t.tq_next);
    if t.tq_prev == NIL {
        c.tmo_head = t.tq_next;
    } else {
        thread::get(t.tq_prev).tq_next = t.tq_next;
    }
    if t.tq_next != NIL {
        thread::get(t.tq_next).tq_prev = t.tq_prev;
    }
    t.tq_next = NIL;
    t.tq_prev = NIL;
}

/// Endort le thread courant jusqu'a l'echeance (valeur absolue du compteur).
pub fn sleep_until(i: u16, deadline: u64) {
    thread::get(i).state = State::Sleeping;
    arm(i, deadline);
}

/// Traite les echeances atteintes du coeur courant : les dormeurs repartent
/// avec 0, les attentes bornees avec -ETIMEDOUT.
pub fn expire(now: u64) {
    let c = percpu::this();
    while c.tmo_head != NIL {
        let i = c.tmo_head;
        let t = thread::get(i);
        if t.wake_at > now {
            break;
        }
        let slept = t.state == State::Sleeping;
        detach(i);
        let t = thread::get(i);
        t.frame.x[0] = if slept { 0 } else { (-ETIMEDOUT) as u64 };
        make_ready(i);
    }
}

// ------------------------------------------------ priorites et migrations ---

/// Change la priorite d'un thread, ou qu'il soit.
pub fn set_prio(i: u16, prio: u8) {
    let t = thread::get(i);
    if t.prio == prio {
        return;
    }
    let home = t.home as usize;
    let c = percpu::get(home);
    let requeue = t.state == State::Ready && c.rq.remove(i);
    t.prio = prio;
    if requeue {
        c.rq.push(i);
    }
    if matches!(t.state, State::Ready | State::Running) {
        // Le coeur reevalue son choix (preemption dans un sens ou l'autre).
        c.need_resched = true;
        if home != percpu::this().id {
            kick(home);
        }
    }
}

/// Impose (ou libere) le coeur d'un thread. La migration est confiee a son
/// coeur actuel.
pub fn set_affinity(i: u16, cpu: u8) {
    let t = thread::get(i);
    t.affinity = cpu;
    if cpu == ANY_CPU || t.home == cpu {
        return;
    }
    let src = t.home as usize;
    if src == percpu::this().id {
        migrate_now(i, cpu as usize);
    } else if !t.mig_queued {
        let c = percpu::get(src);
        t.mig_queued = true;
        t.mig_next = c.mig_head;
        c.mig_head = i;
        kick(src);
    }
}

/// Deplace `i`, dont le coeur courant est le coeur d'attache, vers `dst`.
fn migrate_now(i: u16, dst: usize) {
    let c = percpu::this();
    let t = thread::get(i);
    if t.home as usize != c.id || dst == c.id || !percpu::get(dst).online {
        return;
    }
    if c.fp_owner == i {
        fpu::save_owner(c);
    }
    match t.state {
        State::Ready => {
            c.rq.remove(i);
            t.home = dst as u8;
            push_to(i, dst);
        }
        State::Running => {
            // Thread courant de ce coeur : il repartira depuis `dst`.
            if c.current == i {
                c.current = NIL;
            }
            t.home = dst as u8;
            push_to(i, dst);
        }
        State::Free | State::Zombie => {}
        _ => {
            // Bloque : seule son echeance est rattachee a un coeur.
            if t.timed {
                let d = t.wake_at;
                disarm(i);
                t.home = dst as u8;
                arm(i, d);
            } else {
                t.home = dst as u8;
            }
        }
    }
}

/// Place un thread pret dans la file de `dst` et previent ce coeur.
fn push_to(i: u16, dst: usize) {
    let t = thread::get(i);
    t.state = State::Ready;
    let c = percpu::get(dst);
    c.rq.push(i);
    c.need_resched = true;
    if dst != percpu::this().id {
        kick(dst);
    }
}

/// Migrations demandees par d'autres coeurs, et demande de travail d'un coeur
/// au repos.
fn run_migrations(c: &mut PerCpu) {
    while c.mig_head != NIL {
        let i = c.mig_head;
        let t = thread::get(i);
        c.mig_head = t.mig_next;
        t.mig_next = NIL;
        t.mig_queued = false;
        if t.affinity != ANY_CPU {
            migrate_now(i, t.affinity as usize);
        }
    }
}

/// Cede un thread pret sans affinite a un coeur au repos. Vrai si fait.
/// Seulement en surplus : ce coeur garde toujours de quoi s'occuper.
fn push_to_idle(c: &mut PerCpu) -> bool {
    let running = c.current != NIL && thread::get(c.current).state == State::Running;
    if c.rq.len() + u32::from(running) < 2 {
        return false;
    }
    let Some(dst) = idle_core(c.id) else { return false };
    let Some(i) = c.rq.movable() else { return false };
    migrate_now(i, dst);
    true
}

/// Un coeur passe au repos : le plus charge des autres, s'il a un thread
/// pret deplacable, le lui cedera a sa prochaine sortie du noyau.
fn request_work(me: usize) {
    let mut best = None;
    let mut best_len = 0;
    for k in 0..NR_CPUS {
        let c = percpu::get(k);
        if k != me
            && c.online
            && !c.push_pending
            && c.current != NIL
            && c.rq.len() > best_len
            && c.rq.movable().is_some()
        {
            best_len = c.rq.len();
            best = Some(k);
        }
    }
    if let Some(k) = best {
        percpu::get(k).push_pending = true;
        kick(k);
    }
}

/// Un thread disparait : plus aucune trace dans les structures par coeur.
pub fn forget_thread(i: u16) {
    for k in 0..NR_CPUS {
        let c = percpu::get(k);
        if c.acct_thread == i {
            let now = timer::counter();
            let dt = now - c.acct_start;
            c.stats.busy_time += dt;
            process::get(thread::get(i).proc_idx).cpu_time += dt;
            c.acct_start = now;
            c.acct_thread = NIL;
        }
        if c.last_run == i {
            c.last_run = NIL;
        }
        if c.fp_owner == i {
            c.fp_owner = NIL;
        }
        if thread::get(i).mig_queued {
            let mut prev = NIL;
            let mut cur = c.mig_head;
            while cur != NIL {
                let next = thread::get(cur).mig_next;
                if cur == i {
                    if prev == NIL {
                        c.mig_head = next;
                    } else {
                        thread::get(prev).mig_next = next;
                    }
                    thread::get(i).mig_queued = false;
                    break;
                }
                prev = cur;
                cur = next;
            }
        }
    }
}

// ------------------------------------------------------- sortie du noyau ---

fn choose(c: &mut PerCpu) -> Option<u16> {
    let cur = c.current;
    let resched = core::mem::take(&mut c.need_resched);
    let expired = core::mem::take(&mut c.slice_expired);
    if cur != NIL {
        let t = thread::get(cur);
        if t.state == State::Running {
            if !resched {
                return Some(cur);
            }
            match c.rq.best_prio() {
                Some(p) if p < t.prio => {
                    t.state = State::Ready;
                    c.rq.push(cur);
                }
                Some(p) if p == t.prio && expired => {
                    // Concurrence : un coeur libre prend-il la releve ?
                    if push_to_idle(c) && c.rq.best_prio() != Some(t.prio) {
                        return Some(cur);
                    }
                    t.state = State::Ready;
                    c.rq.push(cur);
                }
                _ => return Some(cur),
            }
        } else if c.rq.len() > 1 {
            push_to_idle(c);
        }
    } else if c.rq.len() > 1 {
        push_to_idle(c);
    }
    c.rq.pop()
}

/// Decompte du temps : l'intervalle ecoule est impute a qui occupait le coeur.
#[inline]
fn account(c: &mut PerCpu, next: u16) -> u64 {
    let now = timer::counter();
    let dt = now - c.acct_start;
    c.acct_start = now;
    let prev = core::mem::replace(&mut c.acct_thread, next);
    if prev == NIL {
        c.stats.idle_time += dt;
    } else {
        c.stats.busy_time += dt;
        let t = thread::get(prev);
        t.cpu_time += dt;
        process::get(t.proc_idx).cpu_time += dt;
    }
    now
}

extern "C" {
    fn nxk_return_to_user(frame: *const thread::Frame) -> !;
    fn nxk_enter_idle(kstack_top: u64) -> !;
}

/// Fin de toute entree dans le noyau : reprend le thread choisi, ou endort le
/// coeur. Rend le verrou global juste avant de quitter EL1.
pub fn exit_to_user() -> ! {
    let c = percpu::this();

    if c.mig_head != NIL {
        run_migrations(c);
    }
    if c.push_pending {
        c.push_pending = false;
        push_to_idle(c);
    }
    // Le thread courant a pu mourir pendant ce passage (ou etre tue par un
    // autre coeur pendant qu'il tournait ici) : c'est a ce coeur de le liberer.
    if c.current != NIL && thread::get(c.current).state == State::Zombie {
        let z = c.current;
        c.current = NIL;
        process::reap(z);
    }

    let chosen = loop {
        let chosen = choose(c);
        // Arret demande par un debogueur : le thread s'immobilise au lieu de
        // repartir en EL0, et le coeur choisit a nouveau.
        match chosen {
            Some(i) if thread::get(i).stop_request => {
                let t = thread::get(i);
                t.stop_request = false;
                t.state = State::Running;
                c.current = i;
                if !debug::event(i, nxk_abi::debug::KIND_STOP, 0, 0) {
                    debug::park(i);
                }
            }
            _ => break chosen,
        }
    };
    if chosen.map(|i| i != c.acct_thread).unwrap_or(c.acct_thread != NIL) {
        trace!("[s{}] run {:?} (rq {})", c.id, chosen.map(|i| thread::get(i).tid), c.rq.len());
    }
    match chosen {
        Some(i) => {
            c.polling.store(false, Ordering::Relaxed);
            let t = thread::get(i);
            t.state = State::Running;
            c.current = i;

            // Nouveau thread : decompte du temps et quantum neuf.
            let mut now = 0;
            if c.acct_thread != i {
                now = account(c, i);
                if c.slice_end != 0 {
                    c.slice_end = 0;
                    c.timer_dirty = true;
                }
            }
            // Quantum seulement face a un concurrent de meme priorite.
            if c.rq.best_prio() == Some(t.prio) {
                if c.slice_end == 0 {
                    if now == 0 {
                        now = timer::counter();
                    }
                    c.slice_end = now + timer::ns_to_ticks(SLICE_NS);
                    c.timer_dirty = true;
                }
            } else if c.slice_end != 0 {
                c.slice_end = 0;
                c.timer_dirty = true;
            }
            if c.timer_dirty {
                timer::reprogram(c);
            }

            let ttbr = process::get(t.proc_idx).ttbr;
            if c.cur_ttbr != ttbr {
                sysreg_write!("ttbr0_el1", ttbr);
                arch::isb();
                c.cur_ttbr = ttbr;
                arch::spectre::on_process_switch();
            }
            if c.last_run != i {
                c.stats.switches += 1;
                sysreg_write!("tpidrro_el0", t.utcb_va);
                c.last_run = i;
            }
            fpu::prepare(c, i);
            // Pas a pas : MDSCR_EL1.SS pour ce thread seulement (pose pour
            // un autre, il provoquerait une exception immediate).
            if t.single_step {
                t.frame.spsr |= debug::SPSR_SS;
            }
            if t.single_step != c.ss_enabled {
                let m = crate::sysreg_read!("mdscr_el1");
                sysreg_write!("mdscr_el1", if t.single_step { m | 1 } else { m & !1 });
                arch::isb();
                c.ss_enabled = t.single_step;
            }

            let frame = &t.frame as *const thread::Frame;
            bkl::release();
            unsafe { nxk_return_to_user(frame) }
        }
        None => {
            c.current = NIL;
            if c.acct_thread != NIL {
                account(c, NIL);
            }
            if c.slice_end != 0 {
                c.slice_end = 0;
                c.timer_dirty = true;
            }
            if c.timer_dirty {
                timer::reprogram(c);
            }
            // Un coeur au repos ne garde aucune table de processus chargee :
            // un processus peut mourir et ses tables etre reutilisees.
            let k = crate::arch::mmu::kernel_ttbr();
            if c.cur_ttbr != k {
                sysreg_write!("ttbr0_el1", k);
                arch::isb();
                c.cur_ttbr = k;
            }
            request_work(c.id);
            let top = c.kstack_top;
            bkl::release();
            unsafe { nxk_enter_idle(top) }
        }
    }
}

/// Boucle de repos : le coeur dort jusqu'a une interruption. S'il y a du
/// travail, le gestionnaire repart directement en EL0 sans revenir ici.
#[no_mangle]
pub extern "C" fn nxk_idle_loop() -> ! {
    if cfg!(feature = "idle-poll") {
        let c = percpu::this();
        c.polling.store(true, Ordering::Release);
        arch::irq_enable();
        loop {
            if c.wake.swap(false, Ordering::AcqRel) {
                arch::irq_disable();
                c.polling.store(false, Ordering::Release);
                bkl::acquire();
                if has_work() {
                    exit_to_user();
                }
                bkl::release();
                c.polling.store(true, Ordering::Release);
                arch::irq_enable();
            }
            core::hint::spin_loop();
        }
    }
    loop {
        arch::irq_enable();
        arch::wfi();
    }
}

/// Ce coeur a-t-il un thread a reprendre ou des migrations a faire ?
pub fn has_work() -> bool {
    let c = percpu::this();
    c.rq.len() > 0 || c.mig_head != NIL
}

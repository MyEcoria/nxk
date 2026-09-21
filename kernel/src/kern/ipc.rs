//! IPC synchrone par points de rendez-vous (endpoints), dans l'esprit de seL4.
//!
//! Un message = une etiquette + jusqu'a 504 mots. Les 6 premiers voyagent dans
//! les registres x2-x7 : ils sont recopies de TCB a TCB sans toucher la
//! memoire utilisateur. Les suivants sont copies d'une UTCB a l'autre par leurs
//! adresses physiques, en une seule copie. Jusqu'a 4 handles peuvent etre
//! transmis, sous reserve du droit GRANT.
//!
//! Chemin rapide : quand le serveur attend deja et vit sur le meme coeur que le
//! client, `call` bascule directement du client au serveur, et `reply_recv`
//! directement du serveur au client, sans passer par la file d'execution.

use nxk_abi::err::{EPIPE, ESRCH, ETIMEDOUT};
use nxk_abi::rights::{RECV, SEND};
use nxk_abi::tag;
use nxk_abi::utcb::MR_WORDS;

use crate::kern::bkl::BklCell;
use crate::kern::objtab::Table;
use crate::kern::cap::{self, Kind};
use crate::kern::thread::{self, Queue, State, NIL};
use crate::kern::sched::Timeout;
use crate::kern::{notify, percpu, sched};

pub struct Endpoint {
    pub used: bool,
    pub free_link: u16,
    /// Plus aucun handle ne permet de recevoir : les envois echouent.
    pub dead: bool,
    /// Tube (`endpoint::PIPE`) : quand plus aucun handle ne permet d'envoyer
    /// (hors ceux qui peuvent aussi recevoir), la reception rend -EPIPE,
    /// comme la lecture d'un tube dont tous les ecrivains sont fermes.
    pub pipe: bool,
    pub gen: u16,
    pub refs: u32,
    pub recv_refs: u32,
    /// Handles d'envoi seul (SEND sans RECV).
    pub send_refs: u32,
    pub send_q: Queue,
    pub recv_q: Queue,
}

fn ep_reset(e: &mut Endpoint) {
    e.used = false;
    e.dead = false;
    e.pipe = false;
    e.refs = 0;
    e.recv_refs = 0;
    e.send_refs = 0;
    e.send_q = Queue::new();
    e.recv_q = Queue::new();
}

fn ep_link(e: &mut Endpoint) -> &mut u16 {
    &mut e.free_link
}

pub static EPS: BklCell<Table<Endpoint>> = BklCell::new(Table::new(ep_reset, ep_link));

#[inline(always)]
fn ep(i: u16) -> &'static mut Endpoint {
    EPS.get().get(i)
}

pub fn init() {}

pub fn alive(i: u16, gen: u16) -> bool {
    (i as usize) < EPS.get().capacity() && ep(i).used && ep(i).gen == gen
}

pub fn create(pipe: bool) -> Option<(u16, u16)> {
    let i = EPS.get().alloc()?;
    let e = ep(i);
    let gen = e.gen;
    ep_reset(e);
    e.gen = gen;
    e.used = true;
    e.pipe = pipe;
    Some((i, e.gen))
}

#[inline]
fn send_only(rights: u16) -> bool {
    rights & SEND != 0 && rights & RECV == 0
}

pub fn incref(i: u16, rights: u16) {
    let e = ep(i);
    e.refs += 1;
    if rights & RECV != 0 {
        e.recv_refs += 1;
    }
    if send_only(rights) {
        e.send_refs += 1;
    }
}

pub fn decref(i: u16, rights: u16) {
    let e = ep(i);
    e.refs -= 1;
    if rights & RECV != 0 {
        e.recv_refs -= 1;
        if e.recv_refs == 0 {
            // Plus personne ne pourra jamais recevoir : on libere les attentes.
            e.dead = true;
            fail_all(i);
        }
    }
    if send_only(rights) {
        e.send_refs -= 1;
        if e.pipe && e.send_refs == 0 {
            // Tube sans ecrivain : les lecteurs en attente voient la fin.
            let e = ep(i);
            while let Some(t) = e.recv_q.pop() {
                fail(t, -EPIPE);
            }
        }
    }
    if e.refs == 0 {
        fail_all(i);
        e.used = false;
        e.gen = e.gen.wrapping_add(1);
        EPS.get().free(i);
    }
}

/// Revocation : nouvelle generation, seule la capacite de l'appelant compte
/// encore. Les attentes en cours (venues par d'anciennes capacites) echouent.
pub fn revoke(i: u16, rights: u16) -> u16 {
    let e = ep(i);
    e.gen = e.gen.wrapping_add(1);
    e.refs = 1;
    e.recv_refs = u32::from(rights & RECV != 0);
    e.send_refs = u32::from(send_only(rights));
    e.dead = e.recv_refs == 0;
    fail_all(i);
    ep(i).gen
}

fn fail_all(i: u16) {
    let e = ep(i);
    while let Some(t) = e.send_q.pop() {
        fail(t, -EPIPE);
    }
    while let Some(t) = e.recv_q.pop() {
        fail(t, -EPIPE);
    }
}

fn fail(t: u16, err: i64) {
    let th = thread::get(t);
    if th.in_fault {
        // Evenement de debogage sans destinataire : le thread reprend tel
        // quel (une faute se reproduira, et tuera le processus).
        th.in_fault = false;
        th.reply_from = NIL;
        th.reply_holder = NIL;
    } else {
        th.frame.x[0] = err as u64;
    }
    th.blocked_on = NIL;
    sched::make_ready(t);
}

pub fn remove_waiter(i: u16, t: u16) {
    if (i as usize) < EPS.get().capacity() {
        let e = ep(i);
        if !e.send_q.remove(t) {
            e.recv_q.remove(t);
        }
    }
}

/// Un client cesse d'attendre sa reponse (delai echu, fin) : le serveur qui
/// la detenait ne doit plus pouvoir la porter.
pub fn abandon_reply(ti: u16) {
    let t = thread::get(ti);
    let h = t.reply_holder;
    if h != NIL && thread::get(h).reply_to == ti {
        thread::get(h).reply_to = NIL;
    }
    t.reply_holder = NIL;
    t.reply_from = NIL;
    // Invalide un eventuel jeton de reponse differee.
    t.call_seq = t.call_seq.wrapping_add(1);
}

/// Fin d'un processus : ses clients en attente de reponse sont liberes.
pub fn abort_process(p: u16) {
    for t in 0..thread::capacity() as u16 {
        let th = thread::get(t);
        if th.state == State::BlockedReply && th.reply_from == p {
            th.reply_from = NIL;
            fail(t, -EPIPE);
        }
    }
}

/// Recopie le message de `src` vers `dst` et y pose le statut de succes.
#[inline]
fn transfer(src: u16, dst: u16, badge: u64) {
    let s = thread::get(src);
    let d = thread::get(dst);
    if s.in_fault {
        // Evenement de debogage : message fabrique par le noyau, registres du
        // thread immobilise laisses intacts.
        d.frame.x[0] = 0;
        d.frame.x[1] = tag::make(nxk_abi::debug::EVENT, 6, 0);
        d.frame.x[2] = s.fault_kind as u64;
        d.frame.x[3] = s.tid as u64;
        d.frame.x[4] = s.fault_esr;
        d.frame.x[5] = s.fault_far;
        d.frame.x[6] = s.frame.elr;
        d.frame.x[7] = crate::kern::process::get(s.proc_idx).pid as u64;
        d.frame.x[8] = badge;
        d.frame.x[9] = 0;
        d.frame.x[10] = crate::kern::process::get(s.proc_idx).pid as u64;
        return;
    }
    let t = s.frame.x[1];
    let words = tag::words(t).min(tag::MAX_WORDS);
    let nh = tag::handles(t).min(tag::MAX_HANDLES);

    d.frame.x[0] = 0;
    // Les drapeaux sont reserves au noyau : un emetteur ne peut pas les forger.
    d.frame.x[1] = tag::make(tag::label(t), words, nh);
    d.frame.x[2..8].copy_from_slice(&s.frame.x[2..8]);
    if words > tag::REG_WORDS {
        unsafe {
            core::ptr::copy_nonoverlapping(
                (s.utcb_pa as *const u64).add(tag::REG_WORDS),
                (d.utcb_pa as *mut u64).add(tag::REG_WORDS),
                words - tag::REG_WORDS,
            );
        }
    }
    if nh > 0 {
        let sh = (s.utcb_pa + MR_WORDS as u64 * 8) as *const u32;
        let dh = (d.utcb_pa + MR_WORDS as u64 * 8) as *mut u32;
        for k in 0..nh {
            let h = unsafe { *sh.add(k) };
            let new = cap::grant(s.proc_idx, h, d.proc_idx, None).unwrap_or(0);
            unsafe { *dh.add(k) = new };
        }
    }
    d.frame.x[8] = badge;
    d.frame.x[9] = 0;
    // Identite de l'emetteur, posee par le noyau : un service sait a quel
    // processus appartient une requete (et peut verifier qu'il vit encore).
    d.frame.x[10] = crate::kern::process::get(s.proc_idx).pid as u64;
}

/// Remet le message synthetique d'une notification liee.
fn deliver_notify(dst: u16, bits: u64) {
    let d = thread::get(dst);
    d.frame.x[0] = 0;
    d.frame.x[1] = tag::FLAG_NOTIFY;
    d.frame.x[2] = bits;
    d.frame.x[8] = 0;
    d.frame.x[9] = 0;
    d.frame.x[10] = 0;
}

/// Resultat d'un appel IPC : `Err` = code d'erreur a rendre immediatement.
pub type IpcResult = Result<(), i64>;

/// Delai d'un appel IPC bloquant : x9 a l'entree.
#[inline]
fn timeout_of(ti: u16) -> Timeout {
    Timeout::from_ns(thread::get(ti).frame.x[9])
}

pub fn call(ti: u16, h: u32) -> IpcResult {
    let pi = thread::get(ti).proc_idx;
    let tmo = timeout_of(ti);
    let c = cap::lookup(pi, h, Kind::Endpoint, SEND)?;
    let e = ep(c.obj as u16);
    if e.dead {
        return Err(-EPIPE);
    }
    let here = percpu::this().id;
    // Un serveur attend deja : de preference un thread de ce coeur.
    let r = e
        .recv_q
        .take(|x| thread::get(x).home as usize == here)
        .or_else(|| e.recv_q.pop());

    match r {
        Some(r) => {
            transfer(ti, r, c.badge);
            let server_proc = thread::get(r).proc_idx;
            let t = thread::get(ti);
            t.state = State::BlockedReply;
            t.reply_from = server_proc;
            t.reply_holder = r;
            t.call_seq = t.call_seq.wrapping_add(1);
            let rt = thread::get(r);
            rt.frame.x[9] = thread::token(ti);
            rt.reply_to = ti;
            rt.blocked_on = NIL;
            // Un appel sans attente porte quand meme sa requete au serveur
            // pret : seule l'attente d'un serveur est evitee, pas la reponse.
            if tmo != Timeout::Now {
                tmo.arm(ti);
            }
            if rt.home as usize == here {
                sched::switch_to(r); // chemin rapide
            } else {
                sched::make_ready(r);
            }
        }
        None => {
            if tmo == Timeout::Now {
                return Err(-ETIMEDOUT);
            }
            let t = thread::get(ti);
            t.state = State::BlockedSend;
            t.blocked_on = c.obj as u16;
            t.send_badge = c.badge;
            t.send_is_call = true;
            e.send_q.push(ti);
            tmo.arm(ti);
        }
    }
    Ok(())
}

/// Appel emis par le noyau au nom d'un thread immobilise par un evenement de
/// debogage (`in_fault`) vers l'endpoint du debogueur.
pub fn fault_call(ti: u16, ep_idx: u16) -> IpcResult {
    let e = ep(ep_idx);
    if e.dead {
        return Err(-EPIPE);
    }
    match e.recv_q.pop() {
        Some(r) => {
            transfer(ti, r, 0);
            let server_proc = thread::get(r).proc_idx;
            let t = thread::get(ti);
            t.state = State::BlockedReply;
            t.reply_from = server_proc;
            t.reply_holder = r;
            t.call_seq = t.call_seq.wrapping_add(1);
            let rt = thread::get(r);
            rt.frame.x[9] = thread::token(ti);
            rt.reply_to = ti;
            rt.blocked_on = NIL;
            sched::make_ready(r);
        }
        None => {
            let t = thread::get(ti);
            t.state = State::BlockedSend;
            t.blocked_on = ep_idx;
            t.send_badge = 0;
            t.send_is_call = true;
            e.send_q.push(ti);
        }
    }
    Ok(())
}

pub fn send(ti: u16, h: u32) -> IpcResult {
    let pi = thread::get(ti).proc_idx;
    let tmo = timeout_of(ti);
    let c = cap::lookup(pi, h, Kind::Endpoint, SEND)?;
    let e = ep(c.obj as u16);
    if e.dead {
        return Err(-EPIPE);
    }
    match e.recv_q.pop() {
        Some(r) => {
            transfer(ti, r, c.badge);
            let rt = thread::get(r);
            rt.reply_to = NIL;
            rt.blocked_on = NIL;
            thread::get(ti).frame.x[0] = 0;
            sched::make_ready(r);
        }
        None => {
            if tmo == Timeout::Now {
                return Err(-ETIMEDOUT);
            }
            let t = thread::get(ti);
            t.state = State::BlockedSend;
            t.blocked_on = c.obj as u16;
            t.send_badge = c.badge;
            t.send_is_call = false;
            e.send_q.push(ti);
            tmo.arm(ti);
        }
    }
    Ok(())
}

/// Partie reception, commune a `recv` et `reply_recv`. `Ok(true)` : le
/// thread repart avec un message ; `Ok(false)` : il s'est bloque.
fn receive(ti: u16, ep_idx: u16, tmo: Timeout) -> Result<bool, i64> {
    let t = thread::get(ti);
    if t.bound_notif != NIL {
        let bits = notify::take_bits(t.bound_notif);
        if bits != 0 {
            t.reply_to = NIL;
            deliver_notify(ti, bits);
            return Ok(true);
        }
    }
    let e = ep(ep_idx);
    if let Some(s) = e.send_q.pop() {
        let st = thread::get(s);
        let badge = st.send_badge;
        transfer(s, ti, badge);
        let st = thread::get(s);
        st.blocked_on = NIL;
        let t = thread::get(ti);
        if st.send_is_call {
            // Son delai eventuel reste arme : il couvre aussi la reponse.
            st.state = State::BlockedReply;
            st.reply_from = t.proc_idx;
            st.reply_holder = ti;
            st.call_seq = st.call_seq.wrapping_add(1);
            t.reply_to = s;
            t.frame.x[9] = thread::token(s);
        } else {
            st.frame.x[0] = 0;
            t.reply_to = NIL;
            sched::make_ready(s);
        }
        return Ok(true);
    }
    let t = thread::get(ti);
    t.reply_to = NIL;
    if e.pipe && e.send_refs == 0 {
        return Err(-EPIPE); // fin du tube
    }
    if tmo == Timeout::Now {
        return Err(-ETIMEDOUT);
    }
    t.state = State::BlockedRecv;
    t.blocked_on = ep_idx;
    e.recv_q.push(ti);
    tmo.arm(ti);
    Ok(false)
}

pub fn recv(ti: u16, h: u32) -> IpcResult {
    let pi = thread::get(ti).proc_idx;
    let tmo = timeout_of(ti);
    let c = cap::lookup(pi, h, Kind::Endpoint, RECV)?;
    receive(ti, c.obj as u16, tmo).map(|_| ())
}

/// Porte la reponse de `ti` a `client`, s'il l'attend bien de ce processus.
/// `Ok(false)` : le client ne doit pas etre reveille (debogueur l'ayant tue).
fn deliver_reply(ti: u16, client: u16) -> Result<bool, i64> {
    let cl = thread::get(client);
    if cl.state != State::BlockedReply || cl.reply_from != thread::get(ti).proc_idx {
        return Err(-ESRCH);
    }
    if cl.in_fault {
        // Reponse du debogueur : une commande, pas un message a recopier.
        let s = thread::get(ti);
        let (label, flags) = (tag::label(s.frame.x[1]), s.frame.x[2]);
        let cl = thread::get(client);
        cl.in_fault = false;
        cl.reply_from = NIL;
        cl.reply_holder = NIL;
        if label == nxk_abi::debug::KILL {
            crate::kern::process::kill(cl.proc_idx, 137);
            return Ok(false);
        }
        cl.single_step = flags & nxk_abi::debug::FLAG_STEP != 0;
        return Ok(true);
    }
    transfer(ti, client, 0);
    let cl = thread::get(client);
    cl.reply_from = NIL;
    cl.reply_holder = NIL;
    Ok(true)
}

pub fn reply(ti: u16) -> IpcResult {
    let client = core::mem::replace(&mut thread::get(ti).reply_to, NIL);
    if client == NIL {
        return Err(-ESRCH);
    }
    if deliver_reply(ti, client)? {
        sched::make_ready(client);
    }
    thread::get(ti).frame.x[0] = 0;
    Ok(())
}

pub fn reply_to(ti: u16, token: u64) -> IpcResult {
    let client = thread::from_token(token).ok_or(-ESRCH)?;
    if deliver_reply(ti, client)? {
        sched::make_ready(client);
    }
    thread::get(ti).frame.x[0] = 0;
    Ok(())
}

/// Boucle de service en un seul appel : repond au client courant puis attend
/// la requete suivante. Si aucune n'est prete, le coeur repart directement
/// avec le client (meme coeur) sans passer par l'ordonnanceur.
pub fn reply_recv(ti: u16, h: u32) -> IpcResult {
    let pi = thread::get(ti).proc_idx;
    let tmo = timeout_of(ti);
    let c = cap::lookup(pi, h, Kind::Endpoint, RECV)?;
    let client = core::mem::replace(&mut thread::get(ti).reply_to, NIL);
    let replied = client != NIL && deliver_reply(ti, client) == Ok(true);

    let got = receive(ti, c.obj as u16, tmo);
    if !matches!(got, Ok(false)) {
        // Message recu, ou rien et pas d'attente : le serveur continue, le
        // client repart de son cote.
        if replied {
            sched::make_ready(client);
        }
        return got.map(|_| ());
    }
    if replied {
        if thread::get(client).home as usize == percpu::this().id {
            sched::switch_to(client); // chemin rapide
        } else {
            sched::make_ready(client);
        }
    }
    Ok(())
}

//! 64-bit async notification words; bound waits break out of `recv`.

use nxk_abi::err::{EBUSY, EPIPE, ETIMEDOUT};

use crate::kern::bkl::BklCell;
use crate::kern::objtab::Table;
use crate::kern::sched::Timeout;
use crate::kern::thread::{self, State, NIL};
use crate::kern::{ipc, irq, sched};

pub struct Notification {
    pub used: bool,
    pub free_link: u16,
    pub gen: u16,
    pub refs: u32,
    pub bits: u64,
    pub waiter: u16,
    pub bound: u16,
}

fn n_reset(x: &mut Notification) {
    x.used = false;
    x.refs = 0;
    x.bits = 0;
    x.waiter = NIL;
    x.bound = NIL;
}

fn n_link(x: &mut Notification) -> &mut u16 {
    &mut x.free_link
}

pub static NOTIFS: BklCell<Table<Notification>> = BklCell::new(Table::new(n_reset, n_link));

#[inline(always)]
fn n(i: u16) -> &'static mut Notification {
    NOTIFS.get().get(i)
}

pub fn init() {}

pub fn alive(i: u16, gen: u16) -> bool {
    (i as usize) < NOTIFS.get().capacity() && n(i).used && n(i).gen == gen
}

pub fn create() -> Option<(u16, u16)> {
    let i = NOTIFS.get().alloc()?;
    let x = n(i);
    let gen = x.gen;
    n_reset(x);
    x.gen = gen;
    x.used = true;
    Some((i, x.gen))
}

pub fn incref(i: u16) {
    n(i).refs += 1;
}

pub fn decref(i: u16) {
    let x = n(i);
    x.refs -= 1;
    if x.refs == 0 {
        if x.waiter != NIL {
            let w = core::mem::replace(&mut x.waiter, NIL);
            thread::get(w).frame.x[0] = (-EPIPE) as u64;
            thread::get(w).blocked_on = NIL;
            sched::make_ready(w);
        }
        unbind(i);
        irq::forget_notification(i);
        x.used = false;
        x.gen = x.gen.wrapping_add(1);
        NOTIFS.get().free(i);
    }
}

/// Revocation: new generation; the in-flight wait fails, the thread binding
/// and interrupt routing (from old caps) are torn down.
pub fn revoke(i: u16) -> u16 {
    let x = n(i);
    x.gen = x.gen.wrapping_add(1);
    x.refs = 1;
    if x.waiter != NIL {
        let w = core::mem::replace(&mut x.waiter, NIL);
        thread::get(w).frame.x[0] = (-EPIPE) as u64;
        thread::get(w).blocked_on = NIL;
        sched::make_ready(w);
    }
    unbind(i);
    irq::forget_notification(i);
    n(i).gen
}

/// Signal bits (also called from the interrupt handler).
pub fn signal(i: u16, bits: u64) {
    let x = n(i);
    x.bits |= bits;
    if x.waiter != NIL {
        let w = core::mem::replace(&mut x.waiter, NIL);
        let t = thread::get(w);
        t.frame.x[0] = core::mem::take(&mut x.bits);
        t.blocked_on = NIL;
        sched::make_ready(w);
    } else if x.bound != NIL {
        let b = x.bound;
        let t = thread::get(b);
        if t.state == State::BlockedRecv {
            // Bound thread waits for a request: break it out.
            ipc::remove_waiter(t.blocked_on, b);
            t.blocked_on = NIL;
            t.frame.x[0] = 0;
            t.frame.x[1] = nxk_abi::tag::FLAG_NOTIFY;
            t.frame.x[2] = core::mem::take(&mut x.bits);
            t.frame.x[8] = 0;
            t.frame.x[9] = 0;
            sched::make_ready(b);
        }
    }
}

/// Return pending bits, or park the thread.
pub fn wait(ti: u16, i: u16, tmo: Timeout) -> Result<Option<u64>, i64> {
    let x = n(i);
    if x.bits != 0 {
        return Ok(Some(core::mem::take(&mut x.bits)));
    }
    if x.waiter != NIL {
        return Err(-EBUSY);
    }
    if tmo == Timeout::Now {
        return Err(-ETIMEDOUT);
    }
    x.waiter = ti;
    let t = thread::get(ti);
    t.state = State::BlockedNotify;
    t.blocked_on = i;
    tmo.arm(ti);
    Ok(None)
}

pub fn take_bits(i: u16) -> u64 {
    core::mem::take(&mut n(i).bits)
}

pub fn bind(i: u16, ti: u16) {
    unbind(i);
    let t = thread::get(ti);
    if t.bound_notif != NIL {
        n(t.bound_notif).bound = NIL;
    }
    n(i).bound = ti;
    t.bound_notif = i;
}

pub fn unbind(i: u16) {
    let x = n(i);
    if x.bound != NIL {
        thread::get(x.bound).bound_notif = NIL;
        x.bound = NIL;
    }
}

pub fn remove_waiter(i: u16, ti: u16) {
    if (i as usize) < NOTIFS.get().capacity() && n(i).waiter == ti {
        n(i).waiter = NIL;
    }
}

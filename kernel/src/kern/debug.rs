//! Minimal debugger hooks: attach, events, reg/mem access, stop.
//! seL4-style fault IPC to a user debugger endpoint.

use nxk_abi::debug::{Regs, KIND_STOP};
use nxk_abi::err::{EBUSY, EFAULT, EINVAL, ESRCH};
use nxk_abi::layout::PAGE_SIZE;
use nxk_abi::rights::SEND;

use crate::kern::thread::{self, State, NIL};
use crate::kern::{ipc, percpu, process, sched};
use crate::mm::{uaccess, vm};

/// SPSR SS flag: single-step active on return to EL0.
pub const SPSR_SS: u64 = 1 << 21;

/// Attach (`Some` endpoint) or detach (`None`) the debugger of `pi`.
pub fn attach(pi: u16, ep: Option<(u16, u16)>) {
    release(pi);
    let p = process::get(pi);
    match ep {
        Some((e, gen)) => {
            ipc::incref(e, SEND);
            p.debug_ep = e;
            p.debug_gen = gen;
            // Threads parked waiting for a debugger meet it now.
            for_each_thread(pi, |ti| {
                if thread::get(ti).state == State::Stopped && !event(ti, KIND_STOP, 0, 0) {
                    park(ti);
                }
            });
        }
        None => {
            for_each_thread(pi, |ti| {
                if thread::get(ti).state == State::Stopped {
                    sched::make_ready(ti);
                }
            });
        }
    }
}

/// Drop the debugger endpoint reference (process exit).
pub fn release(pi: u16) {
    let p = process::get(pi);
    if p.debug_ep != NIL {
        if ipc::alive(p.debug_ep, p.debug_gen) {
            ipc::decref(p.debug_ep, SEND);
        }
        p.debug_ep = NIL;
    }
}

fn for_each_thread(pi: u16, mut f: impl FnMut(u16)) {
    let mut ti = process::get(pi).threads_head;
    while ti != NIL {
        let next = thread::get(ti).proc_next;
        f(ti);
        ti = next;
    }
}

/// Report an event to the thread's process debugger: thread `ti` parks in a
/// call to the debugger endpoint. False with no debugger (or no receiver).
pub fn event(ti: u16, kind: u64, esr: u64, far: u64) -> bool {
    let t = thread::get(ti);
    let p = process::get(t.proc_idx);
    if p.debug_ep == NIL || !ipc::alive(p.debug_ep, p.debug_gen) {
        return false;
    }
    t.in_fault = true;
    t.fault_kind = kind as u8;
    t.fault_esr = esr;
    t.fault_far = far;
    if ipc::fault_call(ti, p.debug_ep).is_err() {
        thread::get(ti).in_fault = false;
        return false;
    }
    true
}

/// Park with no debugger: the thread waits for one to attach to its process.
pub fn park(ti: u16) {
    let t = thread::get(ti);
    t.state = State::Stopped;
    t.blocked_on = NIL;
}

/// Ask a thread to stop: taken at its next kernel exit.
pub fn stop(ti: u16) {
    let t = thread::get(ti);
    if t.in_fault || matches!(t.state, State::Stopped | State::Zombie | State::Free) {
        return;
    }
    t.stop_request = true;
    let home = t.home as usize;
    if t.state == State::Running && home != percpu::this().id {
        percpu::get(home).need_resched = true;
        sched::kick(home);
    }
}

/// Thread `tid` of the process, parked (neither running nor dead).
fn stopped_thread(pi: u16, tid: u32) -> Result<u16, i64> {
    let ti = process::find_thread(pi, tid).ok_or(-ESRCH)?;
    let st = thread::get(ti).state;
    if st == State::Running || percpu::this().current == ti {
        return Err(-EBUSY);
    }
    Ok(ti)
}

pub fn read_regs(pi: u16, tid: u32) -> Result<Regs, i64> {
    let ti = stopped_thread(pi, tid)?;
    let f = &thread::get(ti).frame;
    Ok(Regs {
        x: f.x,
        sp: f.sp,
        pc: f.elr,
        pstate: f.spsr & !SPSR_SS,
        tpidr: f.tpidr,
    })
}

pub fn write_regs(pi: u16, tid: u32, r: &Regs) -> Result<(), i64> {
    let ti = stopped_thread(pi, tid)?;
    let f = &mut thread::get(ti).frame;
    f.x = r.x;
    f.sp = r.sp;
    f.elr = r.pc;
    // NZCV flags only: exception level and masks stay with the kernel.
    f.spsr = (f.spsr & !0xF000_0000) | (r.pstate & 0xF000_0000);
    f.tpidr = r.tpidr;
    Ok(())
}

/// Copy between the debugged process memory (`addr`) and the caller's (`buf`),
/// whatever the target protections.
pub fn mem(pi: u16, addr: u64, buf: u64, len: u64, write: bool) -> Result<u64, i64> {
    if len > 64 * 1024 {
        return Err(-EINVAL);
    }
    let l1 = process::get(pi).l1;
    let mut tmp = [0u8; 256];
    let mut off = 0u64;
    while off < len {
        let a = addr.checked_add(off).ok_or(-EFAULT)?;
        let page = a & !(PAGE_SIZE - 1);
        let mut pte = vm::lookup(l1, page);
        if pte & 1 == 0 {
            if !process::populate(pi, page) {
                return Err(-EFAULT);
            }
            pte = vm::lookup(l1, page);
        }
        if vm::is_device(pte) {
            return Err(-EFAULT);
        }
        let pa = vm::pte_addr(pte) + (a - page);
        let n = (PAGE_SIZE - (a - page))
            .min(len - off)
            .min(tmp.len() as u64) as usize;
        if write {
            uaccess::copy_from_user(&mut tmp[..n], buf + off)?;
            unsafe { core::ptr::copy_nonoverlapping(tmp.as_ptr(), pa as *mut u8, n) };
            if pte & (1 << 54) == 0 {
                vm::sync_icache(pa, n as u64);
            }
        } else {
            unsafe { core::ptr::copy_nonoverlapping(pa as *const u8, tmp.as_mut_ptr(), n) };
            uaccess::copy_to_user(buf + off, &tmp[..n])?;
        }
        off += n as u64;
    }
    Ok(len)
}

/// Live thread IDs of the process; returns their count.
pub fn threads(pi: u16, buf: u64, max: u64) -> Result<u64, i64> {
    let mut n = 0u64;
    let mut err = Ok(());
    for_each_thread(pi, |ti| {
        let t = thread::get(ti);
        if t.state == State::Zombie {
            return;
        }
        if n < max && err.is_ok() {
            err = uaccess::write(buf + n * 4, &t.tid);
        }
        n += 1;
    });
    err.map(|_| n)
}

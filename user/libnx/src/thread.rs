//! Threads d'un meme processus.

use alloc::boxed::Box;
use alloc::sync::Arc;

use nxk_abi::prot;

use crate::sync::Mutex;
use crate::{mem, sys};

const STACK_SIZE: usize = 64 * 1024;

/// Piles de threads termines, gardees pour les suivants (comme glibc) : ni
/// projection ni fautes de page a la creation d'un thread.
static STACKS: Mutex<([usize; 8], usize)> = Mutex::new(([0; 8], 0));

fn stack_get() -> Result<*mut u8, i64> {
    let mut c = STACKS.lock();
    if c.1 > 0 {
        c.1 -= 1;
        let n = c.1;
        return Ok(c.0[n] as *mut u8);
    }
    drop(c);
    mem::map(STACK_SIZE, prot::READ | prot::WRITE)
}

fn stack_put(p: *mut u8) {
    let mut c = STACKS.lock();
    if c.1 < c.0.len() {
        let n = c.1;
        c.0[n] = p as usize;
        c.1 += 1;
    } else {
        drop(c);
        let _ = mem::unmap(p, STACK_SIZE);
    }
}

/// Notification de fin, partagee entre le thread et son `JoinHandle`.
struct Done {
    notif: u32,
}

impl Drop for Done {
    fn drop(&mut self) {
        let _ = sys::handle_close(self.notif);
    }
}

struct Payload {
    f: Box<dyn FnOnce() + Send>,
    done: Arc<Done>,
    tp: u64,
}

pub struct JoinHandle {
    done: Arc<Done>,
    stack: *mut u8,
    handle: u32,
    tp: u64,
}

unsafe impl Send for JoinHandle {}

extern "C" fn trampoline(arg: u64) -> ! {
    let p = unsafe { Box::from_raw(arg as *mut Payload) };
    let Payload { f, done, tp } = *p;
    unsafe { crate::tls::set_tp(tp) };
    f();
    let notif = done.notif;
    drop(done);
    // Signalee par le noyau apres la sortie : plus aucun octet de cette pile
    // ne sera touche quand l'attente se termine.
    sys::thread_exit_notify(0, notif, 1)
}

/// Lance un thread ; `cpu` l'epingle a un coeur, `prio` 0 = defaut.
pub fn spawn_on<F: FnOnce() + Send + 'static>(cpu: Option<u8>, prio: u8, f: F) -> Result<JoinHandle, i64> {
    let stack = stack_get()?;
    let notif = match sys::notify_create() {
        Ok(n) => n,
        Err(e) => {
            stack_put(stack);
            return Err(e);
        }
    };
    let done = Arc::new(Done { notif });
    let tp = crate::tls::alloc_block().unwrap_or(0);
    let payload = Box::into_raw(Box::new(Payload { f: Box::new(f), done: done.clone(), tp }));
    let flags = nxk_abi::thread::flags(prio, cpu);
    match sys::thread_create(trampoline as *const () as u64, stack as u64 + STACK_SIZE as u64, payload as u64, flags) {
        Ok(handle) => Ok(JoinHandle { done, stack, handle, tp }),
        Err(e) => {
            drop(unsafe { Box::from_raw(payload) });
            crate::tls::free_block(tp);
            stack_put(stack);
            Err(e)
        }
    }
}

pub fn spawn<F: FnOnce() + Send + 'static>(f: F) -> Result<JoinHandle, i64> {
    spawn_on(None, 0, f)
}

impl JoinHandle {
    /// Attend la fin du thread, puis rend sa pile.
    pub fn join(self) {
        let _ = sys::notify_wait(self.done.notif);
        stack_put(self.stack);
        crate::tls::free_block(self.tp);
        let _ = sys::handle_close(self.handle);
    }

    /// Handle noyau du thread (reglages, debogage).
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Priorite 1 (la plus haute) a 30 ; 0 = defaut.
    pub fn set_priority(&self, prio: u8) -> Result<(), i64> {
        sys::thread_set(self.handle, nxk_abi::thread::SET_PRIO, prio as u64)
    }

    /// Impose un coeur (`Some`), ou laisse le noyau choisir (`None`).
    pub fn set_affinity(&self, cpu: Option<u8>) -> Result<(), i64> {
        set_affinity_of(self.handle, cpu)
    }
}

fn set_affinity_of(h: u32, cpu: Option<u8>) -> Result<(), i64> {
    let v = cpu.map(|c| c as u64).unwrap_or(nxk_abi::thread::ANY_CPU);
    sys::thread_set(h, nxk_abi::thread::SET_AFFINITY, v)
}

/// Reglages du thread appelant.
pub fn set_current_priority(prio: u8) -> Result<(), i64> {
    sys::thread_set(0, nxk_abi::thread::SET_PRIO, prio as u64)
}

pub fn set_current_affinity(cpu: Option<u8>) -> Result<(), i64> {
    set_affinity_of(0, cpu)
}

//! Threads : un thread du noyau par thread Rust.
//!
//! Une seule projection par thread : la pile (sous laquelle le noyau laisse
//! toujours une page libre, qui sert de garde) puis le bloc TLS. La fin d'un
//! thread est signalee par une notification que le noyau pose une fois le
//! thread sorti d'EL0 : alors seulement sa pile peut etre rendue. Un thread
//! detache se range a sa sortie dans une liste que les creations suivantes
//! vident.

use crate::cell::UnsafeCell;
use crate::ffi::CStr;
use crate::io;
use crate::num::NonZero;
use crate::sync::Arc;
use crate::sync::atomic::{AtomicU32, Ordering};
use crate::sys::pal::abi::{info, timeout};
use crate::sys::pal::{cvt, syscall, tls, vm};
use crate::sys::sync::Mutex;
use crate::thread::ThreadInit;
use crate::time::Duration;

/// Pages allouees au premier acces : une grande pile ne coute que ce qui sert.
pub const DEFAULT_MIN_STACK_SIZE: usize = 2 * 1024 * 1024;

const DETACHED: u32 = 1;
const EXITED: u32 = 2;

/// Etat partage entre le thread et son `Thread`.
struct Shared {
    state: AtomicU32,
    /// Notification posee par le noyau a la sortie du thread.
    notif: u32,
    /// Handle noyau du thread (connu apres sa creation).
    handle: AtomicU32,
    base: usize,
    len: usize,
}

impl Shared {
    /// Rend la pile et les handles d'un thread sorti.
    fn free(&self) {
        // SAFETY: le thread est sorti d'EL0, personne n'utilise plus sa pile.
        unsafe { stack_put(self.base as *mut u8, self.len) };
        syscall::handle_close(self.notif);
        let h = self.handle.load(Ordering::Relaxed);
        if h != 0 {
            syscall::handle_close(h);
        }
    }
}

/// Piles de threads termines, gardees pour les suivants (comme glibc) : ni
/// projection ni fautes de page a la creation d'un thread. Leur contenu est
/// celui du thread precedent du meme processus ; seul le bloc TLS est remis
/// a neuf.
struct Stacks {
    lock: Mutex,
    free: UnsafeCell<[(usize, usize); STACK_CACHE]>,
}

// SAFETY: `free` n'est touche que sous `lock`.
unsafe impl Sync for Stacks {}

const STACK_CACHE: usize = 8;

static STACKS: Stacks = Stacks { lock: Mutex::new(), free: UnsafeCell::new([(0, 0); STACK_CACHE]) };

fn stacks<R>(f: impl FnOnce(&mut [(usize, usize); STACK_CACHE]) -> R) -> R {
    STACKS.lock.lock();
    // SAFETY: acces exclusif sous le verrou.
    let r = f(unsafe { &mut *STACKS.free.get() });
    unsafe { STACKS.lock.unlock() };
    r
}

/// Pile de `len` octets : une pile gardee de meme taille, ou une neuve.
fn stack_get(len: usize) -> Option<(*mut u8, bool)> {
    let hit = stacks(|c| {
        let k = c.iter().position(|&(b, l)| b != 0 && l == len)?;
        let b = c[k].0;
        c[k] = (0, 0);
        Some(b)
    });
    match hit {
        Some(b) => Some((b as *mut u8, false)),
        None => vm::map(len).map(|p| (p, true)),
    }
}

/// # Safety
/// Plus aucun thread ne doit utiliser la pile.
unsafe fn stack_put(base: *mut u8, len: usize) {
    let kept = stacks(|c| match c.iter().position(|&(b, _)| b == 0) {
        Some(k) => {
            c[k] = (base as usize, len);
            true
        }
        None => false,
    });
    if !kept {
        unsafe { vm::unmap(base, len) };
    }
}

/// Threads detaches sortis (ou sur le point de l'etre), a liberer.
struct Zombies {
    lock: Mutex,
    list: UnsafeCell<Vec<Arc<Shared>>>,
}

// SAFETY: `list` n'est touche que sous `lock`.
unsafe impl Sync for Zombies {}

static ZOMBIES: Zombies = Zombies { lock: Mutex::new(), list: UnsafeCell::new(Vec::new()) };

fn zombies<R>(f: impl FnOnce(&mut Vec<Arc<Shared>>) -> R) -> R {
    ZOMBIES.lock.lock();
    // SAFETY: acces exclusif sous le verrou.
    let r = f(unsafe { &mut *ZOMBIES.list.get() });
    unsafe { ZOMBIES.lock.unlock() };
    r
}

/// Libere les threads detaches dont le noyau a confirme la sortie.
fn reap() {
    let done: Vec<Arc<Shared>> = zombies(|z| {
        let mut done = Vec::new();
        z.retain(|s| {
            if syscall::notify_wait(s.notif, timeout::POLL) >= 0 {
                done.push(s.clone());
                false
            } else {
                true
            }
        });
        done
    });
    for s in done {
        s.free();
    }
}

struct Packet {
    init: Box<ThreadInit>,
    shared: Arc<Shared>,
    tp: u64,
}

pub struct Thread {
    shared: Option<Arc<Shared>>,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

extern "C" fn thread_start(arg: u64) -> ! {
    // SAFETY: boite laissee par `Thread::new` pour ce thread seul.
    let p = unsafe { Box::from_raw(crate::ptr::with_exposed_provenance_mut::<Packet>(arg as usize)) };
    let Packet { init, shared, tp } = *p;
    // SAFETY: bloc TLS initialise par `Thread::new`.
    unsafe { tls::set_tp(tp) };
    let rust_start = init.init();
    rust_start();

    // SAFETY: fin du code Rust de ce thread.
    unsafe { crate::sys::thread_local::destructors::run() };
    crate::rt::thread_cleanup();

    let notif = shared.notif;
    if shared.state.fetch_or(EXITED, Ordering::AcqRel) & DETACHED != 0 {
        zombies(|z| z.push(shared));
    } else {
        drop(shared);
    }
    syscall::thread_exit_notify(notif, 1)
}

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        reap();
        let stack = stack.max(16 * 1024).next_multiple_of(vm::PAGE);
        let tls_len = tls::block_size().next_multiple_of(vm::PAGE);
        let len = stack + tls_len;
        let (base, fresh) =
            stack_get(len).ok_or(io::const_error!(io::ErrorKind::OutOfMemory, "pas de memoire pour la pile"))?;
        let tp = if tls_len > 0 {
            // SAFETY: zone de `tls_len` octets alignee sur la page, remise a
            // zero si elle a deja servi.
            unsafe {
                if !fresh {
                    crate::ptr::write_bytes(base.add(stack), 0, tls::block_size());
                }
                tls::init_block(base.add(stack))
            }
        } else {
            0
        };
        let notif = match cvt(syscall::notify_create()) {
            Ok(n) => n as u32,
            Err(e) => {
                unsafe { stack_put(base, len) };
                return Err(e);
            }
        };
        let shared = Arc::new(Shared {
            state: AtomicU32::new(0),
            notif,
            handle: AtomicU32::new(0),
            base: base as usize,
            len,
        });
        let packet = Box::into_raw(Box::new(Packet { init, shared: shared.clone(), tp }));
        let sp = base as u64 + stack as u64;
        let r = syscall::thread_create(thread_start as *const () as u64, sp, packet.expose_provenance() as u64, 0);
        match cvt(r) {
            Ok(h) => {
                shared.handle.store(h as u32, Ordering::Relaxed);
                Ok(Thread { shared: Some(shared) })
            }
            Err(e) => {
                // Le thread n'a pas demarre : tout revient a l'appelant.
                drop(unsafe { Box::from_raw(packet) });
                shared.free();
                Err(e)
            }
        }
    }

    pub fn join(mut self) {
        let s = self.shared.take().unwrap();
        syscall::notify_wait(s.notif, timeout::INFINITE);
        s.free();
    }
}

impl Drop for Thread {
    /// Thread detache : s'il est deja sorti, on le libere ici ; sinon il se
    /// rangera lui-meme parmi les zombies.
    fn drop(&mut self) {
        if let Some(s) = self.shared.take() {
            if s.state.fetch_or(DETACHED, Ordering::AcqRel) & EXITED != 0 {
                syscall::notify_wait(s.notif, timeout::INFINITE);
                s.free();
            }
        }
    }
}

pub fn set_name(_name: &CStr) {}

pub fn current_os_id() -> Option<u64> {
    Some(syscall::gettid())
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    let sz = size_of::<info::CpuInfo>();
    let mut buf = [0u8; 16 * size_of::<info::CpuInfo>()];
    let n = cvt(syscall::sysinfo(info::CPUS, &mut buf))? as usize;
    let online = (0..n.min(16))
        .filter(|&k| {
            // SAFETY: le noyau a ecrit `n` fiches `CpuInfo` a la suite.
            let c = unsafe { (buf.as_ptr().add(k * sz) as *const info::CpuInfo).read_unaligned() };
            c.online != 0
        })
        .count();
    Ok(NonZero::new(online).unwrap_or(NonZero::<usize>::MIN))
}

pub fn yield_now() {
    syscall::yield_now()
}

pub fn sleep(dur: Duration) {
    let mut ns = dur.as_nanos();
    while ns > 0 {
        let step = ns.min(u64::MAX as u128) as u64;
        syscall::sleep_ns(step);
        ns -= step as u128;
    }
}

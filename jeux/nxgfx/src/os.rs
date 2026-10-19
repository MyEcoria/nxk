//! The few system calls the SDK needs, over `nxk_abi`'s raw stubs (the
//! SDK has no libnx: games are `std` programs).

use nxk_abi::err::ENOSYS;
use nxk_abi::proto::pm;
use nxk_abi::sys;
use nxk_abi::user::{self, syscall1, syscall2, syscall3, Msg};
use nxk_abi::{prot, slots};

pub type Result<T> = core::result::Result<T, i64>;

fn check(r: i64) -> Result<u64> {
    if r < 0 {
        Err(r)
    } else {
        Ok(r as u64)
    }
}

pub fn handle_close(h: u32) {
    unsafe { syscall1(sys::HANDLE_CLOSE, h as u64) };
}

/// Monotonic time, ns.
pub fn now_ns() -> u64 {
    user::monotonic_ns()
}

pub fn sleep_ns(ns: u64) {
    unsafe { syscall1(sys::SLEEP, ns) };
}

/// A shared memory object, mapped here, closed with it. Handles go to
/// services by `Service::call_with`: the service gets its own.
pub struct Shm {
    handle: u32,
    len: usize,
    ptr: *mut u8,
}

impl Shm {
    pub fn new(len: usize, flags: u64) -> Result<Self> {
        let handle = check(unsafe { syscall2(sys::SHM_CREATE, len as u64, flags) })? as u32;
        match check(unsafe { syscall3(sys::SHM_MAP, handle as u64, 0, prot::READ | prot::WRITE) }) {
            Ok(a) => Ok(Self {
                handle,
                len,
                ptr: a as *mut u8,
            }),
            Err(e) => {
                handle_close(handle);
                Err(e)
            }
        }
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn write(&self, off: usize, data: &[u8]) {
        assert!(off + data.len() <= self.len);
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(off), data.len()) };
    }

    pub fn write_words(&self, off: usize, w: &[u32]) {
        assert!(off + w.len() * 4 <= self.len && off % 4 == 0);
        unsafe {
            core::ptr::copy_nonoverlapping(w.as_ptr(), self.ptr.add(off) as *mut u32, w.len())
        };
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        let len = self.len.next_multiple_of(4096) as u64;
        unsafe { syscall2(sys::MEM_UNMAP, self.ptr as u64, len) };
        handle_close(self.handle);
    }
}

/// A service endpoint, as the name service gave it.
pub struct Service {
    ep: u32,
}

/// Error reply: its label is the errno.
fn checked(m: Msg) -> Result<Msg> {
    match m.label() {
        0 => Ok(m),
        e => Err(-(e as i64)),
    }
}

impl Service {
    pub fn lookup(name: &str) -> Result<Self> {
        let pm_ep = slots::PM;
        let (m, _) = user::pack_at(pm::LOOKUP, &[], name.as_bytes());
        checked(user::call(pm_ep, &m, 0)?)?;
        let ep = user::received_handle(0);
        if ep == 0 {
            return Err(-ENOSYS);
        }
        Ok(Self { ep })
    }

    /// An endpoint received in a reply.
    pub fn from_handle(ep: u32) -> Self {
        Self { ep }
    }

    pub fn call(&self, m: &Msg) -> Result<Msg> {
        checked(user::call(self.ep, m, 0)?)
    }

    pub fn call_timeout(&self, m: &Msg, ns: u64) -> Result<Msg> {
        checked(user::call(self.ep, m, ns)?)
    }

    /// `m` with the handle `h` staged as its first.
    pub fn call_with(&self, m: Msg, h: u32) -> Result<Msg> {
        user::set_send_handle(0, h);
        self.call(&m.with_handles(1))
    }
}

/// Handle `k` of the reply just received.
pub fn received_handle(k: usize) -> u32 {
    user::received_handle(k)
}

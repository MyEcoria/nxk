//! Driver helpers: mapped registers, interrupts, service loop.

use core::ptr::{read_volatile, write_volatile};

use nxk_abi::rights::{DUP, GRANT, SEND};
use nxk_abi::slots;

use crate::ipc::{self, Msg, Received};
use crate::ns::Service;
use crate::{ns, sys};

/// Found on first use: the screen may still be starting.
static DISPLAY: Service = Service::new("display", 5_000_000_000);

/// Text for the bottom lines of the screen (`display` WRITE).
pub fn screen(text: &str) -> Result<(), i64> {
    DISPLAY
        .call(&ipc::pack(nxk_abi::proto::display::WRITE, text.as_bytes()))
        .map(|_| ())
}

/// A driver failure: `[who] text` on the console, `who: text` on the
/// screen, a Switch's only output without a PC attached.
pub fn report(who: &str, text: &str) {
    crate::println!("[{}] {}", who, text);
    let _ = screen(&alloc::format!("{}: {}\n", who, text));
}

/// Hardware capability number `k` received from init.
pub const fn dev_slot(k: u32) -> u32 {
    slots::DEV_BASE + k
}

/// Register pages of a device, mapped into the process.
#[derive(Clone, Copy)]
pub struct Mmio {
    base: usize,
}

impl Mmio {
    /// Map `len` bytes at physical address `phys`.
    pub fn map(cap: u32, phys: u64, len: u64) -> Result<Self, i64> {
        Ok(Self {
            base: sys::mmio_map(cap, phys, len)? as usize,
        })
    }

    /// Sub-block at `off` bytes (several controllers per page).
    pub const fn offset(self, off: usize) -> Self {
        Self {
            base: self.base + off,
        }
    }

    #[inline(always)]
    pub fn r32(&self, off: usize) -> u32 {
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    #[inline(always)]
    pub fn w32(&self, off: usize, v: u32) {
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }

    #[inline(always)]
    pub fn r16(&self, off: usize) -> u16 {
        unsafe { read_volatile((self.base + off) as *const u16) }
    }

    #[inline(always)]
    pub fn w16(&self, off: usize, v: u16) {
        unsafe { write_volatile((self.base + off) as *mut u16, v) }
    }

    #[inline(always)]
    pub fn r8(&self, off: usize) -> u8 {
        unsafe { read_volatile((self.base + off) as *const u8) }
    }

    #[inline(always)]
    pub fn w8(&self, off: usize, v: u8) {
        unsafe { write_volatile((self.base + off) as *mut u8, v) }
    }

    #[inline]
    pub fn modify(&self, off: usize, clear: u32, set: u32) {
        self.w32(off, (self.r32(off) & !clear) | set);
    }

    /// Wait for `(r32(off) & mask) == val`, at most `timeout_us`
    /// (`time::poll`: long waits sleep rather than spin).
    pub fn wait(&self, off: usize, mask: u32, val: u32, timeout_us: u64) -> bool {
        crate::time::poll(timeout_us, || self.r32(off) & mask == val)
    }
}

/// Interrupt line from init, delivered via a notification.
pub struct Irq {
    cap: u32,
    notif: u32,
}

impl Irq {
    pub fn new(cap: u32) -> Result<Self, i64> {
        let notif = sys::notify_create()?;
        sys::irq_bind(cap, notif, 1)?;
        Ok(Self { cap, notif })
    }

    /// Notification to bind to the service thread (`notify_bind`)
    /// to receive the interrupt as a message.
    pub fn notification(&self) -> u32 {
        self.notif
    }

    /// Wait for the interrupt (0: no limit); false on timeout.
    pub fn wait(&self, timeout_ns: u64) -> bool {
        sys::notify_wait_timeout(self.notif, timeout_ns).is_ok()
    }

    /// Re-arm the line (masked by the kernel on each trigger).
    pub fn ack(&self) {
        let _ = sys::irq_ack(self.cap);
    }
}

/// Create a service endpoint and publish it as `name`.
pub fn publish(name: &str) -> Result<u32, i64> {
    let ep = sys::endpoint_create()?;
    let client = sys::handle_dup(ep, SEND | GRANT | DUP, 0)?;
    let r = ns::register(name, client);
    let _ = sys::handle_close(client);
    r.map(|_| ep)
}

/// Service loop: `f` answers each request; `None` = deferred reply
/// (the handler kept `rx.token` for `ipc::reply_to`).
pub fn serve(ep: u32, mut f: impl FnMut(&Received) -> Option<Msg>) -> ! {
    let recv = || loop {
        if let Ok(r) = ipc::recv(ep) {
            break r;
        }
    };
    let mut rx = recv();
    loop {
        rx = match f(&rx).map(|reply| ipc::reply_recv(ep, &reply)) {
            Some(Ok(r)) => r,
            _ => recv(),
        };
    }
}

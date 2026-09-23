//! Kernel log: a ring the console service drains (`klog_read`), so no
//! message ever waits on a device. Until that service binds, messages
//! also go straight to UART-A and the loader's screen (`fbcon`, boot);
//! panic paths always do. Lock-free before MMU: exclusives are unsafe on
//! Device memory.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::fbcon;
use crate::drivers::uart::{Uart, UART_A};
use crate::kern::thread::NIL;
use crate::kern::{bkl, notify};
use crate::sync::SpinLock;

/// Ring size: the boot log is ~2 KiB, runtime messages are rare.
const RING: usize = 8 * 1024;

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console {
    uart: Uart::new(UART_A),
    ready: false,
    ring: [0; RING],
    head: 0,
    tail: 0,
    notif: NIL,
    notif_gen: 0,
    bits: 0,
    signaled: false,
});

/// MMU and caches on: the console lock is usable.
static LOCKED: AtomicBool = AtomicBool::new(false);

pub struct Console {
    uart: Uart,
    ready: bool,
    ring: [u8; RING],
    /// Bytes ever written, and read by the drain.
    head: u64,
    tail: u64,
    /// The drain's notification (`NIL`: none yet, print directly).
    notif: u16,
    notif_gen: u16,
    bits: u64,
    /// Drain notified and not caught up since.
    signaled: bool,
}

impl Console {
    fn push(&mut self, b: &[u8]) {
        for &x in b {
            self.ring[self.head as usize % RING] = x;
            self.head += 1;
        }
        // Unread and overwritten: the oldest bytes are lost.
        self.tail = self.tail.max(self.head.saturating_sub(RING as u64));
    }

    /// Notification owed to the drain: once per catch-up.
    fn take_signal(&mut self) -> Option<(u16, u16, u64)> {
        if self.notif == NIL || self.signaled || self.head == self.tail {
            return None;
        }
        self.signaled = true;
        Some((self.notif, self.notif_gen, self.bits))
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push(s.as_bytes());
        if self.ready && self.notif == NIL {
            self.uart.write_str(s);
            fbcon::write_str(s);
        }
        Ok(())
    }
}

/// Console access: locked once the MMU is on, direct before that. The
/// drain is signaled after the lock is dropped (signaling may log).
fn with<R>(f: impl FnOnce(&mut Console) -> R) -> R {
    if !LOCKED.load(Ordering::Acquire) {
        return f(unsafe { CONSOLE.get_unlocked() });
    }
    let (r, sig) = {
        let mut c = CONSOLE.lock_irqsafe();
        let r = f(&mut c);
        // Waking a thread needs the global lock: without it, the next
        // message under it signals.
        (r, if bkl::held() { c.take_signal() } else { None })
    };
    if let Some((n, gen, bits)) = sig {
        if notify::alive(n, gen) {
            notify::signal(n, bits);
        }
    }
    r
}

/// Call with MMU off, core 0 alone.
pub fn init() {
    with(|c| {
        c.uart.init(115_200);
        c.ready = true;
    });
    fbcon::init();
}

/// MMU is on: further messages go through the lock.
pub fn enable_lock() {
    LOCKED.store(true, Ordering::Release);
}

/// Raw bytes (`debug_write` syscall).
pub fn write_bytes(b: &[u8]) {
    with(|c| {
        c.push(b);
        if c.ready && c.notif == NIL {
            for &x in b {
                if x == b'\n' {
                    c.uart.put(b'\r');
                }
                c.uart.put(x);
            }
            if let Ok(s) = core::str::from_utf8(b) {
                fbcon::write_str(s);
            }
        }
    });
}

/// The drain binds (`klog_bind`): signaled on new data, and the kernel
/// stops printing itself. Returns the unread bytes already printed.
pub fn bind(notif: u16, notif_gen: u16, bits: u64) -> u64 {
    with(|c| {
        c.notif = notif;
        c.notif_gen = notif_gen;
        c.bits = bits;
        c.signaled = false;
        c.head - c.tail
    })
}

/// Unread bytes into `out` (`klog_read`); returns the count.
pub fn read(out: &mut [u8]) -> usize {
    with(|c| {
        let n = out.len().min((c.head - c.tail) as usize);
        for (k, o) in out[..n].iter_mut().enumerate() {
            *o = c.ring[(c.tail as usize + k) % RING];
        }
        c.tail += n as u64;
        if c.tail == c.head {
            c.signaled = false;
        }
        n
    })
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = with(|c| c.write_fmt(args));
}

/// Emergency output, UART plus screen: ignores the lock and formats
/// nothing (panic, fatal exception).
pub fn emergency_str(s: &str) {
    Uart::new(UART_A).write_str(s);
    fbcon::write_str(s);
}

#[doc(hidden)]
pub fn _print_emergency(args: fmt::Arguments) {
    struct Emergency;
    impl Write for Emergency {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            emergency_str(s);
            Ok(())
        }
    }
    let _ = Emergency.write_fmt(args);
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Lock-free variant, for paths where the lock may already be held.
#[macro_export]
macro_rules! keprintln {
    ($($arg:tt)*) => { $crate::console::_print_emergency(format_args!("{}\n", format_args!($($arg)*))) };
}

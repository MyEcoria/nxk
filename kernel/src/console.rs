//! Locked UART console mirrored to the loader screen (`fbcon`).
//! Lock-free before MMU: exclusives are unsafe on Device memory.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::fbcon;
use crate::drivers::uart::{Uart, UART_A};
use crate::sync::SpinLock;

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console {
    uart: Uart::new(UART_A),
    ready: false,
});

/// MMU and caches on: the console lock is usable.
static LOCKED: AtomicBool = AtomicBool::new(false);

pub struct Console {
    uart: Uart,
    ready: bool,
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.ready {
            self.uart.write_str(s);
            fbcon::write_str(s);
        }
        Ok(())
    }
}

/// Console access: locked once the MMU is on, direct before that.
fn with<R>(f: impl FnOnce(&mut Console) -> R) -> R {
    if LOCKED.load(Ordering::Acquire) {
        f(&mut CONSOLE.lock_irqsafe())
    } else {
        f(unsafe { CONSOLE.get_unlocked() })
    }
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

/// Write raw bytes (`debug_write` syscall).
pub fn write_bytes(b: &[u8]) {
    with(|c| {
        if c.ready {
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

//! Console noyau : un UART unique partage, protege par un verrou.
//!
//! Les macros `kprint!`/`kprintln!` prennent le verrou le temps d'un message,
//! ce qui garde les lignes entieres meme quand plusieurs coeurs ecrivent.

use core::fmt::{self, Write};

use crate::drivers::uart::{Uart, UART_A};
use crate::sync::SpinLock;

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console {
    uart: Uart::new(UART_A),
    ready: false,
});

pub struct Console {
    uart: Uart,
    ready: bool,
}


impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.ready {
            self.uart.write_str(s);
        }
        Ok(())
    }
}

pub fn init() {
    let mut c = CONSOLE.lock_irqsafe();
    c.uart.init(115_200);
    c.ready = true;
}

/// Ecrit des octets bruts (appel systeme `debug_write`).
pub fn write_bytes(b: &[u8]) {
    let c = CONSOLE.lock_irqsafe();
    if c.ready {
        for &x in b {
            if x == b'\n' {
                c.uart.put(b'\r');
            }
            c.uart.put(x);
        }
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = CONSOLE.lock_irqsafe().write_fmt(args);
}

/// Ecriture d'urgence : ignore le verrou (panique, exception fatale).
#[doc(hidden)]
pub fn _print_emergency(args: fmt::Arguments) {
    let mut uart = Uart::new(UART_A);
    let _ = uart.write_fmt(args);
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

/// Variante sans verrou, pour les chemins ou le verrou peut etre deja pris.
#[macro_export]
macro_rules! keprintln {
    ($($arg:tt)*) => { $crate::console::_print_emergency(format_args!("{}\n", format_args!($($arg)*))) };
}

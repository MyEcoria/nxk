//! Console noyau : un UART unique partage, protege par un verrou, et recopie
//! sur l'ecran laisse allume par le chargeur (`drivers::fbcon`).
//!
//! Les macros `kprint!`/`kprintln!` prennent le verrou le temps d'un message,
//! ce qui garde les lignes entieres meme quand plusieurs coeurs ecrivent.
//!
//! Pas de verrou avant la MMU : ses atomiques sont des acces exclusifs
//! (LDXR/STXR, pas de LSE sur Cortex-A57) qui visent alors de la memoire
//! Device. L'architecture ne garantit pas le moniteur exclusif global pour
//! ce type de memoire : sur le SoC, un STXR peut echouer indefiniment
//! (l'emulateur, lui, reussit toujours). Seul le coeur 0 tourne a ce stade.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::drivers::fbcon;
use crate::drivers::uart::{Uart, UART_A};
use crate::sync::SpinLock;

pub static CONSOLE: SpinLock<Console> = SpinLock::new(Console {
    uart: Uart::new(UART_A),
    ready: false,
});

/// MMU et caches actifs : le verrou de la console est utilisable.
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

/// Acces a la console : sous verrou une fois la MMU active, direct avant.
fn with<R>(f: impl FnOnce(&mut Console) -> R) -> R {
    if LOCKED.load(Ordering::Acquire) {
        f(&mut CONSOLE.lock_irqsafe())
    } else {
        f(unsafe { CONSOLE.get_unlocked() })
    }
}

/// A appeler MMU coupee, coeur 0 seul.
pub fn init() {
    with(|c| {
        c.uart.init(115_200);
        c.ready = true;
    });
    fbcon::init();
}

/// La MMU est active : les messages suivants passent par le verrou.
pub fn enable_lock() {
    LOCKED.store(true, Ordering::Release);
}

/// Ecrit des octets bruts (appel systeme `debug_write`).
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

/// Ecriture d'urgence : ignore le verrou (panique, exception fatale).
#[doc(hidden)]
pub fn _print_emergency(args: fmt::Arguments) {
    struct Emergency(Uart);
    impl Write for Emergency {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0.write_str(s);
            fbcon::write_str(s);
            Ok(())
        }
    }
    let _ = Emergency(Uart::new(UART_A)).write_fmt(args);
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

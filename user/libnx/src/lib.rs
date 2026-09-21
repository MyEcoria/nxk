//! libnx : bibliotheque systeme des programmes nxk.
//!
//! L'equivalent de la libc : des fonctions ordinaires au-dessus des appels
//! systeme (memoire, threads, temps) et des messages aux serveurs (console,
//! gestionnaire de processus). Un programme se reduit a :
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//! use libnx::println;
//! libnx::entry!(main);
//! fn main() -> i32 { println!("bonjour"); 0 }
//! ```

#![no_std]

extern crate alloc;

pub use nxk_abi as abi;

pub mod bootfs;
pub mod console;
pub mod drv;
pub mod env;
pub mod fs;
pub mod i2c;
pub mod ipc;
pub mod mem;
pub mod net;
pub mod ns;
pub mod process;
pub mod rt;
pub mod sync;
pub mod sys;
pub mod thread;
pub mod time;
pub mod tls;

/// Declare la fonction principale du programme.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "Rust" fn __nx_main() -> i32 {
            $main()
        }
    };
}

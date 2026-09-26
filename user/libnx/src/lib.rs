//! libnx: system library for nxk programs (syscalls + server IPC).
//! See `entry!` + `println!` for a minimal program.

#![no_std]

extern crate alloc;

pub use nxk_abi as abi;

pub mod bootfs;
pub mod clk;
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
pub mod seqpage;
pub mod sync;
pub mod sys;
pub mod thread;
pub mod time;
pub mod tls;

/// Declare the program's main function.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        pub extern "Rust" fn __nx_main() -> i32 {
            $main()
        }
    };
}

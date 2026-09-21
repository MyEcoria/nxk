//! Demarrage d'un programme et gestion des paniques.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

use nxk_abi::{slots, startup};

use crate::{console, process, sys};

core::arch::global_asm!(
    r#"
.section .text._start, "ax"
.globl _start
.type _start, @function
_start:
    mov     x29, #0
    mov     x30, #0
    bl      nx_rt_start
    brk     #0
"#
);

static FLAGS: AtomicU64 = AtomicU64::new(0);
static ARG: AtomicU64 = AtomicU64::new(0);
static BLOCK: AtomicU64 = AtomicU64::new(0);

extern "Rust" {
    fn __nx_main() -> i32;
}

/// x0 = drapeaux de demarrage, x1 = argument libre (pose par le lanceur),
/// x2 = bloc de demarrage (arguments, environnement, en-tetes de programme).
#[no_mangle]
extern "C" fn nx_rt_start(flags: u64, arg: u64, block: u64) -> ! {
    FLAGS.store(flags, Ordering::Relaxed);
    ARG.store(arg, Ordering::Relaxed);
    BLOCK.store(block, Ordering::Relaxed);
    crate::tls::init();
    if flags & startup::HAS_CONSOLE != 0 {
        console::set(slots::CONSOLE);
    }
    if flags & startup::HAS_PM != 0 {
        process::set_pm(slots::PM);
    }
    let code = unsafe { __nx_main() };
    sys::process_exit(code as i64)
}

pub fn startup_flags() -> u64 {
    FLAGS.load(Ordering::Relaxed)
}

pub fn startup_arg() -> u64 {
    ARG.load(Ordering::Relaxed)
}

pub fn startup_block() -> u64 {
    BLOCK.load(Ordering::Relaxed)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::println!("panique : {}", info);
    sys::process_exit(101)
}

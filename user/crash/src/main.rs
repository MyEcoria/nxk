//! Isolation test: this program writes into kernel-reserved memory.
//! It must be killed, and nothing else affected.

#![no_std]
#![no_main]

use libnx::println;

libnx::entry!(main);

fn main() -> i32 {
    println!("crash: writing into kernel memory (0x80000000)...");
    unsafe { core::ptr::write_volatile(0x8000_0000 as *mut u64, 0xDEAD) };
    println!("crash: FAILED, the write went through");
    1
}

//! First program: checks EL0, console, heap and time.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnx::{println, sys, time};

libnx::entry!(main);

fn main() -> i32 {
    println!("Hello from EL0! (core {})", sys::getcpu());
    let v: Vec<u64> = (1..=100).collect();
    let mut s = String::new();
    for w in ["heap", "vectors", "strings"] {
        s.push_str(w);
        s.push(' ');
    }
    println!(
        "  allocations: {}-> sum 1..100 = {}",
        s,
        v.iter().sum::<u64>()
    );
    println!(
        "  syscall-free clock: {} ms since boot",
        time::now_ns() / 1_000_000
    );
    0
}

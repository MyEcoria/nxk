//! Premier programme : verifie EL0, la console, le tas et le temps.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnx::{println, sys, time};

libnx::entry!(main);

fn main() -> i32 {
    println!("Bonjour depuis EL0 ! (coeur {})", sys::getcpu());
    let v: Vec<u64> = (1..=100).collect();
    let mut s = String::new();
    for w in ["tas", "vecteurs", "chaines"] {
        s.push_str(w);
        s.push(' ');
    }
    println!("  allocations : {}-> somme 1..100 = {}", s, v.iter().sum::<u64>());
    println!("  horloge sans appel systeme : {} ms depuis le demarrage", time::now_ns() / 1_000_000);
    0
}

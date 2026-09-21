//! Test du FP paresseux : 8 threads flottants, 2 par coeur, preemptes en
//! plein calcul. Chaque resultat doit etre identique au bit pres a celui
//! calcule seul, sans concurrence.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use libnx::{println, thread};

libnx::entry!(main);

fn work(k: u64) -> f64 {
    let mut s = 0.0f64;
    let mut x = 1.0f64 + k as f64 * 0.001;
    for i in 1..200_000u64 {
        let fi = i as f64;
        s += x / (fi * fi + k as f64);
        x = x * 1.000_001 - 0.000_000_5;
    }
    s
}

fn main() -> i32 {
    let expected: Vec<u64> = (0..8).map(|k| work(k).to_bits()).collect();
    let fails = Arc::new(AtomicU32::new(0));
    let mut hs = Vec::new();
    for k in 0..8u64 {
        let f = fails.clone();
        let want = expected[k as usize];
        hs.push(
            thread::spawn_on(Some((k % 4) as u8), 0, move || {
                for _ in 0..4 {
                    if work(k).to_bits() != want {
                        f.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
            .expect("thread"),
        );
    }
    for h in hs {
        h.join();
    }
    let n = fails.load(Ordering::Relaxed);
    println!("fptest : 8 threads x 4 passes, 2 threads par coeur : {}", if n == 0 { "OK" } else { "ECHEC" });
    if n == 0 {
        0
    } else {
        1
    }
}

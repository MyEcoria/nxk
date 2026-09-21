//! Test d'isolation : ce programme ecrit dans une zone reservee au noyau.
//! Il doit etre tue, et rien d'autre ne doit etre affecte.

#![no_std]
#![no_main]

use libnx::println;

libnx::entry!(main);

fn main() -> i32 {
    println!("crash : ecriture dans la memoire du noyau (0x80000000)...");
    unsafe { core::ptr::write_volatile(0x8000_0000 as *mut u64, 0xDEAD) };
    println!("crash : ECHEC, l'ecriture est passee");
    1
}

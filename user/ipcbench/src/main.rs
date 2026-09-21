//! Mesures : appel systeme nul, aller-retour IPC (meme coeur / coeurs
//! differents), debit IPC par messages de 4 KiB.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::ipc::{self, Msg};
use libnx::{println, sys, thread, time};

libnx::entry!(main);

const N: u64 = 20_000;

fn main() -> i32 {
    let me = sys::getcpu() as u8;
    println!("ipcbench : thread principal sur le coeur {}", me);

    let t0 = time::ticks();
    for _ in 0..N * 5 {
        sys::nop();
    }
    println!("  appel systeme nul            : {:>6} ns", time::ticks_to_ns(time::ticks() - t0) / (N * 5));

    let same = round_trip(me);
    println!("  IPC aller-retour, meme coeur : {:>6} ns", same);
    let other = round_trip((me + 1) % 4);
    println!("  IPC aller-retour, 2 coeurs   : {:>6} ns", other);
    let mbs = bulk(me);
    println!("  debit IPC (messages 4 KiB)   : {:>6} Mo/s", mbs);
    0
}

/// Serveur d'echo sur le coeur `cpu`, client sur le thread principal.
fn round_trip(cpu: u8) -> u64 {
    let ep = sys::endpoint_create().expect("endpoint");
    let server = thread::spawn_on(Some(cpu), 0, move || {
        let mut rx = match ipc::recv(ep) {
            Ok(r) => r,
            Err(_) => return,
        };
        loop {
            let mut r = Msg::new(0, 1, 0);
            r.mr[0] = rx.msg.mr[0] + 1;
            rx = match ipc::reply_recv(ep, &r) {
                Ok(x) => x,
                Err(_) => return,
            };
        }
    })
    .expect("thread serveur");

    let mut m = Msg::new(1, 1, 0);
    for i in 0..1000 {
        m.mr[0] = i;
        ipc::call(ep, &m).expect("call");
    }
    let t0 = time::ticks();
    for i in 0..N {
        m.mr[0] = i;
        let r = ipc::call(ep, &m).expect("call");
        if r.mr[0] != i + 1 {
            println!("  reponse incorrecte !");
        }
    }
    let ns = time::ticks_to_ns(time::ticks() - t0) / N;
    let _ = sys::handle_close(ep); // le serveur recoit EPIPE et se termine
    server.join();
    ns
}

/// Messages de 4 KiB (UTCB) vers un serveur qui repond a vide.
fn bulk(cpu: u8) -> u64 {
    let ep = sys::endpoint_create().expect("endpoint");
    let server = thread::spawn_on(Some(cpu), 0, move || {
        let mut rx = match ipc::recv(ep) {
            Ok(r) => r,
            Err(_) => return,
        };
        loop {
            let r = Msg::new(0, 0, 0);
            core::hint::black_box(rx.msg.mr[0]);
            rx = match ipc::reply_recv(ep, &r) {
                Ok(x) => x,
                Err(_) => return,
            };
        }
    })
    .expect("thread serveur");

    let data = [0x5Au8; 4000];
    let iters = N / 4;
    let t0 = time::ticks();
    for _ in 0..iters {
        let m = ipc::pack(1, &data);
        ipc::call(ep, &m).expect("call");
    }
    let ns = time::ticks_to_ns(time::ticks() - t0).max(1);
    let _ = sys::handle_close(ep);
    server.join();
    iters * data.len() as u64 * 1000 / ns
}

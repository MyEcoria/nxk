//! Benchmarks: null syscall, IPC round trip (same core / other cores),
//! IPC throughput with 4 KiB messages.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::ipc::{self, Msg};
use libnx::{println, sys, thread, time};

libnx::entry!(main);

const N: u64 = 20_000;

fn main() -> i32 {
    let me = sys::getcpu() as u8;
    println!("ipcbench: main thread on core {}", me);

    let t0 = time::ticks();
    for _ in 0..N * 5 {
        sys::nop();
    }
    println!(
        "  null syscall               : {:>6} ns",
        time::ticks_to_ns(time::ticks() - t0) / (N * 5)
    );

    let same = round_trip(me);
    println!("  IPC round trip, same core  : {:>6} ns", same);
    let other = round_trip((me + 1) % 4);
    println!("  IPC round trip, 2 cores    : {:>6} ns", other);
    let mbs = bulk(me);
    println!("  IPC throughput (4 KiB msgs)  : {:>6} MB/s", mbs);
    0
}

/// Echo server on core `cpu`, client on the main thread.
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
    .expect("server thread");

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
            println!("  bad reply!");
        }
    }
    let ns = time::ticks_to_ns(time::ticks() - t0) / N;
    let _ = sys::handle_close(ep); // the server gets EPIPE and exits
    server.join();
    ns
}

/// 4 KiB messages (UTCB) to a server replying empty.
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
    .expect("server thread");

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

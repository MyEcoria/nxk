//! nxk benchmarks, aligned with `lbench` (Linux side): same loads, same
//! clock (CNTVCT_EL0), same `RESULT <key> <value> <unit>` keys.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::abi::info::{self, MemInfo, ProcInfo};
use libnx::ipc::{self, Msg};
use libnx::{println, process, sys, thread, time};

libnx::entry!(main);

const N: u64 = 20_000;

fn result(key: &str, v: u64, unit: &str) {
    println!("RESULT {} {} {}", key, v, unit);
}

fn main() -> i32 {
    // Pinned: "same core" and "cross core" are measured from `me`.
    let me = sys::getcpu() as u8;
    let _ = thread::set_current_affinity(Some(me));
    result("null_syscall_ns", null_syscall(), "ns");
    result("ipc_same_core_ns", round_trip(me, me, 8), "ns");
    result("ipc_cross_core_ns", round_trip(me, (me + 1) % 4, 8), "ns");
    let iters = N / 4;
    let dt = bulk(me, iters).max(1);
    result("ipc_bulk_4k_mbs", iters * 4000 * 1000 / dt, "MB/s");
    result("thread_create_join_us", thread_create() / 1000, "us");
    result("spawn_wait_us", spawn() / 1000, "us");
    result("compute_ms", compute(), "ms");
    memory();
    println!("RESULT END");
    0
}

fn null_syscall() -> u64 {
    let t0 = time::ticks();
    for _ in 0..N * 5 {
        sys::nop();
    }
    time::ticks_to_ns(time::ticks() - t0) / (N * 5)
}

fn round_trip(client: u8, server: u8, _bytes: usize) -> u64 {
    let _ = client;
    let ep = sys::endpoint_create().expect("endpoint");
    let s = thread::spawn_on(Some(server), 0, move || {
        let mut rx = match ipc::recv(ep) {
            Ok(r) => r,
            Err(_) => return,
        };
        loop {
            let mut r = Msg::new(0, 1, 0);
            r.mr[0] = rx.msg.mr[0];
            rx = match ipc::reply_recv(ep, &r) {
                Ok(x) => x,
                Err(_) => return,
            };
        }
    })
    .expect("thread");
    let mut m = Msg::new(1, 1, 0);
    for i in 0..1000 {
        m.mr[0] = i;
        let _ = ipc::call(ep, &m);
    }
    let t0 = time::ticks();
    for i in 0..N {
        m.mr[0] = i;
        let _ = ipc::call(ep, &m);
    }
    let ns = time::ticks_to_ns(time::ticks() - t0) / N;
    let _ = sys::handle_close(ep);
    s.join();
    ns
}

/// 4000 bytes out (UTCB), 8 bytes back: same shape as `lbench` pipes.
fn bulk(server: u8, iters: u64) -> u64 {
    let ep = sys::endpoint_create().expect("endpoint");
    let s = thread::spawn_on(Some(server), 0, move || {
        let mut rx = match ipc::recv(ep) {
            Ok(r) => r,
            Err(_) => return,
        };
        loop {
            let mut r = Msg::new(0, 1, 0);
            r.mr[0] = rx.msg.mr[0];
            rx = match ipc::reply_recv(ep, &r) {
                Ok(x) => x,
                Err(_) => return,
            };
        }
    })
    .expect("thread");
    let data = [0x5Au8; 4000];
    for _ in 0..1000.min(iters) {
        let _ = ipc::call(ep, &ipc::pack(1, &data));
    }
    let t0 = time::ticks();
    for _ in 0..iters {
        let _ = ipc::call(ep, &ipc::pack(1, &data));
    }
    let ns = time::ticks_to_ns(time::ticks() - t0);
    let _ = sys::handle_close(ep);
    s.join();
    ns
}

fn thread_create() -> u64 {
    let n = 500;
    let t0 = time::ticks();
    for _ in 0..n {
        thread::spawn(|| {}).expect("thread").join();
    }
    time::ticks_to_ns(time::ticks() - t0) / n
}

/// Spawn a program (including ELF load) and wait for it.
fn spawn() -> u64 {
    let n = 200;
    let t0 = time::ticks();
    for _ in 0..n {
        let h = process::spawn("true").expect("spawn");
        process::wait(h);
        let _ = sys::handle_close(h);
    }
    time::ticks_to_ns(time::ticks() - t0) / n
}

/// Pure integer compute, identical to `lbench`: kernel-independent witness.
fn compute() -> u64 {
    let t0 = time::ticks();
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..20_000_000u64 {
        acc ^= acc >> 12;
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
    }
    core::hint::black_box(acc);
    time::ticks_to_ns(time::ticks() - t0) / 1_000_000
}

fn as_bytes_mut<T>(v: &mut [T]) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, core::mem::size_of_val(v)) }
}

fn memory() {
    let mut m = [MemInfo::default()];
    if sys::sysinfo(info::MEMORY, as_bytes_mut(&mut m)).is_ok() {
        let m = m[0];
        result("meminfo_memtotal", m.total_pages * 4, "kB");
        result("meminfo_memfree", m.free_pages * 4, "kB");
        result("mem_used_kb", (m.total_pages - m.free_pages) * 4, "kB");
        result("mem_kernel_kb", m.kernel_pages * 4, "kB");
    }
    let mut procs = [ProcInfo::default(); 64];
    let n = sys::sysinfo(info::PROCESSES, as_bytes_mut(&mut procs)).unwrap_or(0) as usize;
    for p in &procs[..n.min(64)] {
        if p.name.starts_with(b"bench") {
            result("proc_rss_kb", p.pages * 4, "kB");
        }
    }
}

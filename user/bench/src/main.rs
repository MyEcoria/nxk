//! nxk benchmarks, aligned with `lbench` (Linux side): same loads, same
//! clock (CNTVCT_EL0), same `RESULT <key> <value> <unit>` keys.
//! `bench burn [SECONDS]` loads every core at 100% instead.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::aarch64::{float32x4_t, vdupq_n_f32, vfmaq_f32};
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libnx::abi::info::{self, MemInfo, ProcInfo};
use libnx::abi::proto::fan;
use libnx::abi::thread::MAX_PRIO;
use libnx::ipc::{self, Msg};
use libnx::{env, ns, println, process, sys, thread, time};

libnx::entry!(main);

const N: u64 = 20_000;

fn result(key: &str, v: u64, unit: &str) {
    println!("RESULT {} {} {}", key, v, unit);
}

fn main() -> i32 {
    let args: Vec<&str> = env::args().collect();
    match args.get(1).copied() {
        None => {}
        Some("burn") => match args.get(2).map_or(Ok(60), |s| s.parse()) {
            Ok(secs) => return burn(secs),
            Err(_) => return usage(),
        },
        Some(_) => return usage(),
    }
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

fn usage() -> i32 {
    println!("usage: bench [burn [SECONDS]]");
    1
}

const CORES: usize = 4;
/// Burn stops early at this SoC temperature (millidegrees C).
const BURN_HOT_MC: i64 = 85_000;
/// Independent FMA chains per core: enough to fill both A57 FP pipes.
const CHAINS: usize = 12;
const CHUNK: u64 = 1 << 20;
static STOP: AtomicBool = AtomicBool::new(false);
static ITERS: [AtomicU64; CORES] = [const { AtomicU64::new(0) }; CORES];

/// NEON multiply-adds on one core until `STOP`. Accumulators saturate
/// near 1.0: no overflow, no denormal slow path.
fn spin(core: usize) {
    let (x, y) = (black_box(1e-4f32), black_box(1e-4f32));
    // SAFETY: NEON is part of the aarch64 base target.
    let (x, y) = unsafe { (vdupq_n_f32(x), vdupq_n_f32(y)) };
    let mut acc: [float32x4_t; CHAINS] = [x; CHAINS];
    while !STOP.load(Ordering::Relaxed) {
        for _ in 0..CHUNK {
            for a in acc.iter_mut() {
                *a = unsafe { vfmaq_f32(*a, x, y) };
            }
        }
        ITERS[core].fetch_add(CHUNK, Ordering::Relaxed);
    }
    black_box(acc);
}

/// Every core at 100% for `secs` s, at the lowest priority so services
/// and the shell still run; SoC temperature and fan every 10 s.
fn burn(secs: u64) -> i32 {
    let fan = ns::Service::optional("fan");
    println!("burn: {} cores for {} s", CORES, secs);
    let workers: Vec<_> = (0..CORES)
        .map(|c| thread::spawn_on(Some(c as u8), MAX_PRIO, move || spin(c)).expect("thread"))
        .collect();
    let t0 = time::ticks();
    let mut hottest = None;
    let mut hot = false;
    let mut shown = 0;
    loop {
        time::sleep_ms(1000);
        let s = time::ticks_to_ns(time::ticks() - t0) / 1_000_000_000;
        let st = fan.call(&Msg::new(fan::STATUS, 0, 0)).ok();
        let temp = st.as_ref().map(|m| m.mr[2]).filter(|&t| t != fan::NO_TEMP);
        if let Some(t) = temp {
            hottest = Some(hottest.unwrap_or(i64::MIN).max(t as i64));
            hot = t as i64 >= BURN_HOT_MC;
        }
        if hot || s >= secs {
            break;
        }
        if s / 10 > shown {
            shown = s / 10;
            match (&st, temp) {
                (Some(m), Some(t)) => {
                    let rpm = match m.mr[3] {
                        fan::NO_RPM => 0,
                        r => r,
                    };
                    println!(
                        "burn: {} s, SoC {} C, fan {}% {} rpm",
                        s,
                        milli(t as i64),
                        m.mr[0],
                        rpm
                    );
                }
                _ => println!("burn: {} s", s),
            }
        }
    }
    STOP.store(true, Ordering::Relaxed);
    for w in workers {
        w.join();
    }
    let ns = time::ticks_to_ns(time::ticks() - t0).max(1);
    let iters: u64 = ITERS.iter().map(|i| i.load(Ordering::Relaxed)).sum();
    // Per iteration: CHAINS vector FMAs of 4 lanes, 2 flops each.
    let mflops = iters * CHAINS as u64 * 8 * 1000 / ns;
    let t = hottest.map_or("?".into(), |t| alloc::format!("{} C", milli(t)));
    if hot {
        println!("burn: stopped, SoC at {} C or more", milli(BURN_HOT_MC));
    }
    println!(
        "burn: {} s on {} cores, {}.{} GFLOP/s, SoC max {}",
        ns / 1_000_000_000,
        CORES,
        mflops / 1000,
        mflops % 1000 / 100,
        t
    );
    result("burn_mflops", mflops, "MFLOP/s");
    i32::from(hot)
}

/// Millidegrees as "NN.N".
fn milli(t: i64) -> alloc::string::String {
    alloc::format!("{}.{}", t / 1000, (t % 1000).abs() / 100)
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

//! ktest: check kernel mechanisms from user space.
//! Each test prints `[ok]`/`[FAILED]`; last line is `RESULT ktest_failed N`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use libnx::abi::err::{self, EACCES, EAGAIN, EBADF, ENOMEM, ESRCH, ETIMEDOUT};
use libnx::abi::info::{self, CpuInfo, MemInfo};
use libnx::abi::prot::{EXEC, READ, WRITE};
use libnx::abi::rights;
use libnx::abi::shm::{CONTIGUOUS, UNCACHED};
use libnx::abi::timeout::POLL;
use libnx::ipc::{self, Msg};
use libnx::mem::{self, SharedMemory};
use libnx::process::{self, Strings};
use libnx::sync::{Condvar, Mutex};
use libnx::{env, println, sys, thread, time};

libnx::entry!(main);

type R = Result<(), String>;

const MS: u64 = 1_000_000;

fn main() -> i32 {
    let args: Vec<&str> = env::args().collect();
    if args.get(1) == Some(&"--child") {
        return child(&args[2..]);
    }
    type Test = (&'static str, fn() -> R);
    let tests: &[Test] = &[
        ("getrandom", t_random),
        ("kernel log: private, no UART wait", t_klog),
        ("precise sleep", t_sleep),
        ("bounded notify", t_notify_timeout),
        ("bounded IPC and tokens", t_ipc_timeouts),
        ("futex", t_futex),
        ("mutex under contention", t_mutex),
        ("condition variable", t_condvar),
        ("wall clock", t_realtime),
        ("growable handle tables", t_many_handles),
        ("many threads", t_many_threads),
        ("affinity and migration", t_affinity),
        ("priority preemption", t_priority),
        ("tickless round robin", t_round_robin),
        ("load balancing", t_balance),
        ("tickless idle cores", t_tickless),
        ("mem_protect", t_protect),
        ("long zeroing lets other cores in", t_breathe),
        ("concurrent first touch, fixed mapping", t_first_touch),
        ("pipe: end of stream, write with no reader", t_pipe),
        ("shared memory", t_shm),
        ("contiguous DMA buffer and rights", t_shm_dma),
        ("revocation", t_revoke),
        ("args and environment", t_args),
        ("ASLR (static-PIE)", t_aslr),
        ("just-in-time code (JIT)", t_jit),
        ("faults: RO write, no access, W^X", t_faults),
        ("debug: BRK, registers, single step, memory", t_debug),
        ("debug: remote stop, KILL", t_debug_stop),
    ];
    let mut failed = 0;
    for (name, f) in tests {
        match f() {
            Ok(()) => println!("[ok]     {}", name),
            Err(e) => {
                failed += 1;
                println!("[FAILED] {}: {}", name, e);
            }
        }
    }
    println!(
        "ktest: {}/{} tests passed",
        tests.len() - failed,
        tests.len()
    );
    println!("RESULT ktest_failed {} tests", failed);
    failed as i32
}

fn expect_err<T: core::fmt::Debug>(what: &str, r: Result<T, i64>, e: i64) -> R {
    match r {
        Err(x) if x == -e => Ok(()),
        other => Err(format!(
            "{}: expected {}, got {:?}",
            what,
            err::name(e),
            other.map_err(err::name)
        )),
    }
}

fn t_klog() -> R {
    // Only the console holds the log capability.
    let mut b = [0u8; 16];
    expect_err(
        "klog_read without the capability",
        sys::klog_read(libnx::abi::slots::INIT_KLOG, &mut b),
        EBADF,
    )?;
    // A log line costs a copy into the kernel's ring, never a wait on
    // the UART (87 us per byte at 115200 baud: 5.6 ms for this line).
    // Best of 3: an emulator host can stall any single call.
    let line = b"ktest: kernel log line, copied to a ring, no UART wait .........\n";
    let best = (0..3)
        .map(|_| {
            let t0 = time::now_ns();
            sys::debug_write(line);
            time::now_ns() - t0
        })
        .min()
        .unwrap_or(0);
    if best > 2 * MS {
        return Err(format!(
            "debug_write of {} bytes took {} us",
            line.len(),
            best / 1000
        ));
    }
    Ok(())
}

fn t_random() -> R {
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    sys::getrandom(&mut a);
    sys::getrandom(&mut b);
    if a == b {
        return Err("two identical draws".into());
    }
    if a.iter().all(|&x| x == 0) {
        return Err("all zeros".into());
    }
    // 64 KiB at once: every byte value must appear.
    let mut big = alloc::vec![0u8; 65536];
    sys::getrandom(&mut big);
    let mut seen = [false; 256];
    for &x in &big {
        seen[x as usize] = true;
    }
    if seen.iter().any(|&s| !s) {
        return Err("missing byte values over 64 KiB".into());
    }
    Ok(())
}

fn t_sleep() -> R {
    for d in [1u64, 5, 12] {
        let t0 = time::now_ns();
        time::sleep_ms(d);
        let dt = time::now_ns() - t0;
        if dt < d * MS {
            return Err(format!("sleep {} ms: early wakeup ({} us)", d, dt / 1000));
        }
        if dt > d * MS + 20 * MS {
            return Err(format!("sleep {} ms: late wakeup ({} us)", d, dt / 1000));
        }
    }
    Ok(())
}

fn t_notify_timeout() -> R {
    let n = sys::notify_create().map_err(|e| format!("notify_create {}", e))?;
    let t0 = time::now_ns();
    expect_err("3 ms wait", sys::notify_wait_timeout(n, 3 * MS), ETIMEDOUT)?;
    if time::now_ns() - t0 < 3 * MS {
        return Err("deadline not honored".into());
    }
    expect_err("no-wait poll", sys::notify_wait_timeout(n, POLL), ETIMEDOUT)?;
    sys::notify_signal(n, 5).map_err(|e| format!("signal {}", e))?;
    match sys::notify_wait_timeout(n, MS) {
        Ok(5) => {}
        r => return Err(format!("expected bits 5, got {:?}", r)),
    }
    let _ = sys::handle_close(n);
    Ok(())
}

fn t_ipc_timeouts() -> R {
    let ep = sys::endpoint_create().map_err(|e| format!("endpoint {}", e))?;
    expect_err("2 ms recv", ipc::recv_timeout(ep, 2 * MS), ETIMEDOUT)?;
    expect_err("no-wait recv", ipc::recv_timeout(ep, POLL), ETIMEDOUT)?;
    expect_err(
        "call with no server",
        ipc::call_timeout(ep, &Msg::new(1, 0, 0), POLL),
        ETIMEDOUT,
    )?;
    expect_err(
        "2 ms call with no server",
        ipc::call_timeout(ep, &Msg::new(1, 0, 0), 2 * MS),
        ETIMEDOUT,
    )?;

    // Server holding the request without replying: the client gives up, and
    // neither the implicit reply nor the stale token may reach it later.
    // Stages: 1 server about to recv, 2 request held, 3 client gave up.
    const LONG: u64 = 2000 * MS;
    let stage = Arc::new(AtomicU32::new(0));
    let report = Arc::new(AtomicU64::new(0));
    let (st, rp) = (stage.clone(), report.clone());
    let server = thread::spawn(move || {
        let code = |r: Result<(), i64>| match r {
            Ok(()) => 0u64,
            Err(e) => (-e) as u64,
        };
        // Report word: step << 48; steps 1-2 = that recv failed (errno below).
        st.store(1, Ordering::Release);
        let rx = match ipc::recv_timeout(ep, LONG) {
            Ok(r) => r,
            Err(e) => return rp.store(1 << 48 | code(Err(e)), Ordering::Release),
        };
        st.store(2, Ordering::Release);
        while st.load(Ordering::Acquire) != 3 {
            time::sleep_ms(1);
        }
        let late = ipc::reply(&Msg::new(7, 0, 0));
        let rx2 = match ipc::recv_timeout(ep, LONG) {
            Ok(r) => r,
            Err(e) => return rp.store(2 << 48 | code(Err(e)), Ordering::Release),
        };
        let stale = ipc::reply_to(rx.token, &Msg::new(8, 0, 0));
        let good = ipc::reply_to(rx2.token, &Msg::new(9, 0, 0));
        rp.store(
            code(late) | code(stale) << 16 | code(good) << 32 | 3 << 48,
            Ordering::Release,
        );
    })
    .map_err(|e| format!("thread {}", e))?;

    // A call that times out before the server sits in recv proves nothing:
    // retry until the server reports holding one of them.
    let start = time::now_ns();
    let gave_up = loop {
        match stage.load(Ordering::Acquire) {
            0 => time::sleep_ms(1),
            1 => {
                let t0 = time::now_ns();
                let r = ipc::call_timeout(ep, &Msg::new(1, 0, 0), 5 * MS);
                let dt = time::now_ns() - t0;
                if let Err(e) =
                    expect_err("5 ms call with no reply", r.map(|m| m.label()), ETIMEDOUT)
                {
                    break Err(e);
                }
                if dt < 5 * MS {
                    break Err(format!("call gave up too early ({} us)", dt / 1000));
                }
            }
            _ => break Ok(()),
        }
        if time::now_ns() - start > LONG {
            break Err("server never took a request".into());
        }
    };
    stage.store(3, Ordering::Release);
    let second = ipc::call_timeout(ep, &Msg::new(2, 0, 0), LONG);
    server.join();
    let _ = sys::handle_close(ep);

    gave_up?;
    let r = report.load(Ordering::Acquire);
    if r >> 48 != 3 {
        return Err(format!(
            "server recv #{}: {}",
            r >> 48,
            err::name((r & 0xFFFF) as i64)
        ));
    }
    match second {
        Ok(m) if m.label() == 9 => {}
        r => {
            return Err(format!(
                "second call: expected label 9, got {:?}",
                r.map(|m| m.label())
            ))
        }
    }
    let r = report.load(Ordering::Acquire);
    let (late, stale, good) = (r & 0xFFFF, (r >> 16) & 0xFFFF, (r >> 32) & 0xFFFF);
    if late != ESRCH as u64 || stale != ESRCH as u64 || good != 0 {
        return Err(format!(
            "replies: late {}, stale token {}, good {} (expected ESRCH, ESRCH, 0)",
            late, stale, good
        ));
    }
    Ok(())
}

fn t_futex() -> R {
    let w = AtomicU32::new(1);
    expect_err("different value", sys::futex_wait(&w, 0, 0), EAGAIN)?;
    expect_err("2 ms wait", sys::futex_wait(&w, 1, 2 * MS), ETIMEDOUT)?;
    if sys::futex_wake(&w, 10) != 0 {
        return Err("wake with no waiter: nonzero count".into());
    }
    // One thread waits, we wake it.
    let flag = Arc::new(AtomicU32::new(0));
    let f = flag.clone();
    let h = thread::spawn(move || {
        while f.load(Ordering::Acquire) == 0 {
            let _ = sys::futex_wait(&f, 0, 0);
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    time::sleep_ms(3);
    flag.store(1, Ordering::Release);
    let woken = sys::futex_wake(&flag, 1);
    h.join();
    if woken != 1 {
        return Err(format!("expected 1 waiter, {} woken", woken));
    }
    Ok(())
}

fn t_mutex() -> R {
    let m = Arc::new(Mutex::new(0u64));
    let mut hs = Vec::new();
    for _ in 0..4 {
        let m = m.clone();
        hs.push(
            thread::spawn(move || {
                for _ in 0..20_000 {
                    *m.lock() += 1;
                }
            })
            .map_err(|e| format!("thread {}", e))?,
        );
    }
    for h in hs {
        h.join();
    }
    let v = *m.lock();
    if v != 80_000 {
        return Err(format!("counter {} instead of 80000", v));
    }
    Ok(())
}

fn t_condvar() -> R {
    let pair = Arc::new((Mutex::new(Vec::<u32>::new()), Condvar::new()));
    let p2 = pair.clone();
    let producer = thread::spawn(move || {
        for i in 1..=100 {
            p2.0.lock().push(i);
            p2.1.notify_one();
            if i % 10 == 0 {
                time::sleep_us(200);
            }
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    let mut sum = 0u32;
    let mut got = 0;
    let mut g = pair.0.lock();
    while got < 100 {
        while g.is_empty() {
            g = pair.1.wait(g);
        }
        for v in g.drain(..) {
            sum += v;
            got += 1;
        }
    }
    drop(g);
    producer.join();
    if sum != 5050 {
        return Err(format!("sum {} instead of 5050", sum));
    }
    let (_g, timed_out) = pair.1.wait_timeout(pair.0.lock(), 2 * MS);
    if !timed_out {
        return Err("wait_timeout with no signal did not expire".into());
    }
    Ok(())
}

fn t_realtime() -> R {
    let mono = sys::clock_ns();
    let real = sys::realtime_ns();
    if real < mono.saturating_sub(MS) {
        return Err(format!("REALTIME {} < MONOTONIC {}", real, mono));
    }
    expect_err(
        "set without capability",
        sys::clock_set_realtime(0, 1),
        EBADF,
    )
}

fn t_many_handles() -> R {
    let mut hs = Vec::new();
    for _ in 0..600 {
        hs.push(
            sys::notify_create().map_err(|e| format!("notify {}: {}", hs.len(), err::name(e)))?,
        );
    }
    for (k, &h) in hs.iter().enumerate() {
        sys::notify_signal(h, k as u64 + 1).map_err(|e| format!("signal {}", err::name(e)))?;
    }
    for (k, &h) in hs.iter().enumerate() {
        if sys::notify_wait_timeout(h, POLL) != Ok(k as u64 + 1) {
            return Err(format!("handle {}: wrong object", k));
        }
    }
    for h in hs {
        sys::handle_close(h).map_err(|e| format!("close {}", err::name(e)))?;
    }
    Ok(())
}

fn t_many_threads() -> R {
    let count = Arc::new(AtomicU32::new(0));
    let mut hs = Vec::new();
    for _ in 0..150 {
        let c = count.clone();
        hs.push(
            thread::spawn(move || {
                time::sleep_ms(2);
                c.fetch_add(1, Ordering::Relaxed);
            })
            .map_err(|e| format!("thread {}: {}", hs.len(), err::name(e)))?,
        );
    }
    for h in hs {
        h.join();
    }
    let n = count.load(Ordering::Relaxed);
    if n != 150 {
        return Err(format!("{} threads finished out of 150", n));
    }
    Ok(())
}

fn t_affinity() -> R {
    let out = Arc::new(AtomicU64::new(0));
    let o = out.clone();
    let h = thread::spawn_on(Some(1), 0, move || {
        let a = sys::getcpu() as u64;
        let _ = thread::set_current_affinity(Some(2));
        let b = sys::getcpu() as u64;
        let _ = thread::set_current_affinity(Some(3));
        let c = sys::getcpu() as u64;
        let _ = thread::set_current_affinity(None);
        o.store(a | b << 8 | c << 16 | 1 << 24, Ordering::Release);
    })
    .map_err(|e| format!("thread {}", e))?;
    h.join();
    let v = out.load(Ordering::Acquire);
    let (a, b, c) = (v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF);
    if (a, b, c) != (1, 2, 3) {
        return Err(format!(
            "successive cores {} {} {} instead of 1 2 3",
            a, b, c
        ));
    }

    // Externally forced migration of a spinning thread.
    let stop = Arc::new(AtomicBool::new(false));
    let where_ = Arc::new(AtomicU32::new(99));
    let (s, w) = (stop.clone(), where_.clone());
    let h = thread::spawn_on(Some(1), 0, move || {
        while !s.load(Ordering::Acquire) {
            w.store(sys::getcpu(), Ordering::Release);
            core::hint::spin_loop();
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    time::sleep_ms(5);
    let before = where_.load(Ordering::Acquire);
    h.set_affinity(Some(2))
        .map_err(|e| format!("set_affinity {}", err::name(e)))?;
    let t0 = time::now_ns();
    while where_.load(Ordering::Acquire) != 2 && time::now_ns() - t0 < 200 * MS {
        time::sleep_ms(1);
    }
    let after = where_.load(Ordering::Acquire);
    stop.store(true, Ordering::Release);
    h.join();
    if before != 1 || after != 2 {
        return Err(format!(
            "thread seen on {} then {} (expected 1 then 2)",
            before, after
        ));
    }
    Ok(())
}

fn t_priority() -> R {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let low = thread::spawn_on(Some(2), 25, move || {
        while !s.load(Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    let worst = Arc::new(AtomicU64::new(0));
    let wr = worst.clone();
    let high = thread::spawn_on(Some(2), 5, move || {
        for _ in 0..10 {
            let t0 = time::now_ns();
            time::sleep_ms(2);
            let late = (time::now_ns() - t0).saturating_sub(2 * MS);
            wr.fetch_max(late, Ordering::Relaxed);
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    high.join();
    stop.store(true, Ordering::Relaxed);
    low.join();
    let w = worst.load(Ordering::Relaxed);
    if w > 5 * MS {
        return Err(format!(
            "max lateness of high-priority thread: {} us",
            w / 1000
        ));
    }
    Ok(())
}

fn t_breathe() -> R {
    // 64 MiB populated then unmapped on core 1 while core 2 makes locked
    // syscalls: the kernel lets its global lock go during long work.
    let probe = |phase: &AtomicU32, worst: &[AtomicU64; 2]| {
        while phase.load(Ordering::Relaxed) < 3 {
            let t0 = time::now_ns();
            let _ = sys::getpid(0);
            let dt = time::now_ns() - t0;
            match phase.load(Ordering::Relaxed) {
                1 => worst[0].fetch_max(dt, Ordering::Relaxed),
                2 => worst[1].fetch_max(dt, Ordering::Relaxed),
                _ => 0,
            };
        }
    };
    let phase = Arc::new(AtomicU32::new(0));
    let worst = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let (p2, w2) = (phase.clone(), worst.clone());
    let prober = thread::spawn_on(Some(2), 0, move || probe(&p2, &w2))
        .map_err(|e| format!("thread {}", e))?;
    let p3 = phase.clone();
    let filler = thread::spawn_on(Some(1), 0, move || {
        let len = 64 << 20;
        time::sleep_ms(5);
        p3.store(1, Ordering::Relaxed);
        let a = sys::mem_map(0, len, READ | WRITE | libnx::abi::prot::POPULATE);
        p3.store(2, Ordering::Relaxed);
        let _ = a.and_then(|a| sys::mem_unmap(a, len));
        p3.store(3, Ordering::Relaxed);
    })
    .map_err(|e| format!("thread {}", e))?;
    filler.join();
    prober.join();
    // One batch of zeroing, not the whole 64 MiB (the emulator zeroes
    // 64 pages in a few ms; before, a core stalled 245 ms on the unmap).
    let w = [
        worst[0].load(Ordering::Relaxed),
        worst[1].load(Ordering::Relaxed),
    ];
    if w[0] > 20 * MS || w[1] > 20 * MS {
        return Err(format!(
            "a syscall waited {} us during the populate, {} us during the unmap",
            w[0] / 1000,
            w[1] / 1000
        ));
    }
    Ok(())
}

fn t_round_robin() -> R {
    let stop = Arc::new(AtomicBool::new(false));
    let counts = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
    let mut hs = Vec::new();
    for k in 0..2 {
        let (s, c) = (stop.clone(), counts.clone());
        hs.push(
            thread::spawn_on(Some(3), 0, move || {
                while !s.load(Ordering::Relaxed) {
                    c[k].fetch_add(1, Ordering::Relaxed);
                }
            })
            .map_err(|e| format!("thread {}", e))?,
        );
    }
    time::sleep_ms(120);
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join();
    }
    let (a, b) = (
        counts[0].load(Ordering::Relaxed),
        counts[1].load(Ordering::Relaxed),
    );
    let total = (a + b).max(1);
    if a * 5 < total || b * 5 < total {
        return Err(format!("unequal share: {} vs {}", a, b));
    }
    Ok(())
}

fn t_balance() -> R {
    // Eight threads born on core 0, then released: they must spread out.
    let go = Arc::new(AtomicU32::new(0));
    let mask = Arc::new(AtomicU32::new(0));
    let mut hs = Vec::new();
    for _ in 0..8 {
        let (g, m) = (go.clone(), mask.clone());
        hs.push(
            thread::spawn_on(Some(0), 0, move || {
                while g.load(Ordering::Acquire) == 0 {
                    let _ = sys::futex_wait(&g, 0, 0);
                }
                let end = time::now_ns() + 60 * MS;
                while time::now_ns() < end {
                    m.fetch_or(1 << sys::getcpu(), Ordering::Relaxed);
                    for _ in 0..1000 {
                        core::hint::spin_loop();
                    }
                }
            })
            .map_err(|e| format!("thread {}", e))?,
        );
    }
    time::sleep_ms(5);
    for h in &hs {
        h.set_affinity(None)
            .map_err(|e| format!("set_affinity {}", err::name(e)))?;
    }
    go.store(1, Ordering::Release);
    sys::futex_wake(&go, u64::MAX);
    for h in hs {
        h.join();
    }
    let m = mask.load(Ordering::Relaxed);
    if m.count_ones() < 3 {
        return Err(format!("cores used: mask {:#b}", m));
    }
    Ok(())
}

fn cpus() -> [CpuInfo; 4] {
    let mut c = [CpuInfo::default(); 4];
    let buf = unsafe {
        core::slice::from_raw_parts_mut(c.as_mut_ptr() as *mut u8, core::mem::size_of_val(&c))
    };
    let _ = sys::sysinfo(info::CPUS, buf);
    c
}

fn t_tickless() -> R {
    let me = sys::getcpu() as usize;
    let worst = |a: &[CpuInfo; 4], b: &[CpuInfo; 4]| {
        (0..4)
            .filter(|&k| k != me && a[k].online != 0)
            .map(|k| b[k].irqs - a[k].irqs)
            .max()
            .unwrap_or(0)
    };
    // Earlier output may still drain to UART-A (a FIFO refill every
    // 1.4 ms on the console's core): wait up to 3 s for quiet cores. A
    // periodic tick never goes quiet, and still fails below.
    for _ in 0..30 {
        let a = cpus();
        time::sleep_ms(100);
        if worst(&a, &cpus()) <= 5 {
            break;
        }
    }
    let a = cpus();
    time::sleep_ms(300);
    let b = cpus();
    let worst = worst(&a, &b);
    // With a 100 Hz tick, an idle core would get 30.
    if worst > 5 {
        let detail: Vec<String> = (0..4)
            .map(|k| {
                format!(
                    "c{} irq+{} cs+{} busy {} us",
                    k,
                    b[k].irqs - a[k].irqs,
                    b[k].switches - a[k].switches,
                    (b[k].busy_ns - a[k].busy_ns) / 1000
                )
            })
            .collect();
        return Err(format!(
            "{} interrupts on an idle core in 300 ms (me: c{}; {})",
            worst,
            me,
            detail.join(", ")
        ));
    }
    Ok(())
}

const PAGE: usize = 4096;

fn t_protect() -> R {
    let p = mem::map(3 * PAGE, READ | WRITE).map_err(|e| format!("map {}", err::name(e)))?;
    unsafe {
        *p = 42;
        *p.add(PAGE) = 43;
    }
    mem::protect(p, PAGE, READ).map_err(|e| format!("read-only {}", err::name(e)))?;
    if unsafe { core::ptr::read_volatile(p) } != 42 {
        return Err("content lost in read-only".into());
    }
    mem::protect(p, PAGE, 0).map_err(|e| format!("no access {}", err::name(e)))?;
    mem::protect(p, PAGE, READ | WRITE).map_err(|e| format!("back to RW {}", err::name(e)))?;
    unsafe {
        if core::ptr::read_volatile(p) != 42 {
            return Err("content lost after no-access".into());
        }
        core::ptr::write_volatile(p, 44);
    }
    expect_err("W+X", mem::protect(p, PAGE, READ | WRITE | EXEC), EACCES)?;
    // Guard page after the mapping: nothing to protect.
    expect_err(
        "outside mapping",
        sys::mem_protect(p as u64 + 3 * PAGE as u64, PAGE as u64, READ),
        ENOMEM,
    )?;
    // Reserved page never touched: populated read-only on first access.
    mem::protect(unsafe { p.add(2 * PAGE) }, PAGE, READ)
        .map_err(|e| format!("reserved {}", err::name(e)))?;
    if unsafe { core::ptr::read_volatile(p.add(2 * PAGE)) } != 0 {
        return Err("reserved page not zero".into());
    }
    mem::unmap(p, 3 * PAGE).map_err(|e| format!("unmap {}", err::name(e)))?;
    Ok(())
}

/// Four threads race first-touch on shared pages; then a fixed mapping
/// over an occupied area must fail cleanly.
fn t_first_touch() -> R {
    const PAGES: usize = 512;
    let p =
        mem::map(PAGES * PAGE, READ | WRITE).map_err(|e| format!("map {}", err::name(e)))? as usize;
    let go = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicU32::new(0));
    let mut hs = Vec::new();
    for c in 0..4u8 {
        let (go, ready) = (go.clone(), ready.clone());
        hs.push(
            thread::spawn_on(Some(c), 0, move || {
                ready.fetch_add(1, Ordering::SeqCst);
                while !go.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                for k in 0..PAGES {
                    unsafe { core::ptr::write_volatile((p + k * PAGE + c as usize) as *mut u8, 1) };
                }
            })
            .map_err(|e| format!("thread {}", err::name(e)))?,
        );
    }
    while ready.load(Ordering::SeqCst) < 4 {
        sys::yield_now();
    }
    go.store(true, Ordering::Release);
    for h in hs {
        h.join();
    }
    for k in 0..PAGES {
        for c in 0..4 {
            if unsafe { core::ptr::read_volatile((p + k * PAGE + c) as *const u8) } != 1 {
                return Err(format!("page {} byte {} lost", k, c));
            }
        }
    }
    let ptr = p as *mut u8;
    expect_err(
        "mapping over occupied area",
        sys::mem_map(p as u64 + PAGE as u64, 2 * PAGE as u64, READ | WRITE),
        libnx::abi::err::EEXIST,
    )?;
    if unsafe { core::ptr::read_volatile(ptr.add(PAGE)) } != 1 {
        return Err("existing page destroyed by the failure".into());
    }
    mem::unmap(ptr, PAGES * PAGE).map_err(|e| format!("unmap {}", err::name(e)))?;
    Ok(())
}

/// `PIPE` endpoint: -EPIPE when the last writer/reader is gone.
fn t_pipe() -> R {
    use libnx::abi::endpoint::PIPE;
    use libnx::abi::err::EPIPE;
    use libnx::abi::rights::{DUP, GRANT, RECV, SEND};
    let ep = sys::check(unsafe { sys::syscall1(libnx::abi::sys::ENDPOINT_CREATE, PIPE) })
        .map_err(|e| format!("create {}", err::name(e)))? as u32;
    let r = sys::handle_dup(ep, RECV | GRANT | DUP, 0)
        .map_err(|e| format!("dup r {}", err::name(e)))?;
    let w = sys::handle_dup(ep, SEND | GRANT | DUP, 0)
        .map_err(|e| format!("dup w {}", err::name(e)))?;
    sys::handle_close(ep).map_err(|e| format!("close {}", err::name(e)))?;
    let sent = Arc::new(AtomicBool::new(false));
    let sent2 = sent.clone();
    let writer = thread::spawn(move || {
        sent2.store(
            ipc::call(w, &ipc::pack(libnx::abi::proto::console::WRITE, b"abc")).is_ok(),
            Ordering::SeqCst,
        );
        time::sleep_ms(5);
        let _ = sys::handle_close(w);
    })
    .map_err(|e| format!("thread {}", err::name(e)))?;
    let m = ipc::recv(r).map_err(|e| format!("recv {}", err::name(e)))?;
    let mut b = [0u8; 8];
    let n = ipc::unpack(&m.msg, &mut b);
    ipc::reply(&Msg::new(0, 0, 0)).map_err(|e| format!("reply {}", err::name(e)))?;
    if &b[..n] != b"abc" {
        return Err("content".into());
    }
    // Wait in progress when the writer closes: end of stream.
    expect_err("read after last writer", ipc::recv(r).map(|_| ()), EPIPE)?;
    writer.join();
    if !sent.load(Ordering::SeqCst) {
        return Err("write refused".into());
    }
    expect_err("read with no writer", ipc::recv(r).map(|_| ()), EPIPE)?;
    // With no reader, writes fail.
    let w2 = sys::handle_dup(r, RECV, 0).map_err(|e| format!("dup {}", err::name(e)))?;
    let _ = sys::handle_close(w2);
    let ep2 = sys::check(unsafe { sys::syscall1(libnx::abi::sys::ENDPOINT_CREATE, PIPE) })
        .map_err(|e| format!("create {}", err::name(e)))? as u32;
    let w3 = sys::handle_dup(ep2, SEND, 0).map_err(|e| format!("dup {}", err::name(e)))?;
    sys::handle_close(ep2).map_err(|e| format!("close {}", err::name(e)))?;
    expect_err(
        "write with no reader",
        ipc::call(w3, &Msg::new(1, 0, 0)).map(|_| ()),
        EPIPE,
    )?;
    let _ = sys::handle_close(w3);
    let _ = sys::handle_close(r);
    Ok(())
}

fn free_pages() -> u64 {
    let mut m = [MemInfo::default()];
    let buf = unsafe {
        core::slice::from_raw_parts_mut(m.as_mut_ptr() as *mut u8, core::mem::size_of_val(&m))
    };
    let _ = sys::sysinfo(info::MEMORY, buf);
    m[0].free_pages
}

fn t_shm() -> R {
    const LEN: usize = 64 * 1024;
    let before = free_pages();
    let a = SharedMemory::new(LEN, 0).map_err(|e| format!("create {}", err::name(e)))?;
    let b = sys::shm_map(a.handle(), 0, READ | WRITE)
        .map_err(|e| format!("2nd mapping {}", err::name(e)))?;
    let during = free_pages();
    if b == a.as_ptr() as u64 {
        return Err("same address for two mappings".into());
    }
    unsafe {
        core::ptr::write_volatile(a.as_ptr().add(100), 0x5A);
        if core::ptr::read_volatile((b + 100) as *const u8) != 0x5A {
            return Err("write invisible in the other mapping".into());
        }
        core::ptr::write_volatile((b + LEN as u64 - 1) as *mut u8, 0xA5);
        if core::ptr::read_volatile(a.as_ptr().add(LEN - 1)) != 0xA5 {
            return Err("write invisible (reverse way)".into());
        }
    }
    // Futex across two mappings: same physical page, same key.
    let wa = unsafe { &*(a.as_ptr().add(512) as *const AtomicU32) };
    let wb: &'static AtomicU32 = unsafe { &*((b + 512) as *const AtomicU32) };
    wa.store(0, Ordering::Release);
    let h = thread::spawn(move || {
        while wb.load(Ordering::Acquire) == 0 {
            let _ = sys::futex_wait(wb, 0, 0);
        }
    })
    .map_err(|e| format!("thread {}", e))?;
    time::sleep_ms(3);
    wa.store(1, Ordering::Release);
    let woken = sys::futex_wake(wa, 1);
    h.join();
    if woken != 1 {
        return Err(format!("futex across mappings: {} woken", woken));
    }
    // No handle left, one mapping: the pages stay.
    drop(a);
    if unsafe { core::ptr::read_volatile((b + 100) as *const u8) } != 0x5A {
        return Err("pages lost after handle close".into());
    }
    sys::mem_unmap(b, LEN as u64).map_err(|e| format!("unmap {}", err::name(e)))?;
    let after = free_pages();
    if after < during + (LEN / PAGE) as u64 || after + 8 < before {
        return Err(format!(
            "pages not returned: before {} during {} after {}",
            before, during, after
        ));
    }
    Ok(())
}

fn t_shm_dma() -> R {
    const LEN: usize = 1 << 20;
    let m = SharedMemory::new(LEN, CONTIGUOUS | UNCACHED)
        .map_err(|e| format!("create {}", err::name(e)))?;
    let p0 = m.paddr(0).map_err(|e| format!("paddr {}", err::name(e)))?;
    let p1 = m
        .paddr(LEN / 2 + 3)
        .map_err(|e| format!("paddr {}", err::name(e)))?;
    if p1 != p0 + (LEN / 2 + 3) as u64 || p0 % LEN as u64 != 0 || p0 < 0x8000_0000 {
        return Err(format!("physical addresses {:#x} / {:#x}", p0, p1));
    }
    m.as_mut_slice()[LEN - 1] = 7;
    if m.as_slice()[LEN - 1] != 7 {
        return Err("uncached mapping unreadable".into());
    }
    // Restricted handle: neither DMA nor write.
    let h2 = sys::handle_dup(m.handle(), rights::MAP | rights::DUP, 0)
        .map_err(|e| format!("dup {}", err::name(e)))?;
    expect_err("paddr without DMA right", sys::shm_paddr(h2, 0), EACCES)?;
    expect_err(
        "RW mapping without right",
        sys::shm_map(h2, 0, READ | WRITE),
        EACCES,
    )?;
    let ro = sys::shm_map(h2, 0, READ).map_err(|e| format!("RO mapping {}", err::name(e)))?;
    if unsafe { core::ptr::read_volatile((ro + LEN as u64 - 1) as *const u8) } != 7 {
        return Err("RO mapping: bad content".into());
    }
    expect_err(
        "mem_protect beyond rights",
        sys::mem_protect(ro, PAGE as u64, READ | WRITE),
        EACCES,
    )?;
    sys::mem_unmap(ro, LEN as u64).map_err(|e| format!("unmap {}", err::name(e)))?;
    let _ = sys::handle_close(h2);
    Ok(())
}

fn t_revoke() -> R {
    let ep = sys::endpoint_create().map_err(|e| format!("endpoint {}", err::name(e)))?;
    let h2 = sys::handle_dup(ep, rights::SEND | rights::DUP, 0)
        .map_err(|e| format!("dup {}", err::name(e)))?;
    expect_err(
        "call before revoke",
        ipc::call_timeout(h2, &Msg::new(1, 0, 0), POLL),
        ETIMEDOUT,
    )?;
    sys::handle_revoke(ep).map_err(|e| format!("revoke {}", err::name(e)))?;
    expect_err(
        "revoked copy",
        ipc::call_timeout(h2, &Msg::new(1, 0, 0), POLL),
        EBADF,
    )?;
    expect_err("original handle", ipc::recv_timeout(ep, POLL), ETIMEDOUT)?;
    let h3 = sys::handle_dup(ep, rights::SEND | rights::DUP, 0)
        .map_err(|e| format!("dup {}", err::name(e)))?;
    expect_err(
        "new copy",
        ipc::call_timeout(h3, &Msg::new(1, 0, 0), POLL),
        ETIMEDOUT,
    )?;
    expect_err("revoke without CONTROL", sys::handle_revoke(h3), EACCES)?;
    for h in [h2, h3, ep] {
        let _ = sys::handle_close(h);
    }
    Ok(())
}

// ------------------------------------------------------------ processes ---

/// Re-run ktest in a child process (`--child MODE ...`); return its code.
fn run_child(args: &[&str], envs: &[String]) -> Result<i64, String> {
    let mut argv = alloc::vec!["ktest", "--child"];
    argv.extend_from_slice(args);
    let h = process::spawn_with(&Strings::new(&argv, envs))
        .map_err(|e| format!("spawn {}", err::name(e)))?;
    let code = process::wait(h);
    let _ = sys::handle_close(h);
    Ok(code)
}

fn child(a: &[&str]) -> i32 {
    match a.first().copied() {
        Some("args") => {
            let all: Vec<&str> = env::args().collect();
            let ok = all == ["ktest", "--child", "args", "x y", "z"]
                && env::var("NXK_TEST").as_deref() == Some("42");
            i32::from(!ok)
        }
        Some("addrs") => {
            let b = env::startup_block().map(|b| b.load_base).unwrap_or(0);
            sys::process_exit(b as i64)
        }
        Some("stack") => {
            let x = 0u64;
            sys::process_exit(&x as *const u64 as i64)
        }
        Some("ro-write") => {
            let p = mem::map(PAGE, READ | WRITE).unwrap();
            mem::protect(p, PAGE, READ).unwrap();
            unsafe { core::ptr::write_volatile(p, 1) };
            0
        }
        Some("no-access") => {
            let p = mem::map(PAGE, READ | WRITE).unwrap();
            mem::protect(p, PAGE, 0).unwrap();
            unsafe { core::ptr::read_volatile(p) as i32 }
        }
        Some("target") => {
            let r: u64;
            unsafe {
                core::arch::asm!(
                    "mov x9, #7",
                    "brk #0x42",
                    "add x9, x9, #1",
                    "add x9, x9, #10",
                    "mov {r}, x9",
                    r = out(reg) r,
                    out("x9") _,
                )
            };
            sys::process_exit(r as i64)
        }
        Some("loop") => loop {
            core::hint::spin_loop();
        },
        Some("exec-data") => {
            let p = mem::map(PAGE, READ | WRITE).unwrap() as *mut u32;
            unsafe {
                p.write(0xd280_00e0); // mov x0, #7
                p.add(1).write(0xd65f_03c0); // ret
                let f: extern "C" fn() -> i32 = core::mem::transmute(p);
                f()
            }
        }
        _ => 2,
    }
}

fn t_args() -> R {
    match run_child(&["args", "x y", "z"], &[String::from("NXK_TEST=42")])? {
        0 => Ok(()),
        c => Err(format!("child missed its arguments (code {})", c)),
    }
}

fn t_aslr() -> R {
    const TEXT_BASE: i64 = 0x10_0000_0000;
    const HEAP_BASE: i64 = 0x20_0000_0000;
    let (b1, b2) = (run_child(&["addrs"], &[])?, run_child(&["addrs"], &[])?);
    for b in [b1, b2] {
        if !(TEXT_BASE..HEAP_BASE).contains(&b) || b % 0x10000 != 0 {
            return Err(format!("load base {:#x} out of range", b));
        }
    }
    let (s1, s2) = (run_child(&["stack"], &[])?, run_child(&["stack"], &[])?);
    if b1 == b2 || s1 == s2 {
        return Err(format!(
            "identical addresses: base {:#x}/{:#x}, stack {:#x}/{:#x}",
            b1, b2, s1, s2
        ));
    }
    Ok(())
}

fn t_jit() -> R {
    let p = mem::map(PAGE, READ | WRITE).map_err(|e| format!("map {}", err::name(e)))? as *mut u32;
    unsafe {
        p.write(0xd280_0540); // mov x0, #42
        p.add(1).write(0xd65f_03c0); // ret
    }
    mem::protect(p as *mut u8, PAGE, READ | EXEC)
        .map_err(|e| format!("protect RX {}", err::name(e)))?;
    let f: extern "C" fn() -> i64 = unsafe { core::mem::transmute(p) };
    let v = f();
    let _ = mem::unmap(p as *mut u8, PAGE);
    if v != 42 {
        return Err(format!("generated code returned {}", v));
    }
    Ok(())
}

fn t_faults() -> R {
    for mode in ["ro-write", "no-access", "exec-data"] {
        let c = run_child(&[mode], &[])?;
        if c != 139 {
            return Err(format!("{}: code {} instead of 139", mode, c));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- debug ---

use libnx::abi::debug::{self as dbg, Regs};

/// Wait for the next debug event: (kind, tid, pc).
fn next_event(ep: u32) -> Result<(u64, u32, u64), String> {
    let rx =
        ipc::recv_timeout(ep, 2000 * MS).map_err(|e| format!("no event ({})", err::name(e)))?;
    if rx.msg.label() != dbg::EVENT {
        return Err(format!("unexpected message (label {})", rx.msg.label()));
    }
    Ok((rx.msg.mr[0], rx.msg.mr[1] as u32, rx.msg.mr[4]))
}

fn resume(flags: u64) -> R {
    let mut m = Msg::new(dbg::RESUME, 1, 0);
    m.mr[0] = flags;
    ipc::reply(&m).map_err(|e| format!("resume {}", err::name(e)))
}

fn spawn_debuggee(mode: &str) -> Result<(u32, u32), String> {
    let argv = ["ktest", "--child", mode];
    let h = process::spawn_with(&Strings::new(&argv, &[]).suspended())
        .map_err(|e| format!("spawn {}", err::name(e)))?;
    let ep = sys::endpoint_create().map_err(|e| format!("endpoint {}", err::name(e)))?;
    sys::debug_attach(h, ep).map_err(|e| format!("attach {}", err::name(e)))?;
    Ok((h, ep))
}

fn t_debug() -> R {
    let (h, ep) = spawn_debuggee("target")?;
    // Started suspended: one stop before the first instruction.
    let (kind, tid, _) = next_event(ep)?;
    if kind != dbg::KIND_STOP {
        return Err(format!("first event {} instead of STOP", kind));
    }
    resume(0)?;
    let (kind, tid2, pc) = next_event(ep)?;
    if kind != dbg::KIND_BREAK || tid2 != tid {
        return Err(format!(
            "expected BRK from thread {}, got kind {} thread {}",
            tid, kind, tid2
        ));
    }
    let mut r: Regs =
        sys::debug_regs_read(h, tid).map_err(|e| format!("read regs {}", err::name(e)))?;
    if r.x[9] != 7 || r.pc != pc {
        return Err(format!(
            "regs: x9 = {}, pc {:#x} (event {:#x})",
            r.x[9], r.pc, pc
        ));
    }
    let mut code = [0u8; 4];
    sys::debug_mem_read(h, pc, &mut code).map_err(|e| format!("read memory {}", err::name(e)))?;
    if u32::from_le_bytes(code) != 0xd420_0840 {
        return Err(format!(
            "read instruction {:#x} instead of brk #0x42",
            u32::from_le_bytes(code)
        ));
    }
    let mut tids = [0u32; 8];
    let n = sys::debug_threads(h, &mut tids).map_err(|e| format!("threads {}", err::name(e)))?;
    if n != 1 || tids[0] != tid {
        return Err(format!("threads: {} ({:?})", n, &tids[..n.min(8)]));
    }
    // x9 := 100, skip the BRK, then a single step.
    r.x[9] = 100;
    r.pc += 4;
    sys::debug_regs_write(h, tid, &r).map_err(|e| format!("write regs {}", err::name(e)))?;
    resume(dbg::FLAG_STEP)?;
    let (kind, _, pc2) = next_event(ep)?;
    let r2 = sys::debug_regs_read(h, tid).map_err(|e| format!("regs {}", err::name(e)))?;
    if kind != dbg::KIND_STEP || pc2 != pc + 8 || r2.x[9] != 101 {
        return Err(format!(
            "single step: kind {}, pc {:#x} (expected {:#x}), x9 = {}",
            kind,
            pc2,
            pc + 8,
            r2.x[9]
        ));
    }
    resume(0)?;
    let code = process::wait(h);
    let _ = sys::handle_close(h);
    let _ = sys::handle_close(ep);
    if code != 111 {
        return Err(format!("exit code {} instead of 111", code));
    }
    Ok(())
}

fn t_debug_stop() -> R {
    let (h, ep) = spawn_debuggee("loop")?;
    let (kind, _, _) = next_event(ep)?;
    if kind != dbg::KIND_STOP {
        return Err(format!("first event {} instead of STOP", kind));
    }
    resume(0)?;
    time::sleep_ms(5); // it spins
    sys::debug_stop(h, 0).map_err(|e| format!("stop {}", err::name(e)))?;
    let (kind, _, _) = next_event(ep)?;
    if kind != dbg::KIND_STOP {
        return Err(format!("remote stop: kind {}", kind));
    }
    ipc::reply(&Msg::new(dbg::KILL, 0, 0)).map_err(|e| format!("kill {}", err::name(e)))?;
    let code = process::wait(h);
    let _ = sys::handle_close(h);
    let _ = sys::handle_close(ep);
    if code != 137 {
        return Err(format!("exit code {} instead of 137", code));
    }
    Ok(())
}

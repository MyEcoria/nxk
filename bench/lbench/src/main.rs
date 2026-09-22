//! lbench: Linux-side measures as /init of a minimal initramfs, raw syscalls (no libc).
//! Mirrors nxk `bench` (same loads, CNTVCT_EL0 clock); prints `RESULT <key> <value> <unit>`.

#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

// ------------------------------------------------------------- syscalls ---

const SYS_MOUNT: u64 = 40;
const SYS_OPENAT: u64 = 56;
const SYS_CLOSE: u64 = 57;
const SYS_PIPE2: u64 = 59;
const SYS_READ: u64 = 63;
const SYS_WRITE: u64 = 64;
const SYS_EXIT: u64 = 93;
const SYS_EXIT_GROUP: u64 = 94;
const SYS_FUTEX: u64 = 98;
const SYS_NANOSLEEP: u64 = 101;
const SYS_SCHED_SETAFFINITY: u64 = 122;
const SYS_GETPPID: u64 = 173;
const SYS_MUNMAP: u64 = 215;
const SYS_EXECVE: u64 = 221;
const SYS_MMAP: u64 = 222;
const SYS_WAIT4: u64 = 260;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAIT_PRIVATE: u64 = 128;
const FUTEX_WAKE_PRIVATE: u64 = 129;

#[inline(always)]
unsafe fn sc(nr: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let r: i64;
    unsafe {
        asm!("svc #0", inlateout("x0") a as i64 => r, in("x1") b, in("x2") c, in("x3") d,
             in("x4") e, in("x5") f, in("x8") nr, options(nostack))
    };
    r
}

fn write(fd: u64, b: &[u8]) {
    unsafe { sc(SYS_WRITE, fd, b.as_ptr() as u64, b.len() as u64, 0, 0, 0) };
}

fn read(fd: u64, b: &mut [u8]) -> i64 {
    unsafe { sc(SYS_READ, fd, b.as_mut_ptr() as u64, b.len() as u64, 0, 0, 0) }
}

fn exit_thread(code: u64) -> ! {
    unsafe { sc(SYS_EXIT, code, 0, 0, 0, 0, 0) };
    loop {}
}

fn exit_group(code: u64) -> ! {
    unsafe { sc(SYS_EXIT_GROUP, code, 0, 0, 0, 0, 0) };
    loop {}
}

fn pin(cpu: u32) {
    let mask: u64 = 1 << cpu;
    unsafe { sc(SYS_SCHED_SETAFFINITY, 0, 8, &mask as *const u64 as u64, 0, 0, 0) };
}

fn pipe() -> (u64, u64) {
    let mut fds = [0i32; 2];
    unsafe { sc(SYS_PIPE2, fds.as_mut_ptr() as u64, 0, 0, 0, 0, 0) };
    (fds[0] as u64, fds[1] as u64)
}

fn mmap(len: u64) -> u64 {
    // PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS
    unsafe { sc(SYS_MMAP, 0, len, 3, 0x22, -1i64 as u64, 0) as u64 }
}

fn futex_wait(w: &AtomicU32, v: u32) {
    unsafe { sc(SYS_FUTEX, w as *const _ as u64, FUTEX_WAIT_PRIVATE, v as u64, 0, 0, 0) };
}

fn futex_wake(w: &AtomicU32) {
    unsafe { sc(SYS_FUTEX, w as *const _ as u64, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0) };
}

#[inline(always)]
fn ticks() -> u64 {
    let v: u64;
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

fn freq() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack)) };
    v
}

fn ns(t: u64) -> u64 {
    ((t as u128 * 1_000_000_000) / freq() as u128) as u64
}

// ------------------------------------------------------------- threads ---

// long clone_run(flags, stack_top, ctid, fn, arg): the child calls fn(arg)
// on its own stack then exits.
global_asm!(
    r#"
.globl clone_run
clone_run:
    stp     x3, x4, [x1, #-16]!
    mov     x4, x2
    mov     x2, xzr
    mov     x3, xzr
    mov     x8, #220
    svc     #0
    cbnz    x0, 1f
    ldp     x1, x0, [sp], #16
    blr     x1
    mov     x0, #0
    mov     x8, #93
    svc     #0
1:  ret

.section .text._start, "ax"
.globl _start
_start:
    mov     x0, sp
    bl      lmain
    brk     #0
"#
);

extern "C" {
    fn clone_run(flags: u64, stack: u64, ctid: *const AtomicU32, f: extern "C" fn(u64), arg: u64) -> i64;
}

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const SIGCHLD: u64 = 17;

const STACK: u64 = 64 * 1024;

struct Thread {
    ctid: &'static AtomicU32,
}

/// Spawn `f(arg)` in a new thread of the process.
fn spawn_thread(f: extern "C" fn(u64), arg: u64) -> Thread {
    let stack = mmap(STACK);
    // The exit word lives at the stack bottom: still valid after exit.
    let ctid: &'static AtomicU32 = unsafe { &*(stack as *const AtomicU32) };
    ctid.store(1, Ordering::SeqCst);
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_CHILD_CLEARTID;
    unsafe { clone_run(flags, stack + STACK, ctid, f, arg) };
    Thread { ctid }
}

impl Thread {
    fn join(self) {
        // Kernel signals exit with a *shared* FUTEX_WAKE on the TID word; wait
        // without FUTEX_PRIVATE_FLAG (both keys must match, like glibc).
        loop {
            let v = self.ctid.load(Ordering::SeqCst);
            if v == 0 {
                break;
            }
            unsafe { sc(SYS_FUTEX, self.ctid as *const _ as u64, FUTEX_WAIT, v as u64, 0, 0, 0) };
        }
        let base = self.ctid as *const AtomicU32 as u64;
        unsafe { sc(SYS_MUNMAP, base, STACK, 0, 0, 0, 0) };
    }
}

// ------------------------------------------------------------- output ---

struct Out;

impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(1, s.as_bytes());
        Ok(())
    }
}

macro_rules! out {
    ($($a:tt)*) => { let _ = write!(Out, $($a)*); };
}

fn result(key: &str, v: u64, unit: &str) {
    out!("RESULT {} {} {}\n", key, v, unit);
}

// ------------------------------------------------------------ measure ---

const N: u64 = 20_000;

fn bench_null() -> u64 {
    let t0 = ticks();
    for _ in 0..N * 5 {
        unsafe { sc(SYS_GETPPID, 0, 0, 0, 0, 0, 0) };
    }
    ns(ticks() - t0) / (N * 5)
}

static mut P_REQ: (u64, u64) = (0, 0);
static mut P_RSP: (u64, u64) = (0, 0);
static mut SERVER_CPU: u32 = 0;

extern "C" fn pipe_server(bytes: u64) {
    write(1, b"  [pipe] server started\n");
    unsafe { pin(SERVER_CPU) };
    let mut buf = [0u8; 4096];
    let (req, rsp) = unsafe { (P_REQ.0, P_RSP.1) };
    loop {
        let mut got = 0;
        while got < bytes as usize {
            let n = read(req, &mut buf[got..bytes as usize]);
            if n <= 0 {
                if n < 0 {
                    out!("  [pipe] server: read -> {}\n", n);
                }
                return;
            }
            got += n as usize;
        }
        let w = unsafe { sc(SYS_WRITE, rsp, buf.as_ptr() as u64, 8, 0, 0, 0) };
        if w != 8 {
            out!("  [pipe] server: write -> {}\n", w);
        }
    }
}

/// Pipe round-trip: `bytes` bytes out, 8 bytes back.
fn bench_pipe(client_cpu: u32, server_cpu: u32, bytes: u64, iters: u64) -> u64 {
    pin(client_cpu);
    out!("  [pipe {}->{} {} B] affinity ok\n", client_cpu, server_cpu, bytes);
    unsafe {
        P_REQ = pipe();
        P_RSP = pipe();
        SERVER_CPU = server_cpu;
    }
    out!("  [pipe] pipes {:?} {:?}\n", unsafe { P_REQ }, unsafe { P_RSP });
    let t = spawn_thread(pipe_server, bytes);
    out!("  [pipe] server thread created\n");
    let (req, rsp) = unsafe { (P_REQ.1, P_RSP.0) };
    let data = [0x5Au8; 4096];
    let mut back = [0u8; 8];
    for k in 0..1000.min(iters) {
        write(req, &data[..bytes as usize]);
        read(rsp, &mut back);
        if k == 0 {
            out!("  [pipe] first round-trip ok\n");
        }
    }
    out!("  [pipe] warmup ok\n");
    let t0 = ticks();
    for k in 0..iters {
        let w = unsafe { sc(SYS_WRITE, req, data.as_ptr() as u64, bytes, 0, 0, 0) };
        let r = read(rsp, &mut back);
        if w != bytes as i64 || r != 8 {
            out!("  [pipe] client iter {}: write -> {}, read -> {}\n", k, w, r);
        }
        PROGRESS.fetch_add(1, Ordering::Relaxed);
    }
    let dt = ns(ticks() - t0);
    unsafe {
        sc(SYS_CLOSE, P_REQ.1, 0, 0, 0, 0, 0);
        t.join();
        sc(SYS_CLOSE, P_REQ.0, 0, 0, 0, 0, 0);
        sc(SYS_CLOSE, P_RSP.0, 0, 0, 0, 0, 0);
        sc(SYS_CLOSE, P_RSP.1, 0, 0, 0, 0, 0);
    }
    dt
}

static PROGRESS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn sysrq(c: u8) {
    let fd = unsafe { sc(SYS_OPENAT, -100i64 as u64, b"/proc/sysrq-trigger\0".as_ptr() as u64, 1, 0, 0, 0) };
    if fd >= 0 {
        write(fd as u64, &[c]);
        unsafe { sc(SYS_CLOSE, fd as u64, 0, 0, 0, 0, 0) };
    }
}

/// Watch progress; on stall, ask the kernel to dump blocked-task stacks
/// (sysrq w) and busy cores (sysrq l).
extern "C" fn watchdog(_: u64) {
    pin(3);
    let mut last = u64::MAX;
    loop {
        let ts = [10u64, 0];
        unsafe { sc(SYS_NANOSLEEP, ts.as_ptr() as u64, 0, 0, 0, 0, 0) };
        let p = PROGRESS.load(Ordering::Relaxed);
        if p == last && p != 0 {
            out!("WATCHDOG: no progress for 10 s (iter {})\n", p);
            sysrq(b'w');
            sysrq(b'l');
        }
        last = p;
    }
}

static REQ: AtomicU32 = AtomicU32::new(0);
static RSP: AtomicU32 = AtomicU32::new(0);

extern "C" fn futex_server(cpu: u64) {
    pin(cpu as u32);
    let mut last = 0;
    loop {
        let mut r = REQ.load(Ordering::Acquire);
        while r == last {
            futex_wait(&REQ, last);
            r = REQ.load(Ordering::Acquire);
        }
        if r == u32::MAX {
            return;
        }
        last = r;
        RSP.store(r, Ordering::Release);
        futex_wake(&RSP);
    }
}

/// Futex round-trip: the most direct Linux sync primitive (wake + wait,
/// no data copy).
fn bench_futex(client_cpu: u32, server_cpu: u32) -> u64 {
    pin(client_cpu);
    REQ.store(0, Ordering::SeqCst);
    RSP.store(0, Ordering::SeqCst);
    let t = spawn_thread(futex_server, server_cpu as u64);
    let run = |i: u32| {
        REQ.store(i, Ordering::Release);
        futex_wake(&REQ);
        let mut r = RSP.load(Ordering::Acquire);
        while r != i {
            futex_wait(&RSP, r);
            r = RSP.load(Ordering::Acquire);
        }
    };
    for i in 1..=1000 {
        run(i);
    }
    let t0 = ticks();
    for i in 1001..=(1000 + N as u32) {
        run(i);
    }
    let dt = ns(ticks() - t0) / N;
    REQ.store(u32::MAX, Ordering::Release);
    futex_wake(&REQ);
    t.join();
    dt
}

extern "C" fn nothing(_: u64) {}

fn bench_thread() -> u64 {
    let n = 500;
    let t0 = ticks();
    for _ in 0..n {
        spawn_thread(nothing, 0).join();
    }
    ns(ticks() - t0) / n
}

static ARGV: [u64; 3] = [0, 0, 0];
static PATH: &[u8] = b"/init\0";
static ARG1: &[u8] = b"--exit\0";

extern "C" fn exec_child(_: u64) {
    let argv = [PATH.as_ptr() as u64, ARG1.as_ptr() as u64, 0u64];
    let envp = [0u64];
    unsafe { sc(SYS_EXECVE, PATH.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0) };
    exit_thread(127);
}

/// Program spawn and wait (vfork + execve + wait4, i.e. what posix_spawn
/// does).
fn bench_spawn() -> u64 {
    let _ = &ARGV;
    let stack = mmap(STACK);
    let n = 200;
    let t0 = ticks();
    for _ in 0..n {
        let ctid = AtomicU32::new(0);
        let pid = unsafe { clone_run(CLONE_VM | CLONE_VFORK | SIGCHLD, stack + STACK, &ctid, exec_child, 0) };
        let mut status = 0i32;
        unsafe { sc(SYS_WAIT4, pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0) };
    }
    ns(ticks() - t0) / n
}

/// Pure integer compute, same as `bench` on nxk: a control (kernel-free).
fn bench_compute() -> u64 {
    let t0 = ticks();
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..20_000_000u64 {
        acc ^= acc >> 12;
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
    }
    core::hint::black_box(acc);
    ns(ticks() - t0) / 1_000_000
}

// ------------------------------------------------------------- memory ---

fn read_file(path: &[u8], buf: &mut [u8]) -> usize {
    // openat(AT_FDCWD, path, O_RDONLY)
    let fd = unsafe { sc(SYS_OPENAT, -100i64 as u64, path.as_ptr() as u64, 0, 0, 0, 0) };
    if fd < 0 {
        return 0;
    }
    let mut n = 0;
    loop {
        let r = read(fd as u64, &mut buf[n..]);
        if r <= 0 {
            break;
        }
        n += r as usize;
    }
    unsafe { sc(SYS_CLOSE, fd as u64, 0, 0, 0, 0, 0) };
    n
}

fn field(text: &str, key: &str) -> u64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let v = rest.trim_start_matches(':').trim();
            let num = v.split_whitespace().next().unwrap_or("0");
            return num.parse().unwrap_or(0);
        }
    }
    0
}

// ----------------------------------------------------------------- main ---

#[no_mangle]
extern "C" fn lmain(sp: *const u64) -> ! {
    let start = ticks();
    let argc = unsafe { *sp };
    if argc > 1 {
        exit_group(0); // child of the spawn test
    }
    unsafe {
        sc(SYS_MOUNT, b"proc\0".as_ptr() as u64, b"/proc\0".as_ptr() as u64, b"proc\0".as_ptr() as u64, 0, 0, 0);
        sc(SYS_MOUNT, b"sysfs\0".as_ptr() as u64, b"/sys\0".as_ptr() as u64, b"sysfs\0".as_ptr() as u64, 0, 0, 0);
    }
    spawn_thread(watchdog, 0);
    let mut buf = [0u8; 8192];
    let n = read_file(b"/proc/version\0", &mut buf);
    out!("\nlbench sur {}", core::str::from_utf8(&buf[..n]).unwrap_or("?\n"));
    result("boot_to_init_ms", ns(start) / 1_000_000, "ms");

    result("null_syscall_ns", bench_null(), "ns");
    result("ipc_pipe_same_core_ns", bench_pipe(0, 0, 8, N) / N, "ns");
    result("ipc_pipe_cross_core_ns", bench_pipe(0, 1, 8, N) / N, "ns");
    result("ipc_futex_same_core_ns", bench_futex(0, 0), "ns");
    result("ipc_futex_cross_core_ns", bench_futex(0, 1), "ns");
    let iters = N / 4;
    let dt = bench_pipe(0, 0, 4000, iters).max(1);
    result("ipc_bulk_4k_mbs", iters * 4000 * 1000 / dt, "MB/s");
    result("thread_create_join_us", bench_thread() / 1000, "us");
    result("spawn_wait_us", bench_spawn() / 1000, "us");
    result("compute_ms", bench_compute(), "ms");

    let n = read_file(b"/proc/meminfo\0", &mut buf);
    let mi = core::str::from_utf8(&buf[..n]).unwrap_or("");
    for k in ["MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "Slab", "KernelStack", "PageTables", "CmaTotal", "CmaFree"] {
        result(&alloc_key(k), field(mi, k), "kB");
    }
    let total = field(mi, "MemTotal");
    result("mem_used_kb", total - field(mi, "MemFree"), "kB");
    result("mem_not_managed_kb", 4 * 1024 * 1024 - total, "kB");
    let n = read_file(b"/proc/self/status\0", &mut buf);
    let st = core::str::from_utf8(&buf[..n]).unwrap_or("");
    result("proc_rss_kb", field(st, "VmRSS"), "kB");
    out!("RESULT END\n");

    loop {
        let ts = [3600u64, 0];
        unsafe { sc(SYS_NANOSLEEP, ts.as_ptr() as u64, 0, 0, 0, 0, 0) };
    }
}

/// Result key derived from the /proc/meminfo field name.
fn alloc_key(k: &str) -> heapless_key::Key {
    heapless_key::Key::new("meminfo_", k)
}

mod heapless_key {
    pub struct Key {
        buf: [u8; 48],
        len: usize,
    }
    impl Key {
        pub fn new(a: &str, b: &str) -> Self {
            let mut k = Key { buf: [0; 48], len: 0 };
            for &c in a.as_bytes().iter().chain(b.as_bytes()) {
                k.buf[k.len] = c.to_ascii_lowercase();
                k.len += 1;
            }
            k
        }
    }
    impl core::ops::Deref for Key {
        type Target = str;
        fn deref(&self) -> &str {
            core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    write(1, b"lbench: panic\n");
    exit_group(101)
}

//! nxk ABI: syscalls, errors, rights, layout, formats.
//! Linux arm64 convention (`svc #0`); IPC uses `x1`-`x7`, returns badge/token/pid.

#![no_std]

mod gpu_layout;

/// Syscall numbers; blocking calls take a timeout in ns.
/// (`timeout::INFINITE` = 0: no limit; `timeout::POLL` fails fast).
pub mod sys {
    pub const NOP: u64 = 0;
    pub const DEBUG_WRITE: u64 = 1;
    pub const YIELD: u64 = 2;
    pub const THREAD_EXIT: u64 = 3;
    pub const PROCESS_EXIT: u64 = 4;
    pub const SLEEP: u64 = 5;
    /// clock(id): `clock::MONOTONIC` or `clock::REALTIME`, in ns.
    pub const CLOCK: u64 = 6;
    pub const GETCPU: u64 = 7;
    /// getrandom(buf, len): random bytes (kernel ChaCha20);
    /// returns bytes written (at most 64 KiB per call).
    pub const GETRANDOM: u64 = 8;
    /// clock_set(sysctl, id, ns): set wall clock (SysControl capability).
    pub const CLOCK_SET: u64 = 9;

    pub const HANDLE_CLOSE: u64 = 10;
    pub const HANDLE_DUP: u64 = 11;
    /// handle_revoke(h): invalidate all other capabilities to the same
    /// object; returns a fresh handle. Requires CONTROL.
    pub const HANDLE_REVOKE: u64 = 12;
    /// getpid(process): id of the process named by a handle,
    /// or of the caller (0).
    pub const GETPID: u64 = 13;
    /// gettid(): id of the calling thread (used for debugging).
    pub const GETTID: u64 = 14;

    pub const MEM_MAP: u64 = 20;
    pub const MEM_UNMAP: u64 = 21;
    pub const MMIO_MAP: u64 = 22;
    pub const MMIO_DERIVE: u64 = 23;
    /// mem_protect(addr, len, prot): change rights of mapped pages.
    pub const MEM_PROTECT: u64 = 24;
    /// shm_create(len, flags): shared-memory object (`shm::*`).
    pub const SHM_CREATE: u64 = 25;
    /// shm_map(h, addr, prot): map the whole object (addr 0 = kernel picks).
    pub const SHM_MAP: u64 = 26;
    /// shm_paddr(h, offset): physical address of a page (DMA right).
    pub const SHM_PADDR: u64 = 27;

    pub const THREAD_CREATE: u64 = 30;
    /// thread_set(h, what, value): `thread::SET_*`; h = 0: current thread.
    pub const THREAD_SET: u64 = 31;

    /// futex_wait(addr, val, timeout): block while `*addr == val` (32 bits).
    /// Key is the physical address, so shared memory works.
    pub const FUTEX_WAIT: u64 = 35;
    /// futex_wake(addr, n): wake up to n waiters; returns count.
    pub const FUTEX_WAKE: u64 = 36;

    pub const PROCESS_SPAWN: u64 = 40;
    /// process_wait(h, timeout): exit code.
    pub const PROCESS_WAIT: u64 = 41;
    pub const PROCESS_KILL: u64 = 42;

    /// endpoint_create(flags): `endpoint::PIPE` for a pipe.
    pub const ENDPOINT_CREATE: u64 = 50;
    /// Blocking IPC calls read their timeout from x9.
    pub const IPC_CALL: u64 = 51;
    pub const IPC_SEND: u64 = 52;
    pub const IPC_RECV: u64 = 53;
    pub const IPC_REPLY: u64 = 54;
    pub const IPC_REPLY_RECV: u64 = 55;
    pub const IPC_REPLY_TO: u64 = 56;

    pub const NOTIFY_CREATE: u64 = 60;
    pub const NOTIFY_SIGNAL: u64 = 61;
    /// notify_wait(h, timeout).
    pub const NOTIFY_WAIT: u64 = 62;
    pub const NOTIFY_BIND: u64 = 63;

    pub const IRQ_GET: u64 = 70;
    pub const IRQ_BIND: u64 = 71;
    pub const IRQ_ACK: u64 = 72;

    pub const SYSINFO: u64 = 80;
    /// system_power(sysctl, cmd): `power::REBOOT` or `power::OFF`.
    /// Without a PSCI monitor, power-off goes through the PMIC.
    pub const SYSTEM_POWER: u64 = 81;
    /// klog_read(log, buf, len): unread kernel log bytes (at most `len`),
    /// 0 if none; never waits. Needs the `Log` capability.
    pub const KLOG_READ: u64 = 82;
    /// klog_bind(log, notif, bits): `notif` gets `bits` whenever the log
    /// gains data, and the kernel stops printing to UART-A and the screen
    /// itself. Returns the unread bytes it already printed (boot log).
    pub const KLOG_BIND: u64 = 83;

    /// debug_attach(process, endpoint): faults and breakpoints of the
    /// process become messages on the endpoint (CONTROL right).
    pub const DEBUG_ATTACH: u64 = 90;
    /// debug_regs(process, tid, buf, write): read/write `debug::Regs`.
    pub const DEBUG_REGS: u64 = 91;
    /// debug_mem(process, addr, buf, len | write << 63).
    pub const DEBUG_MEM: u64 = 92;
    /// debug_threads(process, buf, max): thread ids.
    pub const DEBUG_THREADS: u64 = 93;
    /// debug_stop(process, tid): stop a thread (0: all); each one
    /// produces a STOP event. Without a debugger they stay stopped.
    pub const DEBUG_STOP: u64 = 94;
}

/// Flags for `endpoint_create`.
pub mod endpoint {
    /// Pipe: writer (SEND-only) sends via `ipc_call`, reader (RECV)
    /// receives. No writers left: receive gives -EPIPE; no reader: send.
    pub const PIPE: u64 = 1 << 0;
}

/// Timeouts for blocking calls (relative nanoseconds).
pub mod timeout {
    pub const INFINITE: u64 = 0;
    /// Do not wait: fail immediately (-ETIMEDOUT) if it would block.
    pub const POLL: u64 = 1;
}

/// Counter tick <-> ns in fixed point: one multiply per conversion, no
/// division (`scale(v, mult) = v * mult >> 32`).
pub mod ticks {
    /// Multiplier from ticks to ns at counter frequency `freq` (Hz).
    pub const fn ns_mult(freq: u64) -> u64 {
        ((1_000_000_000u128 << 32) / if freq == 0 { 1 } else { freq } as u128) as u64
    }

    /// Multiplier from ns to ticks.
    pub const fn tick_mult(freq: u64) -> u64 {
        (((freq as u128) << 32) / 1_000_000_000) as u64
    }

    #[inline(always)]
    pub const fn scale(v: u64, mult: u64) -> u64 {
        ((v as u128 * mult as u128) >> 32) as u64
    }
}

/// Commands for `system_power`.
pub mod power {
    pub const OFF: u64 = 0;
    pub const REBOOT: u64 = 1;
}

/// Clocks for `clock`.
pub mod clock {
    /// Time since boot, never adjusted.
    pub const MONOTONIC: u64 = 0;
    /// Wall time (ns since 1970), set by the clock service.
    pub const REALTIME: u64 = 1;
}

/// Flags for `shm_create`.
pub mod shm {
    /// Physically contiguous pages (DMA buffers without IOMMU), max 4 MiB.
    pub const CONTIGUOUS: u64 = 1 << 0;
    /// Uncached mapping (Normal-NC): coherent with devices that
    /// do not snoop CPU caches.
    pub const UNCACHED: u64 = 1 << 1;
    /// Pages below 4 GiB (32-bit DMA devices).
    pub const DMA32: u64 = 1 << 2;
}

/// Error codes: Linux errno values, returned negated. File and network
/// services reuse them so std maps them to its usual `ErrorKind`s.
pub mod err {
    pub const EPERM: i64 = 1;
    pub const ENOENT: i64 = 2;
    pub const ESRCH: i64 = 3;
    pub const EINTR: i64 = 4;
    pub const EIO: i64 = 5;
    pub const ENXIO: i64 = 6;
    pub const E2BIG: i64 = 7;
    pub const ENOEXEC: i64 = 8;
    pub const EBADF: i64 = 9;
    pub const ECHILD: i64 = 10;
    pub const EAGAIN: i64 = 11;
    pub const ENOMEM: i64 = 12;
    pub const EACCES: i64 = 13;
    pub const EFAULT: i64 = 14;
    pub const EBUSY: i64 = 16;
    pub const EEXIST: i64 = 17;
    pub const EXDEV: i64 = 18;
    pub const ENODEV: i64 = 19;
    pub const ENOTDIR: i64 = 20;
    pub const EISDIR: i64 = 21;
    pub const EINVAL: i64 = 22;
    pub const ENFILE: i64 = 23;
    pub const EMFILE: i64 = 24;
    pub const ENOTTY: i64 = 25;
    pub const EFBIG: i64 = 27;
    pub const ENOSPC: i64 = 28;
    pub const ESPIPE: i64 = 29;
    pub const EROFS: i64 = 30;
    pub const EMLINK: i64 = 31;
    pub const EPIPE: i64 = 32;
    pub const ERANGE: i64 = 34;
    pub const EDEADLK: i64 = 35;
    pub const ENAMETOOLONG: i64 = 36;
    pub const ENOSYS: i64 = 38;
    pub const ENOTEMPTY: i64 = 39;
    pub const ELOOP: i64 = 40;
    pub const ENODATA: i64 = 61;
    pub const EOVERFLOW: i64 = 75;
    pub const EILSEQ: i64 = 84;
    pub const ENOTSOCK: i64 = 88;
    pub const EDESTADDRREQ: i64 = 89;
    pub const EMSGSIZE: i64 = 90;
    pub const EPROTOTYPE: i64 = 91;
    pub const ENOPROTOOPT: i64 = 92;
    pub const EPROTONOSUPPORT: i64 = 93;
    pub const EOPNOTSUPP: i64 = 95;
    pub const EAFNOSUPPORT: i64 = 97;
    pub const EADDRINUSE: i64 = 98;
    pub const EADDRNOTAVAIL: i64 = 99;
    pub const ENETDOWN: i64 = 100;
    pub const ENETUNREACH: i64 = 101;
    pub const ECONNABORTED: i64 = 103;
    pub const ECONNRESET: i64 = 104;
    pub const ENOBUFS: i64 = 105;
    pub const EISCONN: i64 = 106;
    pub const ENOTCONN: i64 = 107;
    pub const ESHUTDOWN: i64 = 108;
    pub const ETIMEDOUT: i64 = 110;
    pub const ECONNREFUSED: i64 = 111;
    pub const EHOSTUNREACH: i64 = 113;
    pub const EALREADY: i64 = 114;
    pub const EINPROGRESS: i64 = 115;
    /// Same as EAGAIN, as on Linux.
    pub const EWOULDBLOCK: i64 = EAGAIN;

    pub fn name(e: i64) -> &'static str {
        match e.unsigned_abs() as i64 {
            EPERM => "EPERM",
            ENOENT => "ENOENT",
            ESRCH => "ESRCH",
            EINTR => "EINTR",
            EIO => "EIO",
            ENXIO => "ENXIO",
            E2BIG => "E2BIG",
            ENOEXEC => "ENOEXEC",
            EBADF => "EBADF",
            ECHILD => "ECHILD",
            EAGAIN => "EAGAIN",
            ENOMEM => "ENOMEM",
            EACCES => "EACCES",
            EFAULT => "EFAULT",
            EBUSY => "EBUSY",
            EEXIST => "EEXIST",
            EXDEV => "EXDEV",
            ENODEV => "ENODEV",
            ENOTDIR => "ENOTDIR",
            EISDIR => "EISDIR",
            EINVAL => "EINVAL",
            ENFILE => "ENFILE",
            EMFILE => "EMFILE",
            ENOTTY => "ENOTTY",
            EFBIG => "EFBIG",
            ENOSPC => "ENOSPC",
            ESPIPE => "ESPIPE",
            EROFS => "EROFS",
            EMLINK => "EMLINK",
            EPIPE => "EPIPE",
            ERANGE => "ERANGE",
            EDEADLK => "EDEADLK",
            ENAMETOOLONG => "ENAMETOOLONG",
            ENOSYS => "ENOSYS",
            ENOTEMPTY => "ENOTEMPTY",
            ELOOP => "ELOOP",
            ENODATA => "ENODATA",
            EOVERFLOW => "EOVERFLOW",
            EILSEQ => "EILSEQ",
            ENOTSOCK => "ENOTSOCK",
            EDESTADDRREQ => "EDESTADDRREQ",
            EMSGSIZE => "EMSGSIZE",
            EPROTOTYPE => "EPROTOTYPE",
            ENOPROTOOPT => "ENOPROTOOPT",
            EPROTONOSUPPORT => "EPROTONOSUPPORT",
            EOPNOTSUPP => "EOPNOTSUPP",
            EAFNOSUPPORT => "EAFNOSUPPORT",
            EADDRINUSE => "EADDRINUSE",
            EADDRNOTAVAIL => "EADDRNOTAVAIL",
            ENETDOWN => "ENETDOWN",
            ENETUNREACH => "ENETUNREACH",
            ECONNABORTED => "ECONNABORTED",
            ECONNRESET => "ECONNRESET",
            ENOBUFS => "ENOBUFS",
            EISCONN => "EISCONN",
            ENOTCONN => "ENOTCONN",
            ESHUTDOWN => "ESHUTDOWN",
            ETIMEDOUT => "ETIMEDOUT",
            ECONNREFUSED => "ECONNREFUSED",
            EHOSTUNREACH => "EHOSTUNREACH",
            EALREADY => "EALREADY",
            EINPROGRESS => "EINPROGRESS",
            _ => "E?",
        }
    }
}

/// Rights carried by a handle.
pub mod rights {
    pub const SEND: u16 = 1 << 0;
    pub const RECV: u16 = 1 << 1;
    /// Handle may be transferred to another process (IPC, spawn).
    pub const GRANT: u16 = 1 << 2;
    pub const DUP: u16 = 1 << 3;
    pub const MAP: u16 = 1 << 4;
    pub const SIGNAL: u16 = 1 << 5;
    pub const WAIT: u16 = 1 << 6;
    pub const KILL: u16 = 1 << 7;
    /// Physical addresses of shared memory (DMA setup).
    pub const DMA: u16 = 1 << 8;
    /// Control: thread settings, process debugging, revocation.
    pub const CONTROL: u16 = 1 << 9;
    /// Shared memory or registers: writable mapping allowed (registers
    /// without it map read-only).
    pub const WRITE: u16 = 1 << 10;
    pub const ALL: u16 = 0x7FF;
}

/// Protections for `mem_map`.
pub mod prot {
    pub const READ: u64 = 1;
    pub const WRITE: u64 = 2;
    pub const EXEC: u64 = 4;
    /// Allocate pages immediately (otherwise: on first access, like mmap).
    pub const POPULATE: u64 = 1 << 8;
}

/// User layout (39-bit VA); below `USER_MIN` is the kernel view.
/// That region is inaccessible from EL0.
pub mod layout {
    pub const PAGE_SIZE: u64 = 4096;
    /// First 16 GiB are the kernel identity view (DRAM up to 14 GiB).
    pub const USER_MIN: u64 = 0x0000_0004_0000_0000; // 16 GiB
    pub const USER_MAX: u64 = 0x0000_0080_0000_0000; // 512 GiB
    pub const TEXT_BASE: u64 = 0x0000_0010_0000_0000; // program code
    pub const HEAP_BASE: u64 = 0x0000_0020_0000_0000; // address-less mem_map
    pub const HEAP_END: u64 = 0x0000_0040_0000_0000;
    pub const MMIO_BASE: u64 = 0x0000_0040_0000_0000; // device registers
    pub const MMIO_END: u64 = 0x0000_0060_0000_0000;
    pub const BOOTFS_BASE: u64 = 0x0000_0060_0000_0000; // boot image (init)
    pub const UTCB_BASE: u64 = 0x0000_007E_0000_0000; // one page per thread
    pub const STACK_TOP: u64 = 0x0000_007F_F000_0000; // main thread stack
    pub const STACK_SIZE: u64 = 256 * 1024;
    pub const MAX_THREADS_PER_PROCESS: usize = 4096;
}

/// Message tag: `[0:31]` label, `[32:41]` word length,
/// `[42:44]` handle count, `[48:63]` flags.
pub mod tag {
    /// Words carried in registers x2-x7.
    pub const REG_WORDS: usize = 6;
    /// Longest message (registers + UTCB).
    pub const MAX_WORDS: usize = super::utcb::MR_WORDS;
    pub const MAX_HANDLES: usize = 4;
    /// Synthetic message: a notification bound to the thread fired.
    pub const FLAG_NOTIFY: u64 = 1 << 48;

    #[inline(always)]
    pub const fn make(label: u32, words: usize, handles: usize) -> u64 {
        (label as u64) | ((words as u64 & 0x3FF) << 32) | ((handles as u64 & 0x7) << 42)
    }
    #[inline(always)]
    pub const fn label(t: u64) -> u32 {
        t as u32
    }
    #[inline(always)]
    pub const fn words(t: u64) -> usize {
        ((t >> 32) & 0x3FF) as usize
    }
    #[inline(always)]
    pub const fn handles(t: u64) -> usize {
        ((t >> 42) & 0x7) as usize
    }
    #[inline(always)]
    pub const fn flags(t: u64) -> u64 {
        t & (0xFFFF << 48)
    }
}

/// Per-thread control page (UTCB), mapped in its process and read by
/// the kernel via physical memory. Address is in `TPIDRRO_EL0`.
pub mod utcb {
    /// Message registers. The first 6 travel in x2-x7; the kernel
    /// only copies words 6 and up from the page.
    pub const MR_WORDS: usize = 504;

    #[repr(C, align(4096))]
    pub struct Utcb {
        pub mr: [u64; MR_WORDS],
        /// Handles to send (send) or received (receive).
        pub handles: [u32; super::tag::MAX_HANDLES],
        pub _reserved: [u64; 6],
    }

    const _: () = assert!(core::mem::size_of::<Utcb>() == 4096);
}

/// EL0 side of the calling convention: raw syscalls, IPC messages and
/// the UTCB. Shared by libnx and std (the kernel never calls these).
pub mod user {
    use core::arch::asm;

    use super::utcb::{Utcb, MR_WORDS};
    use super::{sys, tag};

    /// # Safety
    /// `nr` and its arguments must follow the syscall's contract.
    #[inline(always)]
    pub unsafe fn syscall0(nr: u64) -> i64 {
        let r: i64;
        unsafe { asm!("svc #0", lateout("x0") r, in("x8") nr, options(nostack)) };
        r
    }

    /// # Safety
    /// See `syscall0`.
    #[inline(always)]
    pub unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
        let r: i64;
        unsafe { asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x8") nr, options(nostack)) };
        r
    }

    /// # Safety
    /// See `syscall0`.
    #[inline(always)]
    pub unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
        let r: i64;
        unsafe {
            asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x8") nr,
                 options(nostack))
        };
        r
    }

    /// # Safety
    /// See `syscall0`.
    #[inline(always)]
    pub unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
        let r: i64;
        unsafe {
            asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x2") a2,
                 in("x8") nr, options(nostack))
        };
        r
    }

    /// # Safety
    /// See `syscall0`.
    #[inline(always)]
    pub unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
        let r: i64;
        unsafe {
            asm!("svc #0", inlateout("x0") a0 as i64 => r, in("x1") a1, in("x2") a2,
                 in("x3") a3, in("x8") nr, options(nostack))
        };
        r
    }

    /// Message: tag plus the 6 words carried in x2-x7.
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Msg {
        pub tag: u64,
        pub mr: [u64; 6],
    }

    impl Msg {
        pub const fn new(label: u32, words: usize, handles: usize) -> Self {
            Self {
                tag: tag::make(label, words, handles),
                mr: [0; 6],
            }
        }

        /// Words-only message (at most 6).
        pub fn from_words(label: u32, w: &[u64]) -> Self {
            let mut m = Self::new(label, w.len(), 0);
            m.mr[..w.len()].copy_from_slice(w);
            m
        }

        pub const fn label(&self) -> u32 {
            tag::label(self.tag)
        }

        pub const fn words(&self) -> usize {
            tag::words(self.tag)
        }

        pub const fn handles(&self) -> usize {
            tag::handles(self.tag)
        }

        /// Synthetic message: the thread-bound notification fired.
        pub const fn is_notify(&self) -> bool {
            self.tag & tag::FLAG_NOTIFY != 0
        }

        /// Same message plus `n` handles (set via `set_send_handle`).
        pub const fn with_handles(self, n: usize) -> Self {
            Self {
                tag: tag::make(self.label(), self.words(), n),
                mr: self.mr,
            }
        }

        /// Error reply (positive errno in the label).
        pub const fn error(e: i64) -> Self {
            Self::new(e.unsigned_abs() as u32, 0, 0)
        }
    }

    /// Received message: contents, sender badge, deferred-reply token,
    /// sender pid (set by the kernel; 0 for a notification).
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Received {
        pub msg: Msg,
        pub badge: u64,
        pub token: u64,
        pub pid: u32,
    }

    /// Generic-timer counter (`CNTVCT_EL0`, opened to EL0 by the kernel).
    #[inline(always)]
    pub fn counter() -> u64 {
        let v: u64;
        unsafe {
            asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags))
        };
        v
    }

    /// Counter frequency (Hz).
    #[inline(always)]
    pub fn counter_freq() -> u64 {
        let v: u64;
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    /// `ticks::ns_mult` of this counter (0: not computed yet).
    static NS_MULT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Counter ticks to ns.
    #[inline]
    pub fn ticks_to_ns(t: u64) -> u64 {
        use core::sync::atomic::Ordering::Relaxed;
        let mut m = NS_MULT.load(Relaxed);
        if m == 0 {
            m = super::ticks::ns_mult(counter_freq());
            NS_MULT.store(m, Relaxed);
        }
        super::ticks::scale(t, m)
    }

    /// Monotonic time in ns since boot, with no syscall (vDSO-style).
    #[inline]
    pub fn monotonic_ns() -> u64 {
        ticks_to_ns(counter())
    }

    /// Install thread pointer `tp` (TLS block, see `startup::Tls`).
    ///
    /// # Safety
    /// `tp` must be this thread's initialized TLS block, or 0.
    #[inline(always)]
    pub unsafe fn set_tp(tp: u64) {
        unsafe { asm!("msr tpidr_el0, {}", in(reg) tp, options(nomem, nostack, preserves_flags)) };
    }

    /// Current thread's UTCB (address in `TPIDRRO_EL0`, no syscall).
    #[inline(always)]
    pub fn utcb() -> *mut Utcb {
        let p: u64;
        unsafe {
            asm!("mrs {}, tpidrro_el0", out(reg) p, options(nomem, nostack, preserves_flags))
        };
        p as *mut Utcb
    }

    /// IPC syscall `nr` on `x0`: `timeout` (ns, 0 = no limit) travels
    /// in x9, the token comes back there. Returns x0 and the message.
    #[inline(always)]
    pub fn ipc(nr: u64, x0: u64, m: &Msg, timeout: u64) -> (i64, Received) {
        let (r0, t, a, b, c, d, e, f, badge, token, pid): (
            i64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
            u64,
        );
        unsafe {
            asm!("svc #0",
                inlateout("x0") x0 as i64 => r0,
                inlateout("x1") m.tag => t,
                inlateout("x2") m.mr[0] => a,
                inlateout("x3") m.mr[1] => b,
                inlateout("x4") m.mr[2] => c,
                inlateout("x5") m.mr[3] => d,
                inlateout("x6") m.mr[4] => e,
                inlateout("x7") m.mr[5] => f,
                inlateout("x8") nr => badge,
                inlateout("x9") timeout => token,
                lateout("x10") pid,
                options(nostack));
        }
        (
            r0,
            Received {
                msg: Msg {
                    tag: t,
                    mr: [a, b, c, d, e, f],
                },
                badge,
                token,
                pid: pid as u32,
            },
        )
    }

    /// `ipc` with its status checked: `Err(-errno)` from the kernel.
    #[inline(always)]
    pub fn ipc_checked(nr: u64, x0: u64, m: &Msg, timeout: u64) -> Result<Received, i64> {
        match ipc(nr, x0, m, timeout) {
            (r, _) if r < 0 => Err(r),
            (_, rx) => Ok(rx),
        }
    }

    /// Send a request and wait for the reply (`timeout` ns covers both,
    /// 0 = no limit).
    #[inline]
    pub fn call(ep: u32, m: &Msg, timeout: u64) -> Result<Msg, i64> {
        ipc_checked(sys::IPC_CALL, ep as u64, m, timeout).map(|r| r.msg)
    }

    /// Bytes per message: word 0 = length, bytes follow.
    pub const MAX_BYTES: usize = (MR_WORDS - 1) * 8;

    /// Word `i` of a message (registers or UTCB).
    #[inline]
    fn word(m: &Msg, i: usize) -> u64 {
        if i < tag::REG_WORDS {
            m.mr[i]
        } else {
            unsafe { (*utcb()).mr[i] }
        }
    }

    #[inline]
    fn set_word(m: &mut Msg, i: usize, v: u64) {
        if i < tag::REG_WORDS {
            m.mr[i] = v;
        } else {
            unsafe { (*utcb()).mr[i] = v };
        }
    }

    /// Header word `i` of a received message (register or UTCB).
    pub fn header(m: &Msg, i: usize) -> u64 {
        word(m, i)
    }

    /// Message of `head` header words, a length word, then the bytes
    /// (truncated to what fits). Also returns the byte count carried.
    pub fn pack_at(label: u32, head: &[u64], data: &[u8]) -> (Msg, usize) {
        let first = head.len();
        let n = data.len().min((MR_WORDS - 1 - first) * 8);
        let mut m = Msg::new(label, first + 1 + n.div_ceil(8), 0);
        for (i, &w) in head.iter().enumerate() {
            set_word(&mut m, i, w);
        }
        set_word(&mut m, first, n as u64);
        for (i, chunk) in data[..n].chunks(8).enumerate() {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            set_word(&mut m, first + 1 + i, u64::from_le_bytes(w));
        }
        (m, n)
    }

    /// Byte count a `pack_at` message with `first` header words
    /// advertises (no copy).
    pub fn bytes_len(m: &Msg, first: usize) -> usize {
        if m.words() <= first {
            return 0;
        }
        (word(m, first) as usize).min((m.words() - first - 1) * 8)
    }

    /// Bytes of a `pack_at` message with `first` header words.
    pub fn unpack_at(m: &Msg, first: usize, out: &mut [u8]) -> usize {
        let n = bytes_len(m, first).min(out.len());
        let mut done = 0;
        let mut wi = first + 1;
        while done < n {
            let b = word(m, wi).to_le_bytes();
            let k = (n - done).min(8);
            out[done..done + k].copy_from_slice(&b[..k]);
            done += k;
            wi += 1;
        }
        n
    }

    /// Handle `k` received with the last message.
    pub fn received_handle(k: usize) -> u32 {
        unsafe { (*utcb()).handles[k] }
    }

    /// Stage handle `k` for the next message sent (GRANT right needed).
    pub fn set_send_handle(k: usize, h: u32) {
        unsafe { (*utcb()).handles[k] = h };
    }
}

/// Args for `process_spawn`, passed by pointer (like `clone3`).
pub mod spawn {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct SpawnArgs {
        pub elf_ptr: u64,
        pub elf_len: u64,
        pub name_ptr: u64,
        pub name_len: u64,
        /// Array of `HandleGrant`: handles copied into the new process.
        pub grants_ptr: u64,
        pub grants_len: u64,
        /// Values received in x0 / x1 by the entry point.
        pub arg0: u64,
        pub arg1: u64,
        /// 0 = default priority.
        pub prio: u32,
        /// -1 = kernel picks.
        pub cpu: i32,
        /// Args then environment: `argc + envc` NUL-terminated strings
        /// back to back (64 KiB max).
        pub strings_ptr: u64,
        pub strings_len: u64,
        pub argc: u32,
        pub envc: u32,
        /// `SUSPENDED`: main thread waits for a debugger.
        pub flags: u64,
    }

    /// Suspended start: first thread stops before its first
    /// instruction until a debugger attaches.
    pub const SUSPENDED: u64 = 1 << 0;

    // stdio of a manager-spawned program (`proto::pm::SPAWN` flags):
    // one pipe for stdin/stdout/stderr, else console or nothing.
    pub const STDIN_PIPE: u64 = 1 << 8;
    pub const STDOUT_PIPE: u64 = 1 << 9;
    pub const STDERR_PIPE: u64 = 1 << 10;
    pub const STDIN_NULL: u64 = 1 << 11;
    pub const STDOUT_NULL: u64 = 1 << 12;
    pub const STDERR_NULL: u64 = 1 << 13;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct HandleGrant {
        pub handle: u32,
        pub slot: u32,
    }
}

/// Flags for `thread_create` and settings for `thread_set`.
pub mod thread {
    pub const DEFAULT_PRIO: u8 = 16;
    /// Lowest usable priority (0 = highest, reserved for the kernel).
    pub const MAX_PRIO: u8 = 30;
    /// thread_set: priority (1..=30; 0 = default).
    pub const SET_PRIO: u64 = 0;
    /// thread_set: pinned core (0..=3), or `ANY_CPU`.
    pub const SET_AFFINITY: u64 = 1;
    pub const ANY_CPU: u64 = u64::MAX;
    /// `prio` 0 = default; `cpu` None = kernel picks.
    pub const fn flags(prio: u8, cpu: Option<u8>) -> u64 {
        let c = match cpu {
            Some(c) => c as u64 + 1,
            None => 0,
        };
        (prio as u64) | (c << 8)
    }
}

/// Slots of handles handed out at startup (boot convention).
pub mod slots {
    // init
    pub const INIT_IRQ_CONTROL: u32 = 1;
    pub const INIT_MMIO_ROOT: u32 = 2;
    /// System control: wall clock.
    pub const INIT_SYSCTL: u32 = 3;
    /// Kernel log (`klog_read`, `klog_bind`), passed on to the console.
    pub const INIT_KLOG: u32 = 4;
    // init-spawned applications
    /// Standard output (console, or a pipe).
    pub const CONSOLE: u32 = 2;
    pub const PM: u32 = 3;
    /// Standard input (`startup::HAS_STDIO`): console, or a pipe.
    pub const STDIN: u32 = 4;
    /// Standard error (`startup::HAS_STDIO`): console, or a pipe.
    pub const STDERR: u32 = 5;
    // console driver (spawned separately)
    pub const DRV_SERVICE: u32 = 2;
    pub const DRV_MMIO: u32 = 3;
    pub const DRV_IRQ: u32 = 4;
    pub const DRV_KLOG: u32 = 5;
    /// Terminal driver or screen: console handle with
    /// `console::BADGE_TERMINAL` or `console::BADGE_SCREEN`.
    pub const TERMINAL: u32 = 6;
    /// Other drivers: resources from here (registers, read-only
    /// registers, IRQs, sysctl) in init's driver-table order.
    pub const DEV_BASE: u32 = 8;
}

/// Process startup: x0 = flags, x1 = launcher-provided argument,
/// x2 = address of the startup block (top of the main stack).
pub mod startup {
    pub const HAS_CONSOLE: u64 = 1 << 0;
    pub const HAS_PM: u64 = 1 << 1;
    /// First process: x1 = boot-image size.
    pub const IS_INIT: u64 = 1 << 2;
    pub const IS_DRIVER: u64 = 1 << 3;
    /// Slots `slots::STDIN` and `slots::STDERR` provided.
    pub const HAS_STDIO: u64 = 1 << 4;
    /// Standard input is a pipe (read via `ipc_recv`), not the console.
    pub const STDIN_PIPE: u64 = 1 << 5;
    /// Empty input, discarded outputs.
    pub const STDIN_NULL: u64 = 1 << 6;
    pub const STDOUT_NULL: u64 = 1 << 7;
    pub const STDERR_NULL: u64 = 1 << 8;
    /// Outputs are pipes: a write error is a broken pipe (no
    /// fallback to the kernel log, reserved for the console).
    pub const STDOUT_PIPE: u64 = 1 << 9;
    pub const STDERR_PIPE: u64 = 1 << 10;

    pub const BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"NXSB");
    /// Largest string area (args + environment).
    pub const MAX_STRINGS: u64 = 64 * 1024;

    /// Startup block, the argv/envp/auxv equivalent on Linux.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Block {
        pub magic: u32,
        pub version: u32,
        pub argc: u32,
        pub envc: u32,
        /// `argc` args then `envc` `NAME=value` vars, each NUL-terminated.
        pub strings: u64,
        pub strings_len: u64,
        /// Copy of the executable's program headers (TLS, relro...).
        pub phdr: u64,
        pub phnum: u32,
        pub phent: u32,
        /// Load base (0: fixed-address executable).
        pub load_base: u64,
        pub entry: u64,
        /// Main thread stack.
        pub stack_base: u64,
        pub stack_size: u64,
        /// Random seed (like AT_RANDOM).
        pub random: [u8; 16],
    }

    /// ELF program header, as copied into the block.
    #[repr(C)]
    struct Phdr {
        ptype: u32,
        pflags: u32,
        offset: u64,
        vaddr: u64,
        paddr: u64,
        filesz: u64,
        memsz: u64,
        align: u64,
    }

    const PT_TLS: u32 = 7;

    /// PT_TLS template: AArch64 variant 1, a 16-byte header at
    /// `TPIDR_EL0`, then `.tdata` and a zeroed `.tbss`.
    #[derive(Clone, Copy, Debug)]
    pub struct Tls {
        pub image: u64,
        pub filesz: usize,
        pub memsz: usize,
        pub align: usize,
    }

    impl Tls {
        /// Data offset from `TPIDR_EL0`.
        pub const fn data_offset(&self) -> usize {
            16usize.next_multiple_of(self.align)
        }

        /// Bytes of one thread's block.
        pub const fn block_size(&self) -> usize {
            self.data_offset() + self.memsz
        }
    }

    impl Block {
        /// The block at `p` (x2 at process entry), if valid.
        pub fn at(p: u64) -> Option<&'static Block> {
            if p == 0 {
                return None;
            }
            // SAFETY: the kernel leaves the block atop the main stack,
            // which outlives the process.
            let b = unsafe { &*(p as *const Block) };
            (b.magic == BLOCK_MAGIC).then_some(b)
        }

        /// `argc` args then `envc` variables, each NUL-terminated.
        pub fn string_area(&self) -> &'static [u8] {
            // SAFETY: copied by the kernel into the block's stack area.
            unsafe {
                core::slice::from_raw_parts(self.strings as *const u8, self.strings_len as usize)
            }
        }

        /// The executable's TLS template, if it has one.
        pub fn tls(&self) -> Option<Tls> {
            let mut t = None;
            for k in 0..self.phnum as usize {
                // SAFETY: `phnum` headers of `phent` bytes, copied by the kernel.
                let ph =
                    unsafe { &*((self.phdr + (k * self.phent as usize) as u64) as *const Phdr) };
                if ph.ptype == PT_TLS && ph.memsz > 0 {
                    t = Some(Tls {
                        image: self.load_base + ph.vaddr,
                        filesz: ph.filesz as usize,
                        memsz: ph.memsz as usize,
                        align: (ph.align as usize).max(16),
                    });
                }
            }
            t
        }
    }
}

/// Requests for `sysinfo`.
pub mod info {
    pub const PROCESSES: u64 = 0;
    pub const MEMORY: u64 = 1;
    pub const CPUS: u64 = 2;

    pub const PSTATE_ALIVE: u32 = 1;
    pub const PSTATE_ZOMBIE: u32 = 2;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct ProcInfo {
        pub pid: u32,
        pub threads: u32,
        pub pages: u64,
        pub name: [u8; 16],
        pub state: u32,
        pub parent: u32,
        /// CPU time consumed (ns).
        pub cpu_ns: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct MemInfo {
        pub total_pages: u64,
        pub free_pages: u64,
        pub kernel_pages: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct CpuInfo {
        pub online: u64,
        pub switches: u64,
        pub syscalls: u64,
        pub irqs: u64,
        pub idle_ns: u64,
        pub busy_ns: u64,
        /// Timer interrupts past their deadline, summed: the platform's
        /// delivery latency (an emulator host can stall it).
        pub timer_late_ns: u64,
        /// Timer or IPI interrupts that found the core idle and left it
        /// idle: wake-ups for nothing (a periodic tick shows here).
        pub empty_wakeups: u64,
    }
}

/// Debugger events: a stopped thread "calls" the endpoint (`EVENT`).
/// Reply `RESUME` (or `KILL`); registers change via `debug_regs`.
pub mod debug {
    pub const EVENT: u32 = 0x4442;
    pub const KIND_FAULT: u64 = 1;
    pub const KIND_BREAK: u64 = 2;
    pub const KIND_STEP: u64 = 3;
    pub const KIND_STOP: u64 = 4;

    pub const RESUME: u32 = 0;
    pub const KILL: u32 = 1;
    pub const FLAG_STEP: u64 = 1;

    /// Thread registers (read/written via `debug_regs`). Only the
    /// NZCV flags of `pstate` are writable.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    pub struct Regs {
        pub x: [u64; 31],
        pub sp: u64,
        pub pc: u64,
        pub pstate: u64,
        pub tpidr: u64,
    }
}

/// Boot-image (bootfs) format embedded in the kernel.
pub mod bootfs {
    pub const MAGIC: [u8; 8] = *b"NXKBOOT1";
    pub const NAME_LEN: usize = 24;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Header {
        pub magic: [u8; 8],
        pub count: u32,
        pub _reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Entry {
        pub name: [u8; NAME_LEN],
        pub offset: u32,
        pub size: u32,
    }
}

/// Service protocols; replies carry the error code in the label.
/// 0 = success, positive errno otherwise.
pub mod proto {
    pub mod console {
        /// Write: word 0 = byte count, bytes from word 1 on.
        pub const WRITE: u32 = 1;
        /// Read a line: reply has the same shape as WRITE.
        pub const READ_LINE: u32 = 2;
        /// Terminal driver only (`BADGE_TERMINAL`): typed bytes, same
        /// shape as WRITE, edited and echoed like UART input.
        pub const INPUT: u32 = 3;
        /// Terminal driver only: pending console output (echo included),
        /// at most word 0 bytes (0: as many as fit), none if caught up;
        /// never waits. Reply shaped like WRITE.
        pub const OUTPUT: u32 = 4;
        /// Terminal driver only: handle 0 = notification (SIGNAL) the
        /// console signals with word 0's bits once output is pending.
        pub const ATTACH: u32 = 5;
        /// Screen only (`BADGE_SCREEN`): output from position word 0
        /// (bytes ever written; the oldest one kept if it is older), at
        /// most word 1 bytes (0: as many as fit). Reply: word 0 = the
        /// position after them, then the bytes (see `pack_at`). Never
        /// waits and takes nothing from the terminal driver.
        pub const TAIL: u32 = 6;
        /// Badge of the console handle init gives a terminal driver.
        pub const BADGE_TERMINAL: u64 = 1;
        /// Badge of all other console handles: a badged handle cannot
        /// be re-minted, so holders cannot forge `BADGE_TERMINAL`.
        pub const BADGE_CLIENT: u64 = 2;
        /// Badge of the console handle init gives the screen (`display`):
        /// what the PC typed is in the output (echo), passphrases included.
        pub const BADGE_SCREEN: u64 = 3;
        /// Badge of the console shell's handle, which the programs it
        /// starts inherit: their output is the session, not a log.
        pub const BADGE_SESSION: u64 = 4;
        /// In the output, each write of another client and each piece of
        /// the kernel log comes between these marks (private OSC 7700,
        /// which terminals ignore): readers tell logs from the session.
        pub const LOG_START: &[u8] = b"\x1b]7700;log\x07";
        pub const LOG_END: &[u8] = b"\x1b]7700;end\x07";
    }
    pub mod pm {
        /// Spawn: bytes = argc, envc, flags, 0 (u32 each) then the
        /// strings (first arg is the program). Reply: process handle.
        pub const SPAWN: u32 = 1;
        /// Available programs, space-separated names.
        pub const LIST: u32 = 2;
        /// Service publication: bytes = name, handle 0 = endpoint (SEND).
        /// Only for init-spawned services.
        pub const REGISTER: u32 = 3;
        /// Service lookup: bytes = name. Reply: transmitted handle
        /// (ENOENT if not published).
        pub const LOOKUP: u32 = 4;
        /// Like LOOKUP, but waits until the service is published.
        pub const LOOKUP_WAIT: u32 = 5;
        /// Published services, space-separated names.
        pub const SERVICES: u32 = 6;
        /// Replace a boot driver with a program from the filesystem, with
        /// the same hardware grants: bytes = driver name, space, path. The
        /// old instance is killed and its services unpublished. Only from
        /// the shell init started (console user): a development path.
        pub const RESTART: u32 = 7;
        /// Badges of the manager endpoint: applications, services.
        pub const BADGE_APP: u64 = 1;
        pub const BADGE_SERVICE: u64 = 2;
    }
    /// Device clocks and resets (CAR).
    pub mod clk {
        /// word 0 = device: enable clock, take out of reset. Blocks in
        /// powered-off PMC partitions are untouched (ENODEV, EIO if not ready).
        pub const ENABLE: u32 = 1;
        pub const DISABLE: u32 = 2;
        /// word 0 = device. Reply: word 0 = state (`ST_*` bits). Only
        /// touch registers at full `ST_READY` (bus hangs otherwise).
        pub const STATUS: u32 = 3;
        /// All its clocks running.
        pub const ST_CLOCK: u64 = 1;
        /// Out of reset.
        pub const ST_OUT_OF_RESET: u64 = 2;
        /// Power partitions on and unclamped.
        pub const ST_POWERED: u64 = 4;
        pub const ST_READY: u64 = ST_CLOCK | ST_OUT_OF_RESET | ST_POWERED;
        pub const I2C1: u64 = 1;
        pub const I2C2: u64 = 2;
        pub const I2C3: u64 = 3;
        pub const I2C4: u64 = 4;
        pub const I2C5: u64 = 5;
        pub const I2C6: u64 = 6;
        pub const SDMMC1: u64 = 10;
        pub const SDMMC4: u64 = 13;
        /// xHCI host: host plus SuperSpeed logic clocks, XUSBA
        /// and XUSBC partitions.
        pub const XUSB_HOST: u64 = 20;
        /// USB2 OTG device controller (USB-C port, Erista): PLLU and
        /// UTMIPLL up, controller clock with a reset pulse.
        pub const USBD: u64 = 21;
        /// USB2 pad tracking clock, on during PHY bias calibration.
        pub const USB2_TRK: u64 = 22;
        /// Fuse controller, STATUS only (`ST_POWERED`: fuses visible).
        /// Read fuses only at `ST_READY`.
        pub const FUSE: u64 = 23;
        /// PCIe root complex: PCIe partition ungated, PLLE up, PEX pads
        /// out of deep power-down, AFI, PCIE and PCIEXCLK out of reset.
        /// STATUS reply word 1: PMC IO_DPD_STATUS (pads still powered down).
        pub const PCIE: u64 = 24;
        /// PCIEXCLK reset alone, once `PCIE` is ready: DISABLE holds the
        /// link logic while the AFI is programmed, ENABLE releases it.
        pub const PCIE_LINK: u64 = 25;
        /// PWM controller (fan on channel 1). Channel 0 dims the Erista
        /// backlight: a running block is left as is, DISABLE is refused.
        pub const PWM: u64 = 26;
        /// GM20B GPU: GPU and PLLG_REF clocks, PLLP_OUT5 at 204 MHz, the
        /// rail clamps (PMC GPU_RG_CNTRL) removed with the reset held.
        /// ENABLE needs the GPU rail on (`gpu` does it first); DISABLE
        /// clamps, resets and stops it before the rail goes off.
        pub const GPU: u64 = 27;
        /// UART B (right Joy-Con rail) and UART C (left), 1 Mbaud
        /// (`UART_BAUD` changes it): clock on, reset released.
        pub const UARTB: u64 = 28;
        pub const UARTC: u64 = 29;
        /// UART D (the BCM4356's Bluetooth HCI), 1 Mbaud like B and C:
        /// `bt` sets `BAUD_3M` and divides it in the UART itself.
        pub const UARTD: u64 = 31;
        /// Audio hub (AHUB, ADMA, I2S1) and the codec's clock: the AUD
        /// partition on, pad power for I2S1 and the audio pads (1.8 V),
        /// PLLA 368.64 MHz and its 36.864 MHz output, the APE, AHUB and
        /// I2S1 clocks, EXTERN1 at 12.288 MHz (the codec's MCLK).
        pub const AUDIO: u64 = 30;
        /// word 0 = `UARTB`, `UARTC` or `UARTD`, word 1 = `BAUD_1M`, `BAUD_3M`
        /// (the source clock at 16 times the baud rate, divisor latch 1)
        /// or `BAUD_115200` (hekate `clock_uart_use_src_div`). Reply: word
        /// 0 = state.
        pub const UART_BAUD: u32 = 4;
        pub const BAUD_1M: u64 = 1_000_000;
        pub const BAUD_3M: u64 = 3_000_000;
        /// PLLP / 2 without the 16x divider mode (hekate `uart_init`
        /// below 1 Mbaud): the UART divides 408 MHz itself (221: 115200).
        pub const BAUD_115200: u64 = 115_200;
    }
    /// Audio output (`audio`), published as "audio": 16-bit stereo PCM at
    /// 48 kHz to the speakers (the ALC5639 codec on I2S1, through the audio
    /// hub's ADMA). A client shares a ring of frames (`RING_*`): it writes
    /// frames after `RING_WRITE` and publishes the new position (release),
    /// the service copies them into the DMA ring and publishes `RING_READ`;
    /// an empty ring plays silence. One client at a time (`OPEN` replaces).
    pub mod audio {
        /// handle 0 = shm (any flags, read/write) of `RING_HEADER` bytes
        /// plus the frames: 4 bytes each, left then right, 16-bit signed.
        /// word 0 = its length. Reply: word 0 = frames in the ring.
        pub const OPEN: u32 = 3;
        /// Stop playing the client's ring.
        pub const CLOSE: u32 = 4;
        /// words: frequency (Hz), duration (ms), level (0..100): a sine on
        /// both speakers, with no client (the reply comes at once).
        pub const TONE: u32 = 1;
        /// Reply: word 0 = `ST_*` bits, word 1 = underruns (periods the
        /// ring could not fill), word 2 = periods played, word 3 = speaker
        /// volume (0..=100 %).
        pub const STATUS: u32 = 2;
        /// word 0 = speaker volume 0..=100 (%, 0 mutes the speakers), or
        /// `VOL_GET` to read without changing. Reply: word 0 = volume (0..=100).
        pub const VOLUME: u32 = 5;
        /// Start the built-in polyphonic test melody on both speakers.
        /// Reply: word 0 = duration (ms).
        pub const MUSIC: u32 = 6;
        pub const VOL_GET: u64 = u64::MAX;
        pub const DEFAULT_VOL: u8 = 70;
        /// Codec and hub up.
        pub const ST_READY: u64 = 1;
        /// The DMA is running.
        pub const ST_PLAYING: u64 = 2;
        pub const RATE: u32 = 48_000;
        /// Ring header (u32 words, bytes from the start): frames written
        /// by the client (wrapping count), frames read by the service,
        /// underruns. The frames start at `RING_HEADER`.
        pub const RING_WRITE: usize = 0;
        pub const RING_READ: usize = 4;
        pub const RING_UNDERRUNS: usize = 8;
        pub const RING_HEADER: usize = 64;
    }
    /// I2C bus.
    pub mod i2c {
        /// word 0 = bus (1..6) | address << 8 | read bytes << 16
        /// [| `REPEATED_START`]; write bytes follow (see `pack`).
        /// Reply: bytes read.
        pub const TRANSFER: u32 = 1;
        /// In word 0: the write then the read as one transaction, a
        /// repeated start between them instead of a stop (both needed).
        pub const REPEATED_START: u64 = 1 << 24;
        pub const fn header(bus: u64, addr: u64, read: u64) -> u64 {
            bus | addr << 8 | read << 16
        }
    }
    /// Power, time, sensors.
    pub mod power {
        pub const OFF: u32 = 1;
        pub const REBOOT: u32 = 2;
        /// Reply: word 0 = local temp, word 1 = remote (millidegrees C).
        pub const TEMPERATURE: u32 = 3;
        /// Re-read the RTC and update the kernel wall clock.
        pub const SYNC_TIME: u32 = 4;
        /// word 0 = 1: SD card I/O rail (PMIC LDO2) on at 3.3 V; 0: off.
        pub const SD_POWER: u32 = 5;
        /// word 0 = 1: PCIe/XUSB rail (PMIC LDO1) on at 1.05 V; 0: off.
        /// Powers the UPHY lanes the Wi-Fi chip sits on.
        pub const PCIE_POWER: u32 = 6;
        /// word 0 = 1: touchscreen rail (PMIC LDO6) on at 2.9 V; 0: off.
        pub const TOUCH_POWER: u32 = 7;
        /// Reply: word 0 = `SUPPLY_*`, word 1 = the charger's status
        /// register (BQ24193 REG08: VBUS_STAT bits 7:6, power good bit 2).
        pub const SUPPLY: u32 = 8;
        /// No input power: the charger's power good is clear.
        pub const SUPPLY_BATTERY: u64 = 0;
        /// Input power on the USB-C port: a PC, a charger or the dock,
        /// not told apart (that takes the USB-PD controller).
        pub const SUPPLY_EXTERNAL: u64 = 1;
        /// Fuel gauge (MAX17050) and charger. Reply: word 0 = state of
        /// charge (1/256 %), word 1 = cell voltage (mV), word 2 = average
        /// current (µA, signed, negative: discharging), word 3 = battery
        /// temperature (millidegrees C, signed), word 4 = remaining charge
        /// (mAh) | full charge (mAh) << 32, word 5 = `CHARGE_*`. Fails
        /// when the gauge does not answer.
        pub const BATTERY: u32 = 9;
        /// Reply: word 0 = age (% of design capacity), word 1 = charge
        /// cycles (1/100 cycle), word 2 = design capacity (mAh), word 3 =
        /// instantaneous current (µA, signed).
        pub const BATTERY_HEALTH: u32 = 10;
        /// The charger's state (BQ24193 REG08 CHRG_STAT bits 5:4).
        pub const CHARGE_NONE: u64 = 0;
        pub const CHARGE_PRE: u64 = 1;
        pub const CHARGE_FAST: u64 = 2;
        pub const CHARGE_DONE: u64 = 3;
    }
    /// Fan: speed from the SoC temperature, or set by hand.
    pub mod fan {
        /// Reply: word 0 = duty (% of full speed), word 1 = `ST_*` bits,
        /// word 2 = SoC temperature (millidegrees C, `NO_TEMP` if
        /// unreadable), word 3 = last measured speed (RPM, `NO_RPM`: none),
        /// word 4 = duty set by hand (%, `AUTO`: none).
        pub const STATUS: u32 = 1;
        /// word 0 = `AUTO`, or a duty 0..=100 (%) kept until `AUTO`.
        /// The SoC at `HOT_MC` or an unreadable sensor still overrides it.
        /// Reply: as `STATUS`.
        pub const SET: u32 = 2;
        /// Count tachometer pulses for 1 s. Reply: word 0 = RPM.
        pub const MEASURE: u32 = 3;
        /// word 0 = a `RAIL_*` user, word 1 = 1: it needs the battery's
        /// 5V rail (GPIO A5, also the Joy-Con rails' source), 0: it does
        /// not. The rail stays on while the fan runs or a user needs it.
        /// Reply: word 0 = the users' mask.
        pub const RAIL: u32 = 4;
        pub const RAIL_FAN: u64 = 1;
        pub const RAIL_JC_R: u64 = 2;
        pub const RAIL_JC_L: u64 = 4;
        pub const AUTO: u64 = u64::MAX;
        pub const NO_TEMP: u64 = i64::MIN as u64;
        pub const NO_RPM: u64 = u64::MAX;
        /// Duty set by hand.
        pub const ST_MANUAL: u64 = 1;
        /// Full speed: the SoC reached `HOT_MC`.
        pub const ST_HOT: u64 = 2;
        /// Sensor unreadable: at least hekate's highest step (43%).
        pub const ST_NO_SENSOR: u64 = 4;
        /// SoC temperature forcing full speed.
        pub const HOT_MC: i64 = 60_000;
    }
    /// CPU clock: PLLX at sys-clk's steps, the CPU rail at NVIDIA's
    /// PLL-source voltage for the chip's speedo (Erista only).
    pub mod cpufreq {
        /// Reply: word 0 = CPU clock (kHz), word 1 = CPU rail (µV, 0:
        /// unreadable), word 2 = CPU speedo (0: unknown), word 3 = `ST_*`
        /// bits, word 4 = steps this chip runs (bit i: `STEPS_MHZ[i]`),
        /// word 5 = step set (MHz, `BOOT`: the loader's setting).
        pub const STATUS: u32 = 1;
        /// word 0 = a `STEPS_MHZ` value, or `BOOT`. ERANGE: the step needs
        /// more than the rail's maximum on this chip. Reply: as `STATUS`.
        pub const SET: u32 = 2;
        pub const BOOT: u64 = 0;
        /// sys-clk's CPU steps (MHz); PLLX lands at or just below each.
        pub const STEPS_MHZ: [u64; 12] = [
            612, 714, 816, 918, 1020, 1122, 1224, 1326, 1428, 1581, 1683, 1785,
        ];
        /// Back to `BOOT`: the SoC reached `HOT_MC` above the boot clock.
        pub const ST_HOT: u64 = 1;
        /// Not an Erista Switch (or fuses unreadable): `BOOT` only.
        pub const ST_UNSUPPORTED: u64 = 2;
        pub const HOT_MC: i64 = 80_000;
    }
    /// GM20B GPU (`gpu`): off until POWER or a 2D / compute request
    /// without `IF_ON`. Requests are synchronous: the reply comes once
    /// the GPU is done. Coordinates and sizes pack as `x | y << 32`.
    /// Client memory must be `shm::UNCACHED`: the GPU does not snoop the
    /// CPU's caches.
    pub mod gpu {
        /// Reply: word 0 = state (`OFF`, `ON`, `FAILED`) | stage << 8
        /// (`STAGE_*` reached or failed in), word 1 = GPU clock (kHz, 0:
        /// PLL bypassed), word 2 = rail (µV), word 3 = chip ID (BOOT_0),
        /// word 4 = GPU speedo | speedo ID << 32, word 5 = last error
        /// (negated errno) or 0.
        pub const STATUS: u32 = 1;
        /// word 0 = 1: power up (secure boot, contexts); 0: power down.
        /// Reply: as `STATUS`.
        pub const POWER: u32 = 2;
        /// handle 0 = surface memory (shm, `DMA` right), words:
        /// width | height << 32, pitch (bytes), format (`FMT_*`).
        /// Reply: word 0 = surface ID.
        pub const ATTACH: u32 = 3;
        /// word 0 = surface ID.
        pub const DETACH: u32 = 4;
        /// 2D solid rectangle. words: surface, x | y << 32,
        /// width | height << 32, color (surface format), flags.
        pub const FILL: u32 = 5;
        /// 2D copy, same size, overlap allowed. words: destination |
        /// source << 32, destination x | y << 32, source x | y << 32,
        /// width | height << 32, flags.
        pub const COPY: u32 = 6;
        /// Built-in compute kernel on 32-bit elements. handle 0 = x,
        /// handle 1 = y (`K_COPY`, `K_SAXPY`), shm with the `DMA` right;
        /// words: kernel (`K_*`), count, value (u32 or f32 bits), flags.
        /// Reply: word 0 = GPU time (ns).
        pub const COMPUTE: u32 = 7;
        /// Self-test on the service's own buffers (2D fill and copy,
        /// the three kernels), checked by the CPU. Reply: word 0 = tests
        /// passed (bit `T_*`), word 1 = tests run, word 2 = GPU time (ns).
        pub const TEST: u32 = 8;
        /// word 0 = milliseconds of FFMA on every SM. Reply: word 0 =
        /// floating-point operations done, word 1 = GPU time (ns).
        pub const BENCH: u32 = 9;
        /// word 0 = a `STEPS_KHZ` value (0: report only), word 1 =
        /// `CLOCK_FORCE` to go past the power source's cap. Set at once
        /// when on (checked by a measurement), at power-up when off.
        /// ERANGE: not a step, or above the cap without `CLOCK_FORCE`;
        /// ENODEV: not an Erista Switch (`STEPS_KHZ[0]` only); EIO: the
        /// clock measured after the change was off (back to the lowest
        /// step). Reply: word 0 = step set (kHz), word 1 = rail (µV, 0:
        /// off), word 2 = cap (kHz), word 3 = supply (`power::SUPPLY_*`,
        /// `NO_SUPPLY`: unreadable), word 4 = clock measured after the
        /// last change (kHz, 0: not measured), word 5 = `CK_*` bits.
        pub const CLOCK: u32 = 10;
        pub const CLOCK_FORCE: u64 = 1;
        /// sys-clk's GPU steps (kHz), the GPCPLL at each exactly.
        pub const STEPS_KHZ: [u64; 12] = [
            76_800, 153_600, 230_400, 307_200, 384_000, 460_800, 537_600, 614_400, 691_200,
            768_000, 844_800, 921_600,
        ];
        /// sys-clk's Erista caps: on battery, then on input power; the
        /// steps above need `CLOCK_FORCE` (official charger or dock).
        pub const CAP_BATTERY_KHZ: u64 = 460_800;
        pub const CAP_EXTERNAL_KHZ: u64 = 768_000;
        pub const NO_SUPPLY: u64 = u64::MAX;
        /// Back to `STEPS_KHZ[0]`: the SoC reached `HOT_MC`.
        pub const CK_HOT: u64 = 1;
        /// Lowered to the cap: the power source changed.
        pub const CK_CAPPED: u64 = 2;
        /// Set with `CLOCK_FORCE`: no cap applies.
        pub const CK_FORCED: u64 = 4;
        pub const HOT_MC: i64 = 80_000;
        /// word 0 = instance of the page already mapped (0: none). Reply:
        /// word 0 = this instance (`S_INSTANCE`); handle 0 = the stats
        /// page, read-only, unless word 0 named it already.
        pub const STATS: u32 = 11;
        /// 3D engine self-test on the service's own buffers (clear, then a
        /// triangle with vertex and fragment shaders), checked by the CPU.
        /// Reply: word 0 = tests passed (bit `T3_*`), word 1 = tests run,
        /// word 2 = GPU time (ns).
        pub const TEST3D: u32 = 12;
        /// Stats page: `STATS_WORDS` u64 words, kept current by the
        /// service even while a batch runs. Written under `S_SEQ`: odd
        /// while it changes; a read is good if it saw one even value.
        pub const S_SEQ: usize = 0;
        /// Service start (`clock`, ns): another value is a restart.
        pub const S_INSTANCE: usize = 1;
        /// Time (ns) with a batch in flight since the service started,
        /// plus now - `S_SINCE_NS` while one runs (`S_SINCE_NS` != 0).
        pub const S_BUSY_NS: usize = 2;
        pub const S_SINCE_NS: usize = 3;
        /// As `STATUS` words 0 and 1: state | stage << 8, clock (kHz).
        pub const S_STATE: usize = 4;
        pub const S_KHZ: usize = 5;
        /// Rail set (µV, 0: off).
        pub const S_UV: usize = 6;
        /// As `CLOCK` words 0 and 3-5: step set, supply, measured, `CK_*`.
        pub const S_STEP: usize = 7;
        pub const S_SUPPLY: usize = 8;
        pub const S_MEASURED: usize = 9;
        pub const S_CK: usize = 10;
        /// The power source's cap (kHz), also with `CK_FORCED`.
        pub const S_CAP: usize = 11;
        /// Steps this chip runs (bit i: `STEPS_KHZ[i]`), 0: fuses unread.
        pub const S_STEPS: usize = 12;
        pub const STATS_WORDS: usize = 13;

        pub const OFF: u64 = 0;
        pub const ON: u64 = 1;
        pub const FAILED: u64 = 2;
        /// Power-up stages, in order.
        pub const STAGE_RAIL: u64 = 1;
        pub const STAGE_CLOCKS: u64 = 2;
        pub const STAGE_PLL: u64 = 3;
        pub const STAGE_MMU: u64 = 4;
        pub const STAGE_FIRMWARE: u64 = 5;
        pub const STAGE_ACR: u64 = 6;
        pub const STAGE_FECS: u64 = 7;
        pub const STAGE_CONTEXT: u64 = 8;
        pub const STAGE_CHANNEL: u64 = 9;
        pub const STAGE_READY: u64 = 10;

        /// Surface formats (Fermi 2D numbering).
        pub const FMT_A8R8G8B8: u64 = 0xCF;
        pub const FMT_A8B8G8R8: u64 = 0xD5;
        /// Flag: fail with EAGAIN rather than power the GPU on.
        pub const IF_ON: u64 = 1;

        /// x[i] = value.
        pub const K_FILL: u64 = 1;
        /// y[i] = x[i].
        pub const K_COPY: u64 = 2;
        /// y[i] = value * x[i] + y[i] (f32).
        pub const K_SAXPY: u64 = 3;

        /// `TEST` bits.
        pub const T_FILL2D: u64 = 1;
        pub const T_COPY2D: u64 = 2;
        pub const T_KFILL: u64 = 4;
        pub const T_KCOPY: u64 = 8;
        pub const T_KSAXPY: u64 = 16;
        /// Share a buffer with the engine (vertices, indices, command
        /// streams): handle 0 = shm with the `DMA` right (`MAP` too, for a
        /// command stream); word 0 = length in bytes. Reply: word 0 =
        /// buffer ID.
        pub const BUFFER_ATTACH: u32 = 13;
        /// word 0 = buffer ID.
        pub const BUFFER_DETACH: u32 = 14;
        /// A block-linear color image or a depth buffer to draw on:
        /// handle 0 = shm with the `DMA` right; words: width | height <<
        /// 32, kind (`TARGET_*`), block height (log2 of GOBs, no taller
        /// than the image needs; the scanout images have
        /// `display::SCANOUT`'s), format (`TF_*`, 0: RGBA8; colors only).
        /// The shm is `tiled_bytes_bpp(width, height, tf_bytes(format),
        /// block height)` long. The same shm can be attached as a
        /// texture too (`TEXTURE_ATTACH`, same format and block height)
        /// to sample what was drawn: a pass that draws it and one that
        /// samples it are separated by a change of target, which waits
        /// for the first to finish. Reply: word 0 = target ID (its own
        /// numbering).
        pub const TARGET_ATTACH: u32 = 15;
        pub const TARGET_DETACH: u32 = 16;
        pub const TARGET_COLOR: u64 = 0;
        pub const TARGET_DEPTH: u64 = 1;
        /// Run a command stream (`CMD_*`): words: buffer ID, first word,
        /// word count. Reply: word 0 = GPU time (ns). The stream is
        /// checked first: IDs, offsets and sizes; nothing runs on a bad one.
        pub const RENDER: u32 = 17;
        /// Drop every buffer and target a client attached (it is gone).
        pub const RESET: u32 = 18;
        pub use super::super::gpu_layout::{
            level_block_log2, level_bytes, level_extent, level_offset, max_levels, mip_bytes,
            tf_bytes, tiled_bytes_bpp, tiled_offset,
        };
        /// Formats of render targets and textures. RGBA8: four bytes, 0..255
        /// read as 0..1; the others hold floats.
        pub const TF_RGBA8: u64 = 0;
        pub const TF_R32F: u64 = 1;
        pub const TF_RGBA16F: u64 = 2;
        pub const TF_RGBA32F: u64 = 3;
        /// Bytes of a block-linear image `width` x `height` of four-byte
        /// pixels, GOBs `1 << block_log2` high per block; the depth
        /// buffer is the same with 16 (`DEPTH_BLOCK_LOG2`).
        pub const fn tiled_bytes(width: u32, height: u32, block_log2: u32) -> usize {
            tiled_bytes_bpp(width, height, 4, block_log2)
        }
        pub const DEPTH_BLOCK_LOG2: u32 = 4;
        /// A texture to sample: handle 0 = shm with the `DMA` right
        /// holding a block-linear image, `tiled_bytes_bpp(width, height,
        /// tf_bytes(format), block height)` long; words: width | height
        /// << 32, flags (`TEX_*`), format (`TF_*`, 0: RGBA8, bytes in R,
        /// G, B, A order), block height (0: `tex_block_log2` of the
        /// height; n: log2 of GOBs + 1, no taller than the image needs,
        /// a render target's). Reply: word 0 = texture ID (its own
        /// numbering). The image is sampled normalized, without mipmaps.
        pub const TEXTURE_ATTACH: u32 = 19;
        /// word 0 = texture ID.
        pub const TEXTURE_DETACH: u32 = 20;
        /// Flags: nearest filtering (linear otherwise), clamp to the
        /// edge (repeat otherwise).
        pub const TEX_NEAREST: u64 = 1;
        pub const TEX_CLAMP: u64 = 2;
        /// Block height (log2 of GOBs) of a texture `height` rows high:
        /// no taller than the image needs.
        pub const fn tex_block_log2(height: u32) -> u32 {
            match height {
                0..=8 => 0,
                9..=16 => 1,
                17..=32 => 2,
                33..=64 => 3,
                _ => 4,
            }
        }
        /// Commands: a word with the opcode (low 16 bits) and the count
        /// of words that follow (high 16), then those.
        /// `[color target, depth target or NO_TARGET]`. Switching to another
        /// target first waits for the draws before it and drops the texture
        /// caches: what a pass drew can be sampled by the next.
        pub const CMD_TARGET: u32 = 1;
        /// `[r, g, b, a (f32 bits)]`: color to all the target, depth to 1.
        pub const CMD_CLEAR: u32 = 2;
        /// `[pipeline (PIPE_*), flags (PF_*)]`.
        pub const CMD_PIPELINE: u32 = 3;
        /// Words for the shaders' constant buffer 1 (`PIPE_LIT`: 33; a shader of
        /// the IR reads up to 1024).
        pub const CMD_UNIFORMS: u32 = 4;
        /// `[vertex buffer, byte offset, vertices, first, count]`.
        pub const CMD_DRAW: u32 = 5;
        /// `[vertex buffer, byte offset, vertices, index buffer, byte
        /// offset, indices (u16), first, count]`.
        pub const CMD_DRAW_INDEXED: u32 = 6;
        /// `[texture ID]`: the texture `PIPE_TEXTURED` samples, for the
        /// draws that follow (before or after `CMD_UNIFORMS`).
        pub const CMD_TEXTURE: u32 = 7;
        pub const NO_TARGET: u32 = u32::MAX;
        /// Vertex of 32 bytes, clip-space position and color (4 floats each).
        pub const PIPE_FLAT: u32 = 0;
        /// Vertex of 28 bytes: position, normal (3 floats), color (4
        /// unorm8); one directional light.
        pub const PIPE_LIT: u32 = 1;
        /// Vertex of 36 bytes: position, normal (3 floats), texture
        /// coordinates (2 floats), color (4 unorm8); `PIPE_LIT`'s light
        /// and uniforms, the color times the bound texture's texel.
        pub const PIPE_TEXTURED: u32 = 2;
        /// Cull back faces (counter-clockwise front).
        pub const PF_CULL_BACK: u32 = 1;
        pub const LIT_UBO_WORDS: usize = 33;
        /// The shader compiler's self-test, apart from `TEST` (a program
        /// the GPU cannot run resets it): as `TEST3D`'s reply, with the
        /// `T3_IR_*` bits.
        pub const TEST_IR: u32 = 25;
        /// `TEST` bits of the 3D engine (and `TEST3D`'s alone).
        pub const T3_CLEAR: u64 = 32;
        pub const T3_TRIANGLE: u64 = 64;
        pub const T3_DEPTH: u64 = 128;
        pub const T3_LIT: u64 = 256;
        pub const T3_TEXTURE: u64 = 512;
        /// Compute programs of the IR (random ones, 64 threads each) give
        /// the reference machine's results; a vertex and a fragment shader
        /// of the IR with a uniform, a texture and a bound state draw the
        /// expected pixel.
        pub const T3_IR_COMPUTE: u64 = 1024;
        pub const T3_IR_DRAW: u64 = 2048;
        /// Compile a shader written in `nxk-shader`'s IR: handle 0 = shm
        /// (readable, `MAP` right) holding its wire words; word 0 = number
        /// of words. Reply: word 0 = shader ID, word 1 = registers per
        /// thread, word 2 = stage (0 vertex, 1 fragment, 2 compute).
        /// EINVAL: it does not decode, validate or compile (the reason is
        /// logged); ENOSPC: no room for its code.
        pub const SHADER_ATTACH: u32 = 21;
        /// word 0 = shader ID.
        pub const SHADER_DETACH: u32 = 22;
        /// A graphics pipeline: handle 0 = shm (readable) with the `PD_*`
        /// words; word 0 = number of words. Reply: word 0 = pipeline ID.
        pub const PIPELINE_ATTACH: u32 = 23;
        /// word 0 = pipeline ID.
        pub const PIPELINE_DETACH: u32 = 24;
        /// Words of a pipeline description: vertex shader ID, fragment
        /// shader ID, bytes per vertex, `PF_*` flags, `BLEND_*`, `DEPTH_*`,
        /// the number of attributes, then that many words
        /// (`offset | format << 16`); attribute n feeds the vertex
        /// shader's input n.
        pub const PD_VERTEX: usize = 0;
        pub const PD_FRAGMENT: usize = 1;
        pub const PD_STRIDE: usize = 2;
        pub const PD_FLAGS: usize = 3;
        pub const PD_BLEND: usize = 4;
        pub const PD_DEPTH: usize = 5;
        pub const PD_COUNT: usize = 6;
        pub const PD_ATTRS: usize = 7;
        pub const MAX_ATTRIBUTES: usize = 16;
        /// Vertex attribute formats.
        pub const AF_F32X1: u32 = 1;
        pub const AF_F32X2: u32 = 2;
        pub const AF_F32X3: u32 = 3;
        pub const AF_F32X4: u32 = 4;
        /// Four bytes, 0..255 read as 0..1.
        pub const AF_U8X4_UNORM: u32 = 5;
        /// More `PF_*` flags (with `PF_CULL_BACK`): no depth writes, no
        /// depth test, cull front faces.
        pub const PF_NO_DEPTH_WRITE: u32 = 2;
        pub const PF_NO_DEPTH_TEST: u32 = 4;
        pub const PF_CULL_FRONT: u32 = 8;
        /// Blending of color target 0: none; source alpha over; source
        /// alpha added; premultiplied (source added, destination faded by
        /// the source alpha).
        pub const BLEND_NONE: u32 = 0;
        pub const BLEND_ALPHA: u32 = 1;
        pub const BLEND_ADD: u32 = 2;
        pub const BLEND_PREMUL: u32 = 3;
        /// Depth comparisons: a fragment passes when it is less, ...
        pub const DEPTH_LESS: u32 = 0;
        pub const DEPTH_LEQUAL: u32 = 1;
        pub const DEPTH_GREATER: u32 = 2;
        pub const DEPTH_GEQUAL: u32 = 3;
        pub const DEPTH_EQUAL: u32 = 4;
        pub const DEPTH_ALWAYS: u32 = 5;
        /// `[pipeline ID]`: the shaders and state of a pipeline made with
        /// `PIPELINE_ATTACH` for the draws that follow (a target first).
        pub const CMD_USE_PIPELINE: u32 = 8;
        /// `[slot, buffer ID]`: buffer `slot` (0-7) of the shaders'
        /// `Buf*` instructions (bounds: the buffer's length in words).
        pub const CMD_BIND_BUFFER: u32 = 9;
        /// `[slot, texture ID]`: texture `slot` (0-15) of the shaders'
        /// `Tex` instructions.
        pub const CMD_BIND_TEXTURE: u32 = 10;
        /// `[compute shader ID, groups x, y, z]`: after the draws before
        /// it are done; the uniforms and bindings as set.
        pub const CMD_DISPATCH: u32 = 11;
        /// Nothing before this is still running when the commands after
        /// it start (a pass that reads what the last wrote).
        pub const CMD_BARRIER: u32 = 12;
        pub const SHADER_BUFFERS: usize = 8;
        pub const SHADER_TEXTURES: usize = 16;
    }
    /// Block device (SD card, eMMC).
    pub mod blk {
        /// Reply: word 0 = block size, word 1 = block count.
        pub const INFO: u32 = 1;
        /// handle 0 = transfer buffer, which the driver's DMA reads and
        /// writes directly: 256 KiB, `CONTIGUOUS | UNCACHED | DMA32`, `DMA` right.
        pub const ATTACH: u32 = 2;
        /// words: start block, block count, buffer offset.
        pub const READ: u32 = 3;
        pub const WRITE: u32 = 4;
    }
    /// Filesystem.
    pub mod fs {
        /// bytes = path; word before: flags (`O_*`). Reply:
        /// word 0 = descriptor, word 1 = size.
        pub const OPEN: u32 = 1;
        /// words: descriptor, position, length (<= MAX_IO).
        /// Reply: bytes read.
        pub const READ: u32 = 2;
        /// words: descriptor, position; bytes to write.
        pub const WRITE: u32 = 3;
        pub const CLOSE: u32 = 4;
        /// bytes = path. Reply: word 0 = size, word 1 = kind,
        /// word 2 = mtime (s since 1970).
        pub const STAT: u32 = 5;
        /// words: index; bytes = dir path. Reply: word 0 = size,
        /// word 1 = kind, bytes = name (empty: end of dir).
        pub const READDIR: u32 = 6;
        pub const MKDIR: u32 = 7;
        pub const UNLINK: u32 = 8;
        pub const RMDIR: u32 = 9;
        /// bytes = "old\0new".
        pub const RENAME: u32 = 10;
        /// words: descriptor, new size.
        pub const TRUNCATE: u32 = 11;
        /// Flush everything cached to the medium.
        pub const SYNC: u32 = 12;
        /// Reply: word 0 = free bytes, word 1 = total bytes.
        pub const STATFS: u32 = 13;
        /// words: descriptor. Reply like STAT (open file).
        pub const FSTAT: u32 = 14;
        /// words: descriptor. Extra reference to the same open file;
        /// CLOSE only closes it at the last one.
        pub const DUP: u32 = 15;

        pub const O_READ: u64 = 1;
        pub const O_WRITE: u64 = 2;
        pub const O_CREATE: u64 = 4;
        pub const O_TRUNC: u64 = 8;
        pub const O_EXCL: u64 = 16;

        pub const KIND_FILE: u64 = 1;
        pub const KIND_DIR: u64 = 2;
        /// Bytes per single-message read or write.
        pub const MAX_IO: usize = 3 * 1024;
    }
    /// Display.
    pub mod display {
        /// Reply: words = width, height, stride (bytes per row), format
        /// (1 = A8B8G8R8); handle 0 = framebuffer memory.
        pub const OPEN: u32 = 1;
        /// Text for the on-screen terminal (see `pack`).
        pub const WRITE: u32 = 2;
        /// Like `OPEN`, but the UI keeps running: handle 0 = the image
        /// being displayed, read-only (what `dbg` captures). Same words,
        /// then the layout (0: pitch-linear, 1: block-linear, as a game's
        /// images are) and its block height (log2 of GOBs).
        pub const SNAPSHOT: u32 = 3;
        /// A game takes the screen (the UI stops): `SCANOUT_BUFFERS`
        /// block-linear images for the GPU to draw on, `CONTIGUOUS |
        /// DMA32 | UNCACHED`, handles 0-2 with the `DMA` and `MAP` rights.
        /// Reply: words = width, height (of the panel as it scans out),
        /// format (`FORMAT_RGBA8888`), block height (log2 of GOBs),
        /// bytes per image; the first image is on display. `ENODEV`: not
        /// started by `display`, or the screen is taken already.
        pub const SCANOUT: u32 = 4;
        /// Show image word 0 from the next vertical blank; the reply
        /// comes once it is on display.
        pub const FLIP: u32 = 5;
        /// The game is done: the UI comes back (also when it exits).
        pub const RELEASE: u32 = 6;
        /// The next input event routed to the game (`input::READ`'s
        /// reply: kind, code, value, x, y, source; raw touch coordinates), waiting
        /// up to word 0 ns (0: no limit). While a game has the screen the
        /// UI reads none.
        pub const EVENT: u32 = 7;
        pub const SCANOUT_BUFFERS: usize = 3;
        pub const FORMAT_RGBA8888: u64 = 1;
    }
    /// USB-C console (`usbcon`), published as "usbcon".
    pub mod usbcon {
        /// Reply: word 0 = state (`ST_*` bits), 1 = bytes typed on the
        /// host, 2 = bytes sent to it, 3 = transfers received, 4 =
        /// transfers sent, 5 = bus resets (low 32 bits) and transfer
        /// errors (high 32).
        pub const STATS: u32 = 1;
        /// A host reset the bus at least once: a PC sees the Switch.
        pub const ST_HOST: u64 = 1;
        pub const ST_CONFIGURED: u64 = 2;
        pub const ST_HIGH_SPEED: u64 = 4;
        /// Terminal open on the host (configured, DTR up).
        pub const ST_TERMINAL: u64 = 8;
        /// The host turned the network function on (CDC-ECM data
        /// interface, alternate setting 1): link `usb0` has a carrier.
        pub const ST_NET: u64 = 16;
        /// One endpoint, two names: "usbcon" (`STATS`) and "usb0", the
        /// network function as an `eth` link.
        pub const BADGE_CTRL: u64 = 1;
        pub const BADGE_LINK: u64 = 2;
    }
    /// Ethernet interface (link layer).
    pub mod eth {
        /// Reply: word 0 = MAC (low 6 bytes, byte 0 first), word 1 = MTU,
        /// word 2 = carrier (1: up; absent: up while published).
        pub const INFO: u32 = 1;
        /// Frame to send (see `pack`).
        pub const SEND: u32 = 2;
        /// Wait for a frame (word 0 = timeout ns, 0: no limit).
        /// `-ENETDOWN`: the link went down since (a Wi-Fi join lost),
        /// the next call waits for frames again.
        pub const RECV: u32 = 3;
    }
    /// Wi-Fi station control (`wlan` driver). The link itself is the
    /// `eth` protocol published as "wlan0".
    pub mod wifi {
        /// Look for networks. word 0 = timeout ns (0: default).
        /// Reply: word 0 = network count, then bytes (see `pack_at`):
        /// that many records, each `RECORD` bytes plus its SSID.
        pub const SCAN: u32 = 1;
        /// Join a network. word 0 = timeout ns (0: default); bytes =
        /// SSID, a 0 byte, then the passphrase (empty: open network).
        /// Replies once associated, or with the failure.
        pub const CONNECT: u32 = 2;
        /// Leave the current network.
        pub const DISCONNECT: u32 = 3;
        /// Reply: words = state (`ST_*`), channel, signal (-dBm, 0 =
        /// unknown), MAC (low 6 bytes); bytes = current SSID.
        pub const STATUS: u32 = 4;
        /// What the adapter is, or why there is none: a line of text
        /// naming the step that failed during bring-up. Reply: bytes.
        pub const DIAG: u32 = 5;
        /// Run the hardware bring-up again and report what happened:
        /// a boot-time race and a dead register read look the same in
        /// the log, not here. With an adapter up, a recovery instead
        /// (module reset, firmware, rejoin), as after a chip loss.
        /// Reply: bytes, like DIAG.
        pub const RETRY: u32 = 6;
        /// Flight mode: word 0 = 0 turns the radio off (leave the
        /// network, then the chip's `WLC_DOWN`, so it neither scans nor
        /// joins until turned on), 1 turns it on (`WLC_UP`, nothing is
        /// joined by itself). Kept across a chip recovery. Reply: word 0 =
        /// 1 if the radio is on.
        pub const RADIO: u32 = 7;
        /// The radio is off (`RADIO`).
        pub const ST_RADIO_OFF: u64 = 5;
        /// No adapter: no PCIe device, or its firmware is missing.
        pub const ST_DOWN: u64 = 0;
        pub const ST_IDLE: u64 = 1;
        pub const ST_SCANNING: u64 = 2;
        pub const ST_CONNECTING: u64 = 3;
        pub const ST_CONNECTED: u64 = 4;
        /// Scan record head: SSID length, channel, security (`SEC_*`),
        /// signal (-dBm), 6 BSSID bytes; the SSID follows.
        pub const RECORD: usize = 10;
        pub const SEC_OPEN: u8 = 0;
        pub const SEC_WEP: u8 = 1;
        pub const SEC_WPA: u8 = 2;
        pub const SEC_WPA2: u8 = 3;
        pub const SEC_WPA3: u8 = 4;
        /// Longest SSID (802.11).
        pub const MAX_SSID: usize = 32;
        /// One endpoint, two names: the badge tells the link apart from
        /// the control channel (the labels of both protocols overlap).
        pub const BADGE_LINK: u64 = 1;
        pub const BADGE_CTRL: u64 = 2;
    }
    /// Joy-Cons on the rails (`joycon`), published as "joycon".
    pub mod joycon {
        /// Reply: words = left state, right state (`ST_*`), left
        /// battery, right battery (0..4 or `BATTERY_UNKNOWN`).
        pub const STATUS: u32 = 1;
        pub const ST_ABSENT: u64 = 0;
        /// Attached, handshake not done (or failing).
        pub const ST_ATTACHED: u64 = 1;
        pub const ST_READY: u64 = 2;
        pub const BATTERY_UNKNOWN: u64 = 0xFF;
    }
    /// Bluetooth controllers (`bluetooth`), published as "bt": the
    /// BCM4356's Bluetooth half on UART-D, Joy-Cons and Pro Controllers
    /// paired with their sync button. Addresses are the 6 bytes of a
    /// BD_ADDR in a word, the first byte printed (most significant) in
    /// bits 40-47.
    pub mod bt {
        /// Reply: words = state (`ST_*`), controllers connected,
        /// controllers paired, the adapter's address; bytes = what the
        /// adapter is (chip, firmware), or the step where bring-up stopped.
        pub const STATUS: u32 = 1;
        /// Look for controllers in pairing mode (sync button held) for
        /// word 0 ns (0: `PAIR_DEFAULT_NS`; with word 1 = 0, a window of
        /// 0 ends pairing). Word 1 = 1: reply once one is paired (word 0 =
        /// its address, bytes = its name) or with `ETIMEDOUT`; 0: reply at
        /// once, `LIST` tells what was paired.
        pub const PAIR: u32 = 2;
        /// Paired controllers. Reply: word 0 = count; bytes = that many
        /// records: `RECORD` bytes (address, `KIND_*`, connected, battery
        /// 0..4 or `BATTERY_UNKNOWN`, name length) then the name.
        pub const LIST: u32 = 3;
        /// Forget the controller at address word 0 (0: all of them): its
        /// link key is dropped, a connection closed.
        pub const FORGET: u32 = 4;
        /// Run the bring-up again (power cycle, firmware). Reply as STATUS.
        pub const RETRY: u32 = 5;
        pub const PAIR_DEFAULT_NS: u64 = 30_000_000_000;
        /// No adapter (no answer on UART-D, or bring-up failed).
        pub const ST_DOWN: u64 = 0;
        pub const ST_STARTING: u64 = 1;
        pub const ST_READY: u64 = 2;
        pub const ST_PAIRING: u64 = 3;
        pub const RECORD: usize = 10;
        pub const KIND_UNKNOWN: u8 = 0;
        pub const KIND_JOYCON_L: u8 = 1;
        pub const KIND_JOYCON_R: u8 = 2;
        pub const KIND_PRO: u8 = 3;
        pub const BATTERY_UNKNOWN: u8 = 0xFF;
        /// Longest name kept (Bluetooth names go up to 248 bytes).
        pub const MAX_NAME: usize = 32;
    }
    /// TCP/IP stack (IPv4). 32-bit addresses, `a.b.c.d` = `a << 24 | ...`
    /// (like `u32::from(Ipv4Addr)`). Timeouts in ns (0: no limit).
    pub mod net {
        /// words: address, port, timeout. Reply: word 0 = socket.
        pub const TCP_CONNECT: u32 = 1;
        /// words: port, backlog. Reply: word 0 = listen socket.
        pub const TCP_LISTEN: u32 = 2;
        /// words: listen socket, timeout. Reply: socket, address, port.
        pub const TCP_ACCEPT: u32 = 3;
        /// words: socket, timeout; bytes. Reply: word 0 = bytes accepted.
        pub const SEND: u32 = 4;
        /// words: socket, max length, timeout. Reply: bytes (none:
        /// end of stream).
        pub const RECV: u32 = 5;
        /// words: socket.
        pub const CLOSE: u32 = 6;
        /// words: socket. End sending (FIN), receiving continues.
        pub const SHUTDOWN: u32 = 7;
        /// words: port (0: ephemeral). Reply: word 0 = UDP socket.
        pub const UDP_BIND: u32 = 8;
        /// words: socket, address, port; bytes.
        pub const SEND_TO: u32 = 9;
        /// words: socket, timeout. Reply: words address, port; bytes.
        pub const RECV_FROM: u32 = 10;
        /// bytes = hostname. Reply: word 0 = address.
        pub const RESOLVE: u32 = 11;
        /// Reply: address, prefix length, gateway, DNS server.
        pub const INFO: u32 = 12;
        /// words: socket. Reply: local address and port, then remote.
        pub const ADDRS: u32 = 13;
        /// words: socket. Extra reference to the same socket (same
        /// id); CLOSE only closes it at the last one.
        pub const DUP: u32 = 14;
        /// words: address, timeout. One ICMP echo request. Reply: word
        /// 0 = round-trip time in ns.
        pub const PING: u32 = 15;
        /// Reply: frames received, bytes received, frames sent, bytes
        /// sent, open sockets, link (`LINK_*`).
        pub const STATS: u32 = 16;
        pub const LINK_ETH0: u64 = 1;
        pub const LINK_WLAN0: u64 = 2;
        /// The USB-C port's network function (`usbcon`), shared by a PC.
        pub const LINK_USB0: u64 = 3;
    }
    /// Input events.
    pub mod input {
        /// Wait for the next event. Reply: kind, code, value, x, y, source
        /// (`SRC_*`: the controller it came from).
        /// Request word 0: timeout ns (0: no limit); word 1: kinds
        /// wanted, `1 << KIND_*` each (0 or absent: all).
        pub const READ: u32 = 1;
        /// Event from another driver (USB keyboard...): same words; a
        /// sixth word missing is `SRC_NONE`.
        pub const INJECT: u32 = 2;
        /// Source word: not a controller (touch, keyboard, the console's
        /// own buttons, the debug channel).
        pub const SRC_NONE: u64 = 0;
        /// The Joy-Cons on the rails.
        pub const SRC_RAIL_L: u64 = 1;
        pub const SRC_RAIL_R: u64 = 2;
        /// A Bluetooth controller: `SRC_BT` | its address (`bt` word).
        pub const SRC_BT: u64 = 1 << 63;
        /// Words in an event.
        pub const WORDS: usize = 6;
        pub const KIND_KEY: u64 = 1;
        pub const KIND_TOUCH: u64 = 2;
        pub const KIND_BUTTON: u64 = 3;
        /// Analog stick: code = `STICK_L` or `STICK_R`, value = 0,
        /// x and y = signed positions (two's complement in the word),
        /// -`STICK_MAX`..=`STICK_MAX`, right and up positive, inside the
        /// driver's dead zone 0. Sent when a position moved.
        pub const KIND_STICK: u64 = 4;
        pub const STICK_L: u64 = 1;
        pub const STICK_R: u64 = 2;
        pub const STICK_MAX: i64 = 32767;
        /// Touch: code = press (1), move (2), release (3);
        /// value = finger index.
        pub const TOUCH_DOWN: u64 = 1;
        pub const TOUCH_MOVE: u64 = 2;
        pub const TOUCH_UP: u64 = 3;
        /// Key: code = HID code (keyboard page), value = 1 press,
        /// 0 release; x = Unicode char produced (0: none).
        pub const KEY_PRESSED: u64 = 1;
        /// Button (gamepad): code = `BTN_*`, value = 1 press, 0 release.
        pub const BTN_A: u64 = 1;
        pub const BTN_B: u64 = 2;
        pub const BTN_X: u64 = 3;
        pub const BTN_Y: u64 = 4;
        pub const BTN_L: u64 = 5;
        pub const BTN_R: u64 = 6;
        pub const BTN_ZL: u64 = 7;
        pub const BTN_ZR: u64 = 8;
        pub const BTN_PLUS: u64 = 9;
        pub const BTN_MINUS: u64 = 10;
        pub const BTN_UP: u64 = 11;
        pub const BTN_DOWN: u64 = 12;
        pub const BTN_LEFT: u64 = 13;
        pub const BTN_RIGHT: u64 = 14;
        pub const BTN_LSTICK: u64 = 15;
        pub const BTN_RSTICK: u64 = 16;
        pub const BTN_HOME: u64 = 17;
        pub const BTN_CAPTURE: u64 = 18;
        /// The console's own volume buttons (+ and -); a press repeats
        /// while held. `display` takes them: they never reach a game.
        pub const BTN_VOL_UP: u64 = 19;
        pub const BTN_VOL_DOWN: u64 = 20;
        /// The Power button (PMIC), sent with value 1 only: `BTN_POWER`
        /// when released after a short press, `BTN_POWER_LONG` once it
        /// has been held `POWER_LONG_MS`. `display` takes them too.
        pub const BTN_POWER: u64 = 21;
        pub const BTN_POWER_LONG: u64 = 22;
        pub const POWER_LONG_MS: u64 = 3000;
        /// Raw touch coordinates (hekate bdk/input/touch.c): landscape
        /// 1280x720 with the edges cut.
        pub const TOUCH_EDGE: u64 = 15;
        pub const TOUCH_X_MAX: u64 = 1264;
        pub const TOUCH_Y_MAX: u64 = 704;
    }
    /// Debug channel on the USB-C console (`usbcon`, `tools/nxdbg.py`):
    /// frames inside the serial stream, which usbcon takes out of the
    /// shell's input and puts between its output. A frame is an APC
    /// string, `FRAME_START`, the base64 of a payload, `FRAME_END`:
    /// terminals ignore it, the console never makes one.
    pub mod dbg {
        pub const FRAME_START: &[u8] = b"\x1b_N";
        pub const FRAME_END: &[u8] = b"\x1b\\";
        /// Payload of a request: id, op, then the op's bytes (little-endian).
        /// Payload of a reply: the id, a status, then data.
        /// `PING`: no bytes. Reply: protocol version.
        pub const PING: u8 = 0;
        /// `SCREEN`: no bytes. Reply: the image on display, in chunks
        /// (`MORE`) until `DONE`; concatenated they are width (u16), height
        /// (u16), `display::FORMAT_*` (u8), layout (u8: 0 pitch-linear, 1
        /// block-linear) and block height log2 (u8), then the pixels (the
        /// image in memory order, 32 bits each) as tokens: u16 n, then bit 15
        /// clear: n + 1 pixels follow; set: one pixel follows, repeated
        /// (n & 0x7FFF) + 1 times.
        pub const SCREEN: u8 = 1;
        /// `TOUCH`: action (`input::TOUCH_*`, u8), finger (u8), x (u16), y
        /// (u16), landscape pixels of the image.
        pub const TOUCH: u8 = 2;
        /// `BUTTON`: `input::BTN_*` (u8), 1 press or 0 release (u8).
        pub const BUTTON: u8 = 3;
        pub const DONE: u8 = 0;
        pub const MORE: u8 = 1;
        /// Data: the error number (u16, positive).
        pub const ERROR: u8 = 2;
        pub const VERSION: u8 = 1;
    }
}

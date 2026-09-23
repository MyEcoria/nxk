//! nxk ABI: syscalls, errors, rights, layout, formats.
//! Linux arm64 convention (`svc #0`); IPC uses `x1`-`x7`, returns badge/token/pid.

#![no_std]

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
    /// Terminal driver: console handle with `console::BADGE_TERMINAL`.
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
        /// Terminal driver only: next console output (echo included),
        /// deferred until there is some. Reply shaped like WRITE.
        pub const OUTPUT: u32 = 4;
        /// Badge of the console handle init gives a terminal driver.
        pub const BADGE_TERMINAL: u64 = 1;
        /// Badge of all other console handles: a badged handle cannot
        /// be re-minted, so holders cannot forge `BADGE_TERMINAL`.
        pub const BADGE_CLIENT: u64 = 2;
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
    }
    /// I2C bus.
    pub mod i2c {
        /// word 0 = bus (1..6) | address << 8 | read bytes << 16;
        /// write bytes follow (see `pack`). Reply: bytes read.
        pub const TRANSFER: u32 = 1;
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
    }
    /// Block device (SD card, eMMC).
    pub mod blk {
        /// Reply: word 0 = block size, word 1 = block count.
        pub const INFO: u32 = 1;
        /// handle 0 = shared memory used as transfer buffer.
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
        pub const FORMAT_RGBA8888: u64 = 1;
    }
    /// Ethernet interface (link layer).
    pub mod eth {
        /// Reply: word 0 = MAC (low 6 bytes, byte 0 first), word 1 = MTU.
        pub const INFO: u32 = 1;
        /// Frame to send (see `pack`).
        pub const SEND: u32 = 2;
        /// Wait for a frame (word 0 = timeout ns, 0: no limit).
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
        /// the log, not here. Reply: bytes, like DIAG.
        pub const RETRY: u32 = 6;
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
    }
    /// Input events.
    pub mod input {
        /// Wait for the next event. Reply: kind, code, value, x, y.
        /// Request word 0: timeout ns (0: no limit).
        pub const READ: u32 = 1;
        /// Event from another driver (USB keyboard...): same words.
        pub const INJECT: u32 = 2;
        pub const KIND_KEY: u64 = 1;
        pub const KIND_TOUCH: u64 = 2;
        pub const KIND_BUTTON: u64 = 3;
        /// Touch: code = press (1), move (2), release (3);
        /// value = finger index.
        pub const TOUCH_DOWN: u64 = 1;
        pub const TOUCH_MOVE: u64 = 2;
        pub const TOUCH_UP: u64 = 3;
        /// Key: code = HID code (keyboard page), value = 1 press,
        /// 0 release; x = Unicode char produced (0: none).
        pub const KEY_PRESSED: u64 = 1;
    }
}

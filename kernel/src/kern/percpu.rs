//! Per-core data, reached via `TPIDR_EL1`.

use core::sync::atomic::AtomicBool;

use crate::arch;
use crate::kern::sched::RunQueue;
use crate::kern::thread::NIL;
use crate::{sysreg_read, sysreg_write};

pub const NR_CPUS: usize = 4;

/// Activity counters. Durations are counter units, converted to ns on read.
#[derive(Default, Clone, Copy)]
pub struct CpuStats {
    pub switches: u64,
    pub syscalls: u64,
    pub irqs: u64,
    pub idle_time: u64,
    pub busy_time: u64,
    pub timer_late: u64,
    pub empty_wakeups: u64,
    /// Overdue deadlines found by another interrupt (`timer::catch_up`).
    pub catch_ups: u64,
}

#[repr(C, align(64))]
pub struct PerCpu {
    /// Core kernel-stack top. Offset 0: read by asm on each EL0 entry.
    pub kstack_top: u64,
    pub id: usize,
    /// Logically current thread (`NIL` = idle core).
    pub current: u16,
    /// Last thread returned to EL0: its UTCB and registers are in place.
    pub last_run: u16,
    /// Current owner of the core FP/SIMD registers.
    pub fp_owner: u16,
    /// Deadlines (sleepers, bounded waits), sorted ascending.
    pub tmo_head: u16,
    /// Threads for this core to migrate (it may hold their FP).
    pub mig_head: u16,
    /// Thread whose runtime is being accounted.
    pub acct_thread: u16,
    pub fp_enabled: bool,
    pub need_resched: bool,
    /// Current thread's quantum is spent.
    pub slice_expired: bool,
    /// Deadlines changed: the timer needs reprogramming.
    pub timer_dirty: bool,
    /// Idle core waits to be handed a ready thread.
    pub push_pending: bool,
    /// MDSCR_EL1.SS set (current thread single-step).
    pub ss_enabled: bool,
    pub online: bool,
    /// Asked to power off: it hands its threads over at its next kernel exit.
    pub going_down: bool,
    /// TTBR0_EL1 value loaded (avoids redundant writes).
    pub cur_ttbr: u64,
    /// Running quantum end (0: no contender, no quantum).
    pub slice_end: u64,
    /// Deadline in the timer (`u64::MAX`: timer stopped).
    pub timer_cval: u64,
    /// Start of the interval being accounted.
    pub acct_start: u64,
    pub rq: RunQueue,
    pub stats: CpuStats,
    /// Idle-poll mode: the core polls this flag instead of sleeping.
    pub polling: AtomicBool,
    pub wake: AtomicBool,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            kstack_top: 0,
            id: 0,
            current: NIL,
            last_run: NIL,
            fp_owner: NIL,
            tmo_head: NIL,
            mig_head: NIL,
            acct_thread: NIL,
            fp_enabled: false,
            need_resched: false,
            slice_expired: false,
            timer_dirty: false,
            push_pending: false,
            ss_enabled: false,
            online: false,
            going_down: false,
            cur_ttbr: 0,
            slice_end: 0,
            timer_cval: u64::MAX,
            acct_start: 0,
            rq: RunQueue::new(),
            stats: CpuStats {
                switches: 0,
                syscalls: 0,
                irqs: 0,
                idle_time: 0,
                busy_time: 0,
                timer_late: 0,
                empty_wakeups: 0,
                catch_ups: 0,
            },
            polling: AtomicBool::new(false),
            wake: AtomicBool::new(false),
        }
    }
}

static mut PERCPU: [PerCpu; NR_CPUS] = [const { PerCpu::new() }; NR_CPUS];

/// Current core, without reading MPIDR or indexing: one `mrs` is enough.
#[inline(always)]
pub fn this() -> &'static mut PerCpu {
    let p = sysreg_read!("tpidr_el1") as *mut PerCpu;
    unsafe { &mut *p }
}

#[inline(always)]
pub fn get(id: usize) -> &'static mut PerCpu {
    unsafe { &mut (*core::ptr::addr_of_mut!(PERCPU))[id] }
}

/// Init the current core's data and publish its address.
pub fn init(id: usize, kstack_top: u64, kernel_ttbr: u64) {
    let c = get(id);
    c.id = id;
    c.kstack_top = kstack_top;
    c.cur_ttbr = kernel_ttbr;
    c.acct_start = crate::arch::timer::counter();
    c.online = true;
    sysreg_write!("tpidr_el1", c as *mut PerCpu as u64);
    arch::isb();
}

//! ARM generic timer: deadlines only, no periodic tick.
//! Counter precision (52 ns at 19.2 MHz), not a tick.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use nxk_abi::ticks;

use crate::arch::{self, gic};
use crate::kern::percpu::{self, PerCpu};
use crate::kern::sched;
use crate::kern::thread::{self, NIL};
use crate::{sysreg_read, sysreg_write};

const CNTP_CTL_ENABLE: u64 = 1 << 0;

static FREQ_HZ: AtomicU64 = AtomicU64::new(0);
/// `nxk_abi::ticks` multipliers, set with the frequency.
static NS_MULT: AtomicU64 = AtomicU64::new(0);
static TICK_MULT: AtomicU64 = AtomicU64::new(0);
/// Wall time = monotonic time + offset (ns), set by the clock service
/// from the RTC.
static REALTIME_OFFSET: AtomicI64 = AtomicI64::new(0);

#[inline(always)]
pub fn counter() -> u64 {
    arch::isb();
    sysreg_read!("cntpct_el0")
}

pub fn freq_hz() -> u64 {
    FREQ_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn ns_to_ticks(ns: u64) -> u64 {
    ticks::scale(ns, TICK_MULT.load(Ordering::Relaxed))
}

#[inline]
pub fn ticks_to_ns(t: u64) -> u64 {
    ticks::scale(t, NS_MULT.load(Ordering::Relaxed))
}

pub fn monotonic_ns() -> u64 {
    ticks_to_ns(counter())
}

pub fn realtime_ns() -> u64 {
    (monotonic_ns() as i64).wrapping_add(REALTIME_OFFSET.load(Ordering::Relaxed)) as u64
}

pub fn set_realtime(ns: u64) {
    REALTIME_OFFSET.store(
        (ns as i64).wrapping_sub(monotonic_ns() as i64),
        Ordering::Relaxed,
    );
}

/// Counter frequency, set once at boot.
pub fn init_global() {
    let freq = sysreg_read!("cntfrq_el0");
    let freq = if freq == 0 { 19_200_000 } else { freq };
    FREQ_HZ.store(freq, Ordering::Relaxed);
    NS_MULT.store(ticks::ns_mult(freq), Ordering::Relaxed);
    TICK_MULT.store(ticks::tick_mult(freq), Ordering::Relaxed);
}

/// Set up the current core's timer (stopped until a deadline exists).
pub fn init_cpu() {
    sysreg_write!("cntp_ctl_el0", 0);
    percpu::this().timer_cval = u64::MAX;
    gic::set_priority(gic::IRQ_TIMER, 0x80);
    gic::enable_irq(gic::IRQ_TIMER);
    arch::isb();
}

/// Program the current core's nearest deadline, or stop the timer.
pub fn reprogram(c: &mut PerCpu) {
    c.timer_dirty = false;
    let mut d = u64::MAX;
    if c.slice_end != 0 {
        d = c.slice_end;
    }
    if c.tmo_head != NIL {
        d = d.min(thread::get(c.tmo_head).wake_at);
    }
    if d == c.timer_cval {
        return;
    }
    crate::trace!("[s{}] timer {} (head {})", c.id, d, c.tmo_head);
    c.timer_cval = d;
    // Drive output to 0 first for a rising edge; an edge-triggered GIC
    // (all PPIs under the emulator) could otherwise lose one.
    sysreg_write!("cntp_ctl_el0", 0);
    if d != u64::MAX {
        arch::isb();
        sysreg_write!("cntp_cval_el0", d);
        sysreg_write!("cntp_ctl_el0", CNTP_CTL_ENABLE);
    }
}

/// Tegra TMR8 (hekate bdk/soc/timer.h, its SPI 154): wakes a core powered
/// down by CPU_SUSPEND, whose own timer is gone. 1 us ticks.
const TMR8_PTV: usize = 0x6000_5078;
const TMR8_PCR: usize = 0x6000_507C;
const TMR_EN: u32 = 1 << 31;
const TMR_INTR_CLR: u32 = 1 << 30;
pub const IRQ_WAKE_TMR: u32 = 32 + 154;
/// TMR9 (SPI 155) on core 1: checks the suspended core came back.
const TMR9_PTV: usize = 0x6000_5080;
const TMR9_PCR: usize = 0x6000_5084;
pub const IRQ_WAKE_CHECK: u32 = 32 + 155;
/// After the wake timer, how long a missing wake is waited for (us).
const WAKE_MARGIN_US: u32 = 50_000;

/// One-shot wake interrupt for `core` in `us` microseconds.
pub fn arm_wake(core: usize, us: u32) {
    unsafe {
        crate::mmio::write32(TMR8_PCR, TMR_INTR_CLR);
        crate::mmio::write32(TMR8_PTV, TMR_EN | us.clamp(1, 0x1FFF_FFFF));
    }
    gic::set_target(IRQ_WAKE_TMR, 1 << core);
    gic::set_priority(IRQ_WAKE_TMR, 0xA0);
    gic::enable_irq(IRQ_WAKE_TMR);
    // Safety net on another core: no wake interrupt, no core back.
    let checker = (0..percpu::NR_CPUS)
        .find(|&k| k != core && percpu::get(k).online)
        .unwrap_or(core);
    unsafe {
        crate::mmio::write32(TMR9_PCR, TMR_INTR_CLR);
        crate::mmio::write32(
            TMR9_PTV,
            TMR_EN | (us.clamp(1, 0x1FFF_0000) + WAKE_MARGIN_US),
        );
    }
    WAKE_CORE.store(core as u64, Ordering::Relaxed);
    gic::set_target(IRQ_WAKE_CHECK, 1 << checker);
    gic::set_priority(IRQ_WAKE_CHECK, 0xA0);
    gic::enable_irq(IRQ_WAKE_CHECK);
}

static WAKE_CORE: AtomicU64 = AtomicU64::new(0);

/// The safety-net timer fired: a core still suspended is powered up by the
/// monitor (the flow controller), and that is logged.
pub fn wake_check_irq() {
    unsafe {
        crate::mmio::write32(TMR9_PTV, 0);
        crate::mmio::write32(TMR9_PCR, TMR_INTR_CLR);
    }
    let core = WAKE_CORE.load(Ordering::Relaxed) as usize;
    if percpu::get(core).suspended {
        let r = crate::arch::psci::nxk_wake(core as u64);
        crate::kprintln!(
            "[smp] core {}: no wake interrupt, monitor wakes it ({})",
            core,
            r
        );
    }
}

/// The wake timer fired: stop and clear it.
pub fn wake_irq() {
    unsafe {
        crate::mmio::write32(TMR8_PTV, 0);
        crate::mmio::write32(TMR8_PCR, TMR_INTR_CLR);
    }
}

/// Overdue by more than this, a deadline's timer interrupt went missing.
const CATCH_UP_NS: u64 = 50_000;

/// Deadlines already due but not expired (their timer edge was lost): handle
/// them as the timer interrupt would.
pub fn catch_up() {
    let c = percpu::this();
    if c.tmo_head == NIL {
        return;
    }
    let (due, now) = (thread::get(c.tmo_head).wake_at, counter());
    if due <= now {
        // Only a real miss counts, not a deadline that came due during
        // this interrupt's own handling (its timer interrupt is on its way).
        if now - due > ns_to_ticks(CATCH_UP_NS) {
            c.stats.catch_ups += 1;
        }
        on_irq();
    }
}

/// Timer interrupt: quantum end and/or reached deadlines.
pub fn on_irq() {
    let c = percpu::this();
    let now = counter();
    crate::trace!("[s{}] timer irq now {} cval {}", c.id, now, c.timer_cval);
    if now > c.timer_cval {
        c.stats.timer_late += now - c.timer_cval;
    }
    if c.slice_end != 0 && now >= c.slice_end {
        c.slice_end = 0;
        c.slice_expired = true;
        c.need_resched = true;
    }
    sched::expire(now);
    // A reached deadline keeps the line asserted: change it before returning.
    c.timer_cval = 0;
    reprogram(c);
}

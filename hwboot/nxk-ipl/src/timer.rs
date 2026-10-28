//! Time (hekate bdk/soc/timer.c, built with USE_RTC_TIMER): delays poll
//! the microsecond counter, milliseconds the RTC; the BPMP's timed halts
//! in the flow controller only cover a counter about to wrap.

use crate::hdr::{APBDEV_RTC_MILLI_SECONDS, APBDEV_RTC_SHADOW_SECONDS};
use crate::reg::{rd, wr, FLOW_CTLR, RTC, TMR};

const TIMERUS_CNTR_1US: u32 = TMR + 0x10;
const FLOW_CTLR_HALT_COP_EVENTS: u32 = FLOW_CTLR + 0x4;
pub const HALT_MSEC: u32 = 1 << 24;
pub const HALT_USEC: u32 = 1 << 25;
pub const HALT_JTAG: u32 = 1 << 28;
pub const HALT_MODE_WAITEVENT: u32 = 2 << 29;
const HALT_MAX_CNT: u32 = 0xFF;

pub fn now_us() -> u32 {
    rd(TIMERUS_CNTR_1US)
}

/// `get_tmr_ms`: the RTC, milliseconds read first (it latches the seconds).
pub fn now_ms() -> u32 {
    let ms = rd(RTC + APBDEV_RTC_MILLI_SECONDS);
    ms.wrapping_add(rd(RTC + APBDEV_RTC_SHADOW_SECONDS).wrapping_mul(1000))
}

/// `usleep`.
pub fn usleep(us: u32) {
    let start = now_us();
    if start.wrapping_add(us) < start {
        bpmp_usleep(us);
    } else {
        while now_us().wrapping_sub(start) <= us {}
    }
}

/// `msleep`.
pub fn msleep(ms: u32) {
    let start = now_ms();
    while now_ms().wrapping_sub(start) <= ms {}
}

/// `bpmp_usleep`.
pub fn bpmp_usleep(mut us: u32) {
    while us > 0 {
        let d = us.min(HALT_MAX_CNT);
        us -= d;
        wr(FLOW_CTLR_HALT_COP_EVENTS, HALT_MODE_WAITEVENT | HALT_USEC | d);
    }
}

/// `bpmp_msleep`.
pub fn bpmp_msleep(mut ms: u32) {
    while ms > 0 {
        let d = ms.min(HALT_MAX_CNT);
        ms -= d;
        wr(FLOW_CTLR_HALT_COP_EVENTS, HALT_MODE_WAITEVENT | HALT_MSEC | d);
    }
}

/// `watchdog_start(us, TIMER_FIQENABL_EN)`: WDT4, the BPMP's, on timer 9;
/// it raises a FIQ when it expires (main.rs `_fiq_handler`).
pub fn watchdog_start(us: u32) {
    use crate::hdr::*;
    wr(TMR + TIMER_WDT4_UNLOCK_PATTERN, TIMER_MAGIC_PTRN);
    wr(TMR + TIMER_TMR9_TMR_PTV, TIMER_EN | TIMER_PER_EN | us);
    wr(TMR + TIMER_WDT4_CONFIG, 9 | 1 << 4 | TIMER_FIQENABL_EN); // TIMER_SRC(9) | TIMER_PER(1)
    wr(TMR + TIMER_WDT4_COMMAND, TIMER_START_CNT);
}

/// `watchdog_end` (the FIQ handler repeats it in assembly).
pub fn watchdog_end() {
    use crate::hdr::*;
    wr(TMR + TIMER_TMR9_TMR_PTV, 0);
    wr(TMR + TIMER_WDT4_UNLOCK_PATTERN, TIMER_MAGIC_PTRN);
    wr(TMR + TIMER_WDT4_COMMAND, TIMER_START_CNT); // re-armed to clear its interrupts
    wr(TMR + TIMER_WDT4_COMMAND, TIMER_CNT_DISABLE);
    wr(TMR + TIMER_TMR9_TMR_PCR, TIMER_INTR_CLR);
}

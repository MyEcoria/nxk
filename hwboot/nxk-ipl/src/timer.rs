//! Time: the microsecond counter and the BPMP's timed halts (hekate
//! bdk/soc/timer.c and bpmp.c without USE_RTC_TIMER: delays halt the BPMP
//! in the flow controller, so they show in a register trace).

use crate::reg::{rd, wr, FLOW_CTLR, TMR};

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

/// `bpmp_usleep`.
pub fn usleep(mut us: u32) {
    while us > 0 {
        let d = us.min(HALT_MAX_CNT);
        us -= d;
        wr(FLOW_CTLR_HALT_COP_EVENTS, HALT_MODE_WAITEVENT | HALT_USEC | d);
    }
}

/// `bpmp_msleep`.
pub fn msleep(mut ms: u32) {
    while ms > 0 {
        let d = ms.min(HALT_MAX_CNT);
        ms -= d;
        wr(FLOW_CTLR_HALT_COP_EVENTS, HALT_MODE_WAITEVENT | HALT_MSEC | d);
    }
}

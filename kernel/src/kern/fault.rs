//! User program faults: the faulty process dies, the rest survives.

use nxk_abi::debug;

use crate::kern::{process, thread};
use crate::kprintln;

/// Exit code of a fault-killed process (like SIGSEGV on Linux).
pub const EXIT_FAULT: i64 = 139;

fn describe(ec: u64) -> &'static str {
    match ec {
        0x00 => "undefined instruction",
        0x0E => "illegal execution state",
        0x18 => "forbidden system register access",
        0x20 => "instruction fault",
        0x22 => "unaligned PC",
        0x24 => "data fault",
        0x26 => "unaligned SP",
        0x2F => "system error (SError)",
        0x32 => "single-step",
        0x3C => "breakpoint (BRK)",
        _ => "unexpected exception",
    }
}

pub fn user_fault(ti: u16, esr: u64, far: u64) {
    let t = thread::get(ti);
    let pi = t.proc_idx;
    let ec = esr >> 26;
    // Translation fault (levels 0-3) on a reserved page: populate it and
    // re-execute the instruction on return.
    if matches!(ec, 0x20 | 0x24)
        && (0x04..=0x07).contains(&(esr & 0x3F))
        && process::populate(pi, far)
    {
        return;
    }
    // Debugged process: the event goes to the debugger instead of killing.
    let kind = match ec {
        0x3C => debug::KIND_BREAK,
        0x32 => {
            t.single_step = false;
            t.frame.spsr &= !crate::kern::debug::SPSR_SS;
            debug::KIND_STEP
        }
        _ => debug::KIND_FAULT,
    };
    if crate::kern::debug::event(ti, kind, esr, far) {
        return;
    }
    if ec == 0x32 {
        return;
    }
    let base = process::get(pi).load_base;
    let off = t.frame.elr.wrapping_sub(base);
    // Data fault: write or read, and kind (translation: nothing mapped;
    // permission: protected page).
    let detail = if ec == 0x24 {
        let dfsc = esr & 0x3F;
        let what = match dfsc {
            0x04..=0x07 => "missing page",
            0x09..=0x0B => "access flag",
            0x0D..=0x0F => "permission",
            0x21 => "alignment",
            _ => "other",
        };
        if esr & (1 << 6) != 0 {
            (" (write, ", what)
        } else {
            (" (read, ", what)
        }
    } else {
        ("", "")
    };
    kprintln!(
        "[nxk] '{}' (pid {}, thread {}) : {} at {:#x}{}{}{}, pc {:#x} (+{:#x}), sp {:#x}, lr +{:#x} -> process killed",
        process::name(pi),
        process::get(pi).pid,
        t.tid,
        describe(ec),
        far,
        detail.0,
        detail.1,
        if detail.0.is_empty() { "" } else { ")" },
        t.frame.elr,
        off,
        t.frame.sp,
        t.frame.x[30].wrapping_sub(base)
    );
    process::kill(pi, EXIT_FAULT);
}

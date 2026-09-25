//! Hardware SPI dispatch to user drivers via bound notifications.
//! Kernel masks at GIC on fire; driver unmasks with `irq_ack`.

use nxk_abi::err::{EBUSY, EINVAL};

use crate::arch::{gic, timer};
use crate::kern::bkl::BklCell;
use crate::kern::thread::NIL;
use crate::kern::{notify, percpu};

pub const MAX_IRQ: usize = 256;

#[derive(Clone, Copy)]
pub struct Line {
    pub owned: bool,
    /// Capabilities naming the line; freed on last close.
    pub refs: u32,
    pub owner: u16,
    pub notif: u16,
    pub notif_gen: u16,
    pub bits: u64,
}

static LINES: BklCell<[Line; MAX_IRQ]> = BklCell::new(
    [Line {
        owned: false,
        refs: 0,
        owner: NIL,
        notif: NIL,
        notif_gen: 0,
        bits: 0,
    }; MAX_IRQ],
);

/// Claim a free line. The capability created next holds the reference.
pub fn claim(intid: u32, proc_idx: u16) -> Result<(), i64> {
    if intid < 32 || intid as usize >= MAX_IRQ || intid >= gic::num_irqs() {
        return Err(-EINVAL);
    }
    let l = &mut LINES.get()[intid as usize];
    if l.owned {
        return Err(-EBUSY);
    }
    l.owned = true;
    l.refs = 0;
    l.owner = proc_idx;
    l.notif = NIL;
    Ok(())
}

pub fn incref(intid: u32) {
    LINES.get()[intid as usize].refs += 1;
}

pub fn decref(intid: u32) {
    let l = &mut LINES.get()[intid as usize];
    l.refs -= 1;
    if l.refs == 0 {
        release(intid);
    }
}

pub fn bind(intid: u32, notif: u16, notif_gen: u16, bits: u64) {
    let l = &mut LINES.get()[intid as usize];
    l.notif = notif;
    l.notif_gen = notif_gen;
    l.bits = bits;
    // The interrupt targets the subscribing thread's core: the notification
    // then delivers with no IPI.
    gic::set_target(intid, 1 << percpu::this().id);
    gic::set_priority(intid, 0xA0);
    gic::enable_irq(intid);
}

pub fn ack(intid: u32) {
    gic::enable_irq(intid);
}

pub fn release(intid: u32) {
    if (intid as usize) < MAX_IRQ {
        gic::disable_irq(intid);
        let l = &mut LINES.get()[intid as usize];
        l.owned = false;
        l.owner = NIL;
        l.notif = NIL;
    }
}

pub fn forget_notification(notif: u16) {
    for l in LINES.get().iter_mut() {
        if l.notif == notif {
            l.notif = NIL;
        }
    }
}

/// Handle all pending interrupts on this core; true if a device line
/// fired.
pub fn handle() -> bool {
    percpu::this().stats.irqs += 1;
    let mut device = false;
    loop {
        let iar = gic::ack();
        let id = iar & 0x3FF;
        if id == gic::SPURIOUS {
            break;
        }
        if cfg!(feature = "trace-sched") {
            crate::kprintln!("[s{}] irq {}", percpu::this().id, id);
        }
        match id {
            gic::IRQ_TIMER => timer::on_irq(),
            gic::SGI_RESCHEDULE => percpu::this().need_resched = true,
            32.. => {
                device = true;
                route(id);
            }
            _ => {}
        }
        gic::eoi(iar);
    }
    device
}

fn route(id: u32) {
    let l = LINES.get()[id as usize];
    // Masked until the driver acks (level lines).
    gic::disable_irq(id);
    if l.owned && l.notif != NIL && notify::alive(l.notif, l.notif_gen) {
        notify::signal(l.notif, l.bits);
    }
}

//! Tegra X1 GIC-400 (GICv2): distributor plus per-core CPU interface.

use crate::arch;
use crate::mmio::{read32, write32};

const GICD_BASE: usize = 0x5004_1000;
const GICC_BASE: usize = 0x5004_2000;

const GICD_CTLR: usize = GICD_BASE + 0x000;
const GICD_TYPER: usize = GICD_BASE + 0x004;
const GICD_IGROUPR: usize = GICD_BASE + 0x080;
const GICD_ISENABLER: usize = GICD_BASE + 0x100;
const GICD_ICENABLER: usize = GICD_BASE + 0x180;
const GICD_ICPENDR: usize = GICD_BASE + 0x280;
const GICD_IPRIORITYR: usize = GICD_BASE + 0x400;
const GICD_ITARGETSR: usize = GICD_BASE + 0x800;
const GICD_ICFGR: usize = GICD_BASE + 0xC00;
const GICD_SGIR: usize = GICD_BASE + 0xF00;

const GICC_CTLR: usize = GICC_BASE + 0x00;
const GICC_PMR: usize = GICC_BASE + 0x04;
const GICC_BPR: usize = GICC_BASE + 0x08;
const GICC_IAR: usize = GICC_BASE + 0x0C;
const GICC_EOIR: usize = GICC_BASE + 0x10;

/// ID returned by IAR when there is nothing to handle.
pub const SPURIOUS: u32 = 1023;

pub const IRQ_TIMER: u32 = 30;
pub const SGI_RESCHEDULE: u32 = 0;

/// Lines handled by the distributor.
pub fn num_irqs() -> u32 {
    let typer = unsafe { read32(GICD_TYPER) };
    ((typer & 0x1F) + 1) * 32
}

/// Move all interrupts to Group 1 (non-secure world).
/// EL3-only: `IGROUPR` is secure-only; Group 0 lines never reach the kernel.
#[no_mangle]
pub extern "C" fn nxk_secure_gic_init(core: usize) {
    unsafe {
        if core == 0 {
            write32(GICD_CTLR, 0);
            let lines = num_irqs().min(1020);
            for i in (0..lines).step_by(32) {
                write32(GICD_IGROUPR + (i as usize / 32) * 4, 0xFFFF_FFFF);
            }
            // EnableGrp0 | EnableGrp1
            write32(GICD_CTLR, 0b11);
        }
        write32(GICD_IGROUPR, 0xFFFF_FFFF);
        // Secure view of the CPU interface: Group 0 as FIQ, Group 1 as IRQ.
        write32(GICC_CTLR, (1 << 0) | (1 << 1) | (1 << 3));
        write32(GICC_PMR, 0xFF);
    }
    crate::arch::dsb_sy();
}

/// Init the distributor. Call once, from the boot core.
pub fn init_dist() {
    unsafe {
        write32(GICD_CTLR, 0);

        let lines = num_irqs().min(256);
        // SPIs: all disabled, not pending, mid priority, group 0.
        for i in (32..lines).step_by(32) {
            let n = (i / 32) as usize;
            write32(GICD_ICENABLER + n * 4, 0xFFFF_FFFF);
            write32(GICD_ICPENDR + n * 4, 0xFFFF_FFFF);
        }
        for i in (32..lines).step_by(4) {
            write32(GICD_IPRIORITYR + i as usize, 0xA0A0_A0A0);
            write32(GICD_ITARGETSR + i as usize, 0x0101_0101);
        }
        for i in (32..lines).step_by(16) {
            write32(GICD_ICFGR + (i as usize / 16) * 4, 0);
        }

        // Non-secure view: bit 0 = EnableGrp1.
        write32(GICD_CTLR, 1);
    }
    arch::dsb_sy();
}

/// Init the current core's CPU interface (call on every core).
pub fn init_cpu() {
    unsafe {
        write32(GICD_ICENABLER, 0xFFFF_FFFF);
        write32(GICD_ICPENDR, 0xFFFF_FFFF);
        for i in (0..32).step_by(4) {
            write32(GICD_IPRIORITYR + i, 0x8080_8080);
        }
        write32(GICD_ISENABLER, 0x0000_FFFF);

        write32(GICC_PMR, 0xF0);
        write32(GICC_BPR, 0x03);
        write32(GICC_CTLR, 1);
    }
    arch::dsb_sy();
}

pub fn enable_irq(irq: u32) {
    let (n, bit) = ((irq / 32) as usize, irq % 32);
    unsafe { write32(GICD_ISENABLER + n * 4, 1 << bit) };
    arch::dsb_sy();
}

pub fn disable_irq(irq: u32) {
    let (n, bit) = ((irq / 32) as usize, irq % 32);
    unsafe { write32(GICD_ICENABLER + n * 4, 1 << bit) };
    arch::dsb_sy();
}

pub fn set_priority(irq: u32, prio: u8) {
    unsafe {
        let addr = GICD_IPRIORITYR + (irq as usize & !3);
        let shift = (irq as usize & 3) * 8;
        let v = read32(addr);
        write32(addr, (v & !(0xFF << shift)) | ((prio as u32) << shift));
    }
}

/// Route an SPI line to a core mask.
pub fn set_target(irq: u32, cpu_mask: u8) {
    unsafe {
        let addr = GICD_ITARGETSR + (irq as usize & !3);
        let shift = (irq as usize & 3) * 8;
        let v = read32(addr);
        write32(addr, (v & !(0xFF << shift)) | ((cpu_mask as u32) << shift));
    }
}

/// Send a software interrupt to a core set.
pub fn send_sgi(cpu_mask: u8, sgi: u32) {
    arch::dsb_sy();
    unsafe { write32(GICD_SGIR, ((cpu_mask as u32) << 16) | (sgi & 0xF)) };
}

/// Claim the highest-priority pending interrupt (IAR read).
#[inline]
pub fn ack() -> u32 {
    unsafe { read32(GICC_IAR) }
}

/// Signal end of handling (full IAR value, SGI source included).
#[inline]
pub fn eoi(iar: u32) {
    unsafe { write32(GICC_EOIR, iar) };
}

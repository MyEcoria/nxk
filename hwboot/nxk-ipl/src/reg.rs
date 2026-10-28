//! MMIO access and the base addresses (hekate bdk/soc/t210.h).

use core::ptr::{read_volatile, write_volatile};

pub const TMR: u32 = 0x6000_5000;
pub const CAR: u32 = 0x6000_6000;
pub const FLOW_CTLR: u32 = 0x6000_7000;
pub const GPIO: u32 = 0x6000_D000;
pub const APB_MISC: u32 = 0x7000_0000;
pub const PINMUX_AUX: u32 = 0x7000_3000;
pub const UART: u32 = 0x7000_6000;
pub const PMC: u32 = 0x7000_E400;
pub const FUSE: u32 = 0x7000_F800;

#[inline(always)]
pub fn rd(a: u32) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}

#[inline(always)]
pub fn wr(a: u32, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}

/// Read-modify-write: keep the bits of `keep`, then or in `set`.
#[inline(always)]
pub fn rmw(a: u32, keep: u32, set: u32) {
    wr(a, rd(a) & keep | set);
}

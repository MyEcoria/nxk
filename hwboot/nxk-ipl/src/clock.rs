//! Device clocks and resets (hekate bdk/soc/clock.c).

use crate::hdr::*;
use crate::reg::{rd, wr, CAR};
use crate::timer::usleep;

/// A device's reset, enable and source registers (hekate `clk_rst_t`).
pub struct ClkRst {
    /// RST_DEV_x_SET.
    pub reset: u32,
    /// CLK_ENB_x_SET.
    pub enable: u32,
    /// CLK_SOURCE_x, 0 if none.
    pub source: u32,
    pub index: u32,
    pub clk_src: u32,
    pub clk_div: u32,
}

/// `CLK_SRC_DIV(d)` for an integer divider.
pub const fn src_div(d: u32) -> u32 {
    if d == 0 {
        0
    } else {
        (d - 1) * 2
    }
}

pub fn car(off: u32) -> u32 {
    rd(CAR + off)
}

pub fn set_car(off: u32, v: u32) {
    wr(CAR + off, v)
}

/// `clock_enable`.
pub fn enable(c: &ClkRst) {
    let module = 1 << c.index;
    set_car(c.reset, module);
    set_car(c.enable + CLK_CLR_OFFSET, module);
    if c.source != 0 {
        set_car(c.source, c.clk_src << 29 | c.clk_div);
    }
    set_car(c.enable, module);
    usleep(2);
    set_car(c.reset + CLK_CLR_OFFSET, module);
    let _ = car(c.reset);
}

/// `clock_disable`.
pub fn disable(c: &ClkRst) {
    let module = 1 << c.index;
    set_car(c.reset, module);
    set_car(c.enable + CLK_CLR_OFFSET, module);
}

pub const UARTA: ClkRst = ClkRst {
    reset: CLK_RST_CONTROLLER_RST_DEV_L_SET,
    enable: CLK_RST_CONTROLLER_CLK_ENB_L_SET,
    source: CLK_RST_CONTROLLER_CLK_SOURCE_UARTA,
    index: CLK_L_UARTA,
    clk_src: 0,
    clk_div: src_div(2),
};

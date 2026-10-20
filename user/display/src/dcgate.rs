//! The display controller's registers behind a gate. A register write
//! always lands in a shadow of the window and global state; it reaches the
//! hardware only while the gate is on. Asleep the controller is gated off
//! with its clock stopped (any access would hang the bus): writes then
//! change the shadow, reads come from it, and `resume` replays the shadow
//! into a controller `panel` brought up from reset.

use libnx::drv::Mmio;
use libnx::sync::Mutex;

use super::{
    CMD_DISPLAY_COMMAND, CMD_DISPLAY_WINDOW_HEADER, CMD_REG_ACT_CONTROL, CMD_STATE_CONTROL,
    DISP_BLEND_BACKGROUND_COLOR, DISP_DISP_ACTIVE, WINBUF_ADDR_H_OFFSET, WINBUF_ADDR_V_OFFSET,
    WINBUF_START_ADDR, WINBUF_SURFACE_KIND, WIN_BLEND_LAYER_CONTROL, WIN_BLEND_MATCH_SELECT,
    WIN_BLEND_NOMATCH_SELECT, WIN_BUFFER_CONTROL, WIN_COLOR_DEPTH, WIN_DDA_INCREMENT,
    WIN_H_INITIAL_DDA, WIN_LINE_STRIDE, WIN_OPTIONS, WIN_POSITION, WIN_PRESCALED_SIZE, WIN_SIZE,
    WIN_V_INITIAL_DDA,
};

/// Window registers kept for the replay, in write-back order (the enable
/// last: a window starts fetching once everything else is set).
const WIN_REGS: [usize; 17] = [
    WIN_BUFFER_CONTROL,
    WIN_COLOR_DEPTH,
    WIN_POSITION,
    WIN_SIZE,
    WIN_PRESCALED_SIZE,
    WIN_H_INITIAL_DDA,
    WIN_V_INITIAL_DDA,
    WIN_DDA_INCREMENT,
    WIN_LINE_STRIDE,
    WINBUF_START_ADDR,
    WINBUF_ADDR_H_OFFSET,
    WINBUF_ADDR_V_OFFSET,
    WINBUF_SURFACE_KIND,
    WIN_BLEND_LAYER_CONTROL,
    WIN_BLEND_MATCH_SELECT,
    WIN_BLEND_NOMATCH_SELECT,
    WIN_OPTIONS,
];

/// `CMD_DISPLAY_WINDOW_HEADER`: window select bits (A to D).
const SELECT_SHIFT: u32 = 4;
/// `CMD_STATE_CONTROL`: general / window update, then activation request.
const GENERAL_UPDATE: u32 = 1 << 8;
const GENERAL_ACT_REQ: u32 = 1;
/// `DC_DISP_DISP_WIN_OPTIONS`: the DSI output on (hekate leaves it set;
/// a controller out of reset has it clear).
const DISP_WIN_OPTIONS: usize = 0x402;
const DSI_ENABLE: u32 = 1 << 29;

struct Shadow {
    on: bool,
    /// Windows the header addresses (bits 3:0).
    sel: u32,
    win: [[u32; WIN_REGS.len()]; 4],
    /// Which registers of each window were written (bit per `WIN_REGS`).
    seen: [u32; 4],
    act: Option<u32>,
    bg: Option<u32>,
    cmd: Option<u32>,
    active: Option<u32>,
}

static DC: Mutex<Shadow> = Mutex::new(Shadow {
    on: true,
    sel: 0,
    win: [[0; WIN_REGS.len()]; 4],
    seen: [0; 4],
    act: None,
    bg: None,
    cmd: None,
    active: None,
});

fn slot(reg: usize) -> Option<usize> {
    WIN_REGS.iter().position(|&r| r == reg)
}

impl Shadow {
    fn record(&mut self, reg: usize, v: u32) {
        match reg {
            CMD_DISPLAY_WINDOW_HEADER => self.sel = (v >> SELECT_SHIFT) & 0xF,
            CMD_REG_ACT_CONTROL => self.act = Some(v),
            DISP_BLEND_BACKGROUND_COLOR => self.bg = Some(v),
            CMD_DISPLAY_COMMAND => self.cmd = Some(v),
            DISP_DISP_ACTIVE => self.active = Some(v),
            _ => {
                if let Some(k) = slot(reg) {
                    for w in (0..4).filter(|w| self.sel >> w & 1 != 0) {
                        self.win[w][k] = v;
                        self.seen[w] |= 1 << k;
                    }
                }
            }
        }
    }

    fn lookup(&self, reg: usize) -> u32 {
        match reg {
            CMD_REG_ACT_CONTROL => self.act.unwrap_or(0),
            DISP_BLEND_BACKGROUND_COLOR => self.bg.unwrap_or(0),
            CMD_DISPLAY_COMMAND => self.cmd.unwrap_or(0),
            DISP_DISP_ACTIVE => self.active.unwrap_or(0),
            _ => match (slot(reg), (0..4).find(|w| self.sel >> w & 1 != 0)) {
                (Some(k), Some(w)) => self.win[w][k],
                _ => 0,
            },
        }
    }
}

/// A register write: kept in the shadow, sent while the gate is on.
pub fn wr(dc: &Mmio, reg: usize, v: u32) {
    let mut s = DC.lock();
    s.record(reg, v);
    if s.on {
        dc.w32(reg * 4, v);
    }
}

/// A register read: the hardware's while the gate is on, else the shadow's
/// (no request pending).
pub fn rd(dc: &Mmio, reg: usize) -> u32 {
    let s = DC.lock();
    if s.on {
        dc.r32(reg * 4)
    } else if reg == CMD_STATE_CONTROL {
        0
    } else {
        s.lookup(reg)
    }
}

/// The controller as the loader left it: its window registers are read
/// into the shadow, so that a replay restores what the service did not set.
pub fn adopt(dc: &Mmio) {
    let mut s = DC.lock();
    for w in 0..4 {
        dc.w32(
            CMD_DISPLAY_WINDOW_HEADER * 4,
            1 << (SELECT_SHIFT + w as u32),
        );
        for (k, &reg) in WIN_REGS.iter().enumerate() {
            s.win[w][k] = dc.r32(reg * 4);
        }
        s.seen[w] = (1 << WIN_REGS.len()) - 1;
    }
    s.act = Some(dc.r32(CMD_REG_ACT_CONTROL * 4));
    s.bg = Some(dc.r32(DISP_BLEND_BACKGROUND_COLOR * 4));
    s.cmd = Some(dc.r32(CMD_DISPLAY_COMMAND * 4));
    s.active = Some(dc.r32(DISP_DISP_ACTIVE * 4));
    let sel = s.sel;
    dc.w32(CMD_DISPLAY_WINDOW_HEADER * 4, sel << SELECT_SHIFT);
    s.on = true;
}

/// Gate off: from here on `panel` owns the hardware. Returns once no
/// thread is inside an access.
pub fn suspend() {
    DC.lock().on = false;
}

/// Gate off, and `f` (which stops the controller's clock) runs before any
/// other thread can touch the registers again.
pub fn suspend_with(f: impl FnOnce()) {
    let mut s = DC.lock();
    s.on = false;
    f();
}

/// The controller is up from reset: the shadow replayed into it (windows,
/// the global registers, then the activation), and the gate on.
pub fn resume(dc: &Mmio) {
    let mut s = DC.lock();
    let w32 = |reg: usize, v: u32| dc.w32(reg * 4, v);
    w32(DISP_WIN_OPTIONS, DSI_ENABLE);
    if let Some(v) = s.act {
        w32(CMD_REG_ACT_CONTROL, v);
    }
    if let Some(v) = s.bg {
        w32(DISP_BLEND_BACKGROUND_COLOR, v);
    }
    if let Some(v) = s.active {
        w32(DISP_DISP_ACTIVE, v);
    }
    let mut update = GENERAL_UPDATE;
    let mut act = GENERAL_ACT_REQ;
    for w in 0..4 {
        if s.seen[w] == 0 {
            continue;
        }
        w32(CMD_DISPLAY_WINDOW_HEADER, 1 << (SELECT_SHIFT + w as u32));
        for (k, &reg) in WIN_REGS.iter().enumerate() {
            if s.seen[w] >> k & 1 != 0 {
                w32(reg, s.win[w][k]);
            }
        }
        update |= 1 << (9 + w);
        act |= 1 << (1 + w);
    }
    w32(CMD_DISPLAY_WINDOW_HEADER, s.sel << SELECT_SHIFT);
    if let Some(v) = s.cmd {
        w32(CMD_DISPLAY_COMMAND, v);
    }
    w32(CMD_STATE_CONTROL, update);
    w32(CMD_STATE_CONTROL, act);
    s.on = true;
}

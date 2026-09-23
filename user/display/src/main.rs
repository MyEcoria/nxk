//! display: Tegra X1 DC-A screen; window A on a shared image. Home
//! screen: the logo, live statistics under it (CPU, memory, USB-C
//! console, network, Wi-Fi, storage, temperatures) refreshed every
//! second, and at the bottom the last lines written to it (driver
//! failures, `fbecho`). Panel init is left to the bootloader; only the
//! window is changed. A portrait panel (Switch) is drawn turned.

#![no_std]
#![no_main]

extern crate alloc;

mod screen;
mod stats;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use libnx::abi::err::EINVAL;
use libnx::abi::proto::display as proto;
use libnx::abi::rights::{DUP, GRANT, MAP, WRITE};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{println, sys, time};

use screen::{Grid, Line, Screen, MAX_COLS};

const DC: u64 = 0x5420_0000;
/// tools/mklogo.py --raw: width, height, 8 R G B A palette entries, indices.
const LOGO: &[u8] = include_bytes!("../../../build/logo.bin");
const TAGLINE: &str = "AArch64 microkernel for Tegra X1";
/// Statistics refresh period.
const REFRESH_NS: u64 = 1_000_000_000;
/// Lines written to the screen, kept at its bottom.
const MSG_ROWS: usize = 3;

// 32-bit word register indices.
const CMD_DISPLAY_COMMAND: usize = 0x32;
const CMD_STATE_CONTROL: usize = 0x41;
const CMD_DISPLAY_WINDOW_HEADER: usize = 0x42;
const DISP_DISP_ACTIVE: usize = 0x409;
const DISP_BLEND_BACKGROUND_COLOR: usize = 0x4E4;
const WIN_OPTIONS: usize = 0x700;
const WIN_COLOR_DEPTH: usize = 0x703;
const WIN_POSITION: usize = 0x704;
const WIN_SIZE: usize = 0x705;
const WIN_PRESCALED_SIZE: usize = 0x706;
const WIN_H_INITIAL_DDA: usize = 0x707;
const WIN_V_INITIAL_DDA: usize = 0x708;
const WIN_DDA_INCREMENT: usize = 0x709;
const WIN_LINE_STRIDE: usize = 0x70A;
const WINBUF_START_ADDR: usize = 0x800;
const WINBUF_ADDR_H_OFFSET: usize = 0x806;
const WINBUF_ADDR_V_OFFSET: usize = 0x808;

const R8G8B8A8: u32 = 13;

/// Logo palette entry `i` (A B G R).
fn palette(i: usize) -> u32 {
    u32::from_le_bytes([
        LOGO[2 + 4 * i],
        LOGO[3 + 4 * i],
        LOGO[4 + 4 * i],
        LOGO[5 + 4 * i],
    ])
}

/// Label and tagline color: the logo's light amber.
pub fn accent() -> u32 {
    palette(6)
}

fn wr(dc: &Mmio, reg: usize, v: u32) {
    dc.w32(reg * 4, v);
}

/// Logo centered at the top, tagline under it; returns the first free
/// landscape row (pixels) below them.
fn splash(scr: &mut Screen, text_scale: usize) -> usize {
    let (sw, sh) = scr.size();
    let (w, h) = (LOGO[0] as usize, LOGO[1] as usize);
    let s = (sw * 40 / 100 / w).min(sh / 5 / h).max(1);
    let (x0, y0) = ((sw.saturating_sub(w * s)) / 2, 16 * text_scale);
    for (i, &p) in LOGO[34..34 + w * h].iter().enumerate() {
        if p != 0 {
            scr.fill(x0 + i % w * s, y0 + i / w * s, s, s, palette(p as usize));
        }
    }
    let ty = y0 + h * s + 8 * text_scale;
    let cw = 8 * text_scale;
    if TAGLINE.len() * cw <= sw {
        let tx = (sw - TAGLINE.len() * cw) / 2;
        for (k, c) in TAGLINE.bytes().enumerate() {
            scr.glyph(tx + k * cw, ty, c, text_scale, accent());
        }
    }
    ty + 16 * text_scale * 2
}

/// Bottom lines: text written to the screen, the latest last.
struct Messages {
    lines: VecDeque<String>,
    /// Unfinished last line, bytes pending (UTF-8).
    partial: Vec<u8>,
}

impl Messages {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'\n' => {
                    let l = String::from_utf8_lossy(&self.partial).into_owned();
                    self.partial.clear();
                    if self.lines.len() == MSG_ROWS {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(l);
                }
                b'\r' => {}
                b if self.partial.len() < MAX_COLS * 4 => self.partial.push(b),
                _ => {}
            }
        }
    }

    fn draw(&self, scr: &mut Screen, grid: &mut Grid) {
        let partial = String::from_utf8_lossy(&self.partial);
        let shown: Vec<&str> = self
            .lines
            .iter()
            .map(String::as_str)
            .chain((!partial.is_empty()).then_some(&*partial))
            .collect();
        let skip = shown.len().saturating_sub(MSG_ROWS);
        for row in 0..MSG_ROWS {
            let mut l = Line::new();
            if let Some(t) = shown.get(skip + row) {
                let _ = write!(l.color(screen::DIM), "{}", t);
            }
            grid.line(scr, row, &l);
        }
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let dc = Mmio::map(drv::dev_slot(0), DC, 0x40000).expect("display: registers");
    let active = dc.r32(DISP_DISP_ACTIVE * 4);
    let (mut w, mut h) = (
        (active & 0x1FFF) as usize,
        ((active >> 16) & 0x1FFF) as usize,
    );
    if w < 16 || h < 16 {
        (w, h) = (1280, 720); // display unconfigured: default mode
        wr(&dc, DISP_DISP_ACTIVE, (h as u32) << 16 | w as u32);
    }
    let fb = SharedMemory::new(w * h * 4, CONTIGUOUS | DMA32 | UNCACHED).expect("display: image");
    let pa = fb.paddr(0).expect("display: image address") as u32;
    let mut scr = Screen::new(fb, w, h);
    let (sw, sh) = scr.size();
    // Statistics at twice the font size on a large screen.
    let scale = if sw >= 1024 { 2 } else { 1 };
    scr.clear();
    let top = splash(&mut scr, scale);
    // Two-character margins.
    let (x0, cw) = (16 * scale, 8 * scale);
    let cols = ((sw - 2 * x0) / cw).min(MAX_COLS);
    let mut grid = Grid::new(x0, top, scale, cols, stats::ROWS);
    let msg_y = sh.saturating_sub((MSG_ROWS + 1) * 16);
    let mut msg_grid = Grid::new(x0, msg_y, 1, ((sw - 2 * x0) / 8).min(MAX_COLS), MSG_ROWS);
    let mut msgs = Messages {
        lines: VecDeque::new(),
        partial: Vec::new(),
    };

    wr(&dc, CMD_DISPLAY_WINDOW_HEADER, 1 << 4); // window A
    wr(&dc, WIN_COLOR_DEPTH, R8G8B8A8);
    wr(&dc, WIN_POSITION, 0);
    wr(&dc, WIN_SIZE, (h as u32) << 16 | w as u32);
    wr(&dc, WIN_PRESCALED_SIZE, (h as u32) << 16 | (w as u32 * 4));
    wr(&dc, WIN_H_INITIAL_DDA, 0);
    wr(&dc, WIN_V_INITIAL_DDA, 0);
    wr(&dc, WIN_DDA_INCREMENT, 0x1000_1000);
    wr(&dc, WIN_LINE_STRIDE, w as u32 * 4);
    wr(&dc, WINBUF_START_ADDR, pa);
    wr(&dc, WINBUF_ADDR_H_OFFSET, 0);
    wr(&dc, WINBUF_ADDR_V_OFFSET, 0);
    wr(&dc, WIN_OPTIONS, 1 << 30);
    wr(&dc, DISP_BLEND_BACKGROUND_COLOR, 0);
    wr(&dc, CMD_DISPLAY_COMMAND, 1 << 5); // continuous display
    wr(&dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(&dc, CMD_STATE_CONTROL, 1 | 2);

    let turned = if scr.turned() {
        ", turned landscape"
    } else {
        ""
    };
    msgs.write(alloc::format!("display: {}x{} panel{}\n", w, h, turned).as_bytes());
    msgs.draw(&mut scr, &mut msg_grid);
    println!(
        "[display] screen {}x{} (window A, image at {:#x})",
        w, h, pa
    );

    let ep = drv::publish("display").expect("display: publish");
    let mut stats = stats::Stats::new();
    let mut next = 0;
    // A client drawing on the image itself (OPEN) stops the refresh.
    let mut owned = false;
    // Handle sent with the previous reply: closed afterwards.
    let mut sent = 0u32;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let now = time::now_ns();
        if !owned && now >= next {
            stats.refresh(&mut scr, &mut grid);
            next = now + REFRESH_NS;
        }
        let wait = if owned {
            0
        } else {
            next.saturating_sub(time::now_ns()).max(1)
        };
        let Ok(rx) = ipc::recv_timeout(ep, wait) else {
            continue;
        };
        if sent != 0 {
            let _ = sys::handle_close(core::mem::take(&mut sent));
        }
        let reply = match rx.msg.label() {
            proto::OPEN => match sys::handle_dup(scr.fb.handle(), MAP | WRITE | GRANT | DUP, 0) {
                Ok(h2) => {
                    ipc::set_send_handle(0, h2);
                    sent = h2;
                    owned = true;
                    Msg::from_words(
                        0,
                        &[w as u64, h as u64, w as u64 * 4, proto::FORMAT_RGBA8888],
                    )
                    .with_handles(1)
                }
                Err(e) => Msg::error(e),
            },
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                msgs.write(&buf[..n]);
                if !owned {
                    msgs.draw(&mut scr, &mut msg_grid);
                }
                Msg::new(0, 0, 0)
            }
            _ => Msg::error(-EINVAL),
        };
        let _ = ipc::reply(&reply);
    }
}

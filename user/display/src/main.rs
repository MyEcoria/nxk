//! display: Tegra X1 DC-A screen; window A on shared image + 8x16 terminal.
//! Panel init is left to the bootloader; only the window is changed. Text
//! and boot logo are landscape: a portrait panel (Switch) is drawn turned.

#![no_std]
#![no_main]

extern crate alloc;

use libnx::abi::err::EINVAL;
use libnx::abi::proto::display as proto;
use libnx::abi::rights::{DUP, GRANT, MAP, WRITE};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::{println, sys};

const DC: u64 = 0x5420_0000;
const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
/// tools/mklogo.py --raw: width, height, 8 R G B A palette entries, indices.
const LOGO: &[u8] = include_bytes!("../../../build/logo.bin");
const TAGLINE: &str = "AArch64 microkernel for Tegra X1";

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
const FG: u32 = 0xFFD8_D8D8; // A B G R
const BG: u32 = 0xFF3A_1E10;

struct Screen {
    fb: SharedMemory,
    /// Image size, in panel scan order.
    w: usize,
    h: usize,
    /// Portrait panel: landscape view turned 90 degrees counterclockwise,
    /// as hekate stores its boot logo.
    rot: bool,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    /// Pending UTF-8 bytes for an incomplete character.
    pending: [u8; 4],
    npending: usize,
}

impl Screen {
    fn pixels(&mut self) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.fb.as_ptr() as *mut u32, self.w * self.h) }
    }

    /// Landscape size.
    fn size(&self) -> (usize, usize) {
        if self.rot {
            (self.h, self.w)
        } else {
            (self.w, self.h)
        }
    }

    /// Image index of landscape pixel (x, y).
    fn at(&self, x: usize, y: usize) -> usize {
        if self.rot {
            (self.h - 1 - x) * self.w + y
        } else {
            y * self.w + x
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for y in y..y + h {
            for x in x..x + w {
                let i = self.at(x, y);
                self.pixels()[i] = c;
            }
        }
    }

    fn clear(&mut self) {
        self.pixels().fill(BG);
        self.col = 0;
        self.row = 0;
    }

    /// Glyph at landscape pixel (x, y), `scale` times enlarged.
    fn draw_glyph(&mut self, x: usize, y: usize, cp: u32, scale: usize, fg: u32) {
        let g = if cp < 256 { cp } else { b'?' as u32 } as usize;
        for (dy, &bits) in FONT[g * 16..g * 16 + 16].iter().enumerate() {
            for dx in 0..8 {
                let c = if bits & (0x80 >> dx) != 0 { fg } else { BG };
                self.fill(x + dx * scale, y + dy * scale, scale, scale, c);
            }
        }
    }

    fn glyph(&mut self, cp: u32) {
        self.draw_glyph(self.col * 8, self.row * 16, cp, 1, FG);
    }

    /// Boot screen: logo centered, tagline under it; text starts below.
    fn splash(&mut self) {
        let (sw, sh) = self.size();
        let (w, h) = (LOGO[0] as usize, LOGO[1] as usize);
        let color = |i: usize| {
            u32::from_le_bytes([
                LOGO[2 + 4 * i],
                LOGO[3 + 4 * i],
                LOGO[4 + 4 * i],
                LOGO[5 + 4 * i],
            ])
        };
        let s = (sw * 55 / 100 / w).min(sh * 2 / 5 / h).max(1);
        let ts = if sw >= 1024 { 2 } else { 1 };
        let (x0, y0) = (
            (sw.saturating_sub(w * s)) / 2,
            (sh.saturating_sub(h * s + 32 * ts)) / 2,
        );
        for (i, &p) in LOGO[34..34 + w * h].iter().enumerate() {
            if p != 0 {
                self.fill(x0 + i % w * s, y0 + i / w * s, s, s, color(p as usize));
            }
        }
        let (tx, ty) = (
            (sw.saturating_sub(TAGLINE.len() * 8 * ts)) / 2,
            y0 + h * s + 16 * ts,
        );
        if TAGLINE.len() * 8 * ts <= sw {
            for (k, c) in TAGLINE.bytes().enumerate() {
                self.draw_glyph(tx + k * 8 * ts, ty, c as u32, ts, color(6));
            }
        }
        self.row = ((ty + 16 * ts).div_ceil(16) + 1).min(self.rows - 1);
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row == self.rows {
            // Scroll up one text line: image columns when turned.
            let (w, rot) = (self.w, self.rot);
            let px = self.pixels();
            if rot {
                for line in px.chunks_exact_mut(w) {
                    line.copy_within(16.., 0);
                    line[w - 16..].fill(BG);
                }
            } else {
                px.copy_within(16 * w.., 0);
                let n = px.len();
                px[n - 16 * w..].fill(BG);
            }
            self.row -= 1;
        }
    }

    fn put(&mut self, cp: u32) {
        match cp {
            0x0A => self.newline(),
            0x0D => self.col = 0,
            0x09 => {
                for _ in 0..(8 - self.col % 8) {
                    self.put(b' ' as u32);
                }
            }
            c if c >= 0x20 => {
                if self.col == self.cols {
                    self.newline();
                }
                self.glyph(c);
                self.col += 1;
            }
            _ => {}
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if b < 0x80 {
                self.npending = 0;
                self.put(b as u32);
                continue;
            }
            if b & 0xC0 != 0x80 {
                self.npending = 0;
            }
            if self.npending < 4 {
                self.pending[self.npending] = b;
                self.npending += 1;
            }
            let need = match self.pending[0] {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                _ => 4,
            };
            if self.npending == need {
                if let Some(c) = core::str::from_utf8(&self.pending[..need])
                    .ok()
                    .and_then(|s| s.chars().next())
                {
                    self.put(c as u32);
                }
                self.npending = 0;
            }
        }
    }
}

fn wr(dc: &Mmio, reg: usize, v: u32) {
    dc.w32(reg * 4, v);
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
    let rot = h > w;
    let (cols, rows) = if rot {
        (h / 8, w / 16)
    } else {
        (w / 8, h / 16)
    };
    let mut scr = Screen {
        fb,
        w,
        h,
        rot,
        cols,
        rows,
        col: 0,
        row: 0,
        pending: [0; 4],
        npending: 0,
    };
    scr.clear();
    scr.splash();

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

    let turned = if rot { " turned landscape" } else { "" };
    scr.write(
        alloc::format!(
            "nxk - screen {}x{}{}, terminal {}x{}\n",
            w,
            h,
            turned,
            cols,
            rows
        )
        .as_bytes(),
    );
    println!(
        "[display] screen {}x{} (window A, image at {:#x})",
        w, h, pa
    );

    let ep = drv::publish("display").expect("display: publish");
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    // Handle sent with the previous reply: closed afterwards.
    let mut sent = 0u32;
    drv::serve(ep, |rx| {
        if sent != 0 {
            let _ = sys::handle_close(core::mem::take(&mut sent));
        }
        Some(match rx.msg.label() {
            proto::OPEN => match sys::handle_dup(scr.fb.handle(), MAP | WRITE | GRANT | DUP, 0) {
                Ok(h2) => {
                    ipc::set_send_handle(0, h2);
                    let mut m = Msg::new(0, 4, 1);
                    m.mr[..4].copy_from_slice(&[
                        w as u64,
                        h as u64,
                        w as u64 * 4,
                        proto::FORMAT_RGBA8888,
                    ]);
                    sent = h2;
                    m
                }
                Err(e) => Msg::error(e),
            },
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                scr.write(&buf[..n]);
                Msg::new(0, 0, 0)
            }
            _ => Msg::error(-EINVAL),
        })
    })
}

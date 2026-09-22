//! Hekate-lit screen: only output without UART. Monitor clears the boot logo, mirrors logs
//! (8x16 font, portrait), leaves nxk the resume row in the last two pixels. MMU off: direct DRAM writes.

use core::ptr::{read_volatile, write_volatile};

const CAR: usize = 0x6000_6000;
const RST_DEVICES_L: usize = CAR + 0x004;
const CLK_OUT_ENB_L: usize = CAR + 0x010;
/// DISP1 + HOST1X clocks on and out of reset required (else bus hangs).
const DISP1_HOST1X: u32 = (1 << 27) | (1 << 28);

// Controleur d'affichage A (indices de mots de 32 bits).
const DC: usize = 0x5420_0000;
const CMD_DISPLAY_WINDOW_HEADER: usize = 0x42;
const WIN_OPTIONS: usize = 0x700;
const WIN_COLOR_DEPTH: usize = 0x703;
const WIN_SIZE: usize = 0x705;
const WIN_LINE_STRIDE: usize = 0x70A;
const WINBUF_START_ADDR: usize = 0x800;
const WINDOW_A_SELECT: u32 = 1 << 4;
const WIN_ENABLE: u32 = 1 << 30;
/// B8G8R8A8 (hekate) and R8G8B8A8.
const DEPTH_32BPP: [u32; 2] = [12, 13];

/// Same colors in both formats (equal components).
const BG: u32 = 0xFF00_0000;
const FG: u32 = 0xFFD8_D8D8;
const HANDOFF_MAGIC: u32 = 0x304B_584E; // "NXK0"

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
const GW: usize = 8;
const GH: usize = 16;

struct Fb {
    base: usize,
    w: usize,
    h: usize,
    /// One row stride, in pixels.
    stride: usize,
    col: usize,
    row: usize,
}

/// Core 0 only (secondaries are parked).
static mut FB: Option<Fb> = None;

fn fb() -> Option<&'static mut Fb> {
    unsafe { (*core::ptr::addr_of_mut!(FB)).as_mut() }
}

fn rd(a: usize) -> u32 {
    unsafe { read_volatile(a as *const u32) }
}
fn wr(a: usize, v: u32) {
    unsafe { write_volatile(a as *mut u32, v) }
}
fn dc(i: usize) -> usize {
    DC + i * 4
}

/// Window A lit on a 32-bit DRAM image: (base, width, height, stride in
/// pixels).
fn probe() -> Option<(usize, usize, usize, usize)> {
    if rd(CLK_OUT_ENB_L) & DISP1_HOST1X != DISP1_HOST1X || rd(RST_DEVICES_L) & DISP1_HOST1X != 0 {
        return None;
    }
    wr(dc(CMD_DISPLAY_WINDOW_HEADER), WINDOW_A_SELECT);
    if rd(dc(WIN_OPTIONS)) & WIN_ENABLE == 0
        || !DEPTH_32BPP.contains(&(rd(dc(WIN_COLOR_DEPTH)) & 0x7F))
    {
        return None;
    }
    let size = rd(dc(WIN_SIZE));
    let (w, h) = ((size & 0x1FFF) as usize, ((size >> 16) & 0x1FFF) as usize);
    let pitch = (rd(dc(WIN_LINE_STRIDE)) & 0xFFFF) as usize;
    let base = rd(dc(WINBUF_START_ADDR)) as usize;
    let sane = (64..=4096).contains(&w)
        && (64..=4096).contains(&h)
        && pitch >= w * 4
        && pitch.is_multiple_of(4)
        && base.is_multiple_of(4)
        && base >= 0x8000_0000
        && base + pitch * h <= 0x1_0000_0000;
    sane.then_some((base, w, h, pitch / 4))
}

/// Clear the screen if on (logo vanishes: visible proof of life).
pub fn init() {
    let Some((base, w, h, stride)) = probe() else {
        return;
    };
    let f = Fb {
        base,
        w,
        h,
        stride,
        col: 0,
        row: 0,
    };
    for r in 0..h / GH {
        f.clear_row(r);
    }
    unsafe { *core::ptr::addr_of_mut!(FB) = Some(f) };
}

pub fn write_str(s: &str) {
    let Some(f) = fb() else { return };
    for b in s.bytes() {
        match b {
            b'\n' => f.newline(),
            b'\r' => f.col = 0,
            0x20..=0x7E => f.put(b),
            _ => {}
        }
    }
}

/// Leave nxk the row to continue on (just before the jump).
pub fn handoff() {
    let Some(f) = fb() else { return };
    let next = if f.col == 0 {
        f.row
    } else {
        (f.row + 1) % (f.h / GH)
    };
    wr(f.addr(f.w - 2, f.h - 1), HANDOFF_MAGIC);
    wr(f.addr(f.w - 1, f.h - 1), next as u32);
}

impl Fb {
    fn addr(&self, x: usize, y: usize) -> usize {
        self.base + (y * self.stride + x) * 4
    }

    fn clear_row(&self, row: usize) {
        for y in row * GH..(row + 1) * GH {
            let line = self.addr(0, y);
            for x in 0..self.w {
                wr(line + x * 4, BG);
            }
        }
    }

    fn put(&mut self, c: u8) {
        if self.col == self.w / GW {
            self.newline();
        }
        let x0 = self.col * GW;
        for (dy, &bits) in FONT[c as usize * GH..][..GH].iter().enumerate() {
            let line = self.addr(x0, self.row * GH + dy);
            for dx in 0..GW {
                wr(
                    line + dx * 4,
                    if bits & (0x80 >> dx) != 0 { FG } else { BG },
                );
            }
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row = (self.row + 1) % (self.h / GH);
        self.clear_row(self.row);
    }
}

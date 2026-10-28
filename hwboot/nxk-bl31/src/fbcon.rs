//! Hekate-lit screen: only output without UART. The monitor draws the boot
//! screen over hekate's logo (`nxk-bootsplash`), or mirrors its logs (8x16
//! font, portrait) when Vol+ is held; the last two pixels tell nxk which,
//! and where to go on. MMU off: direct DRAM writes.

use core::ptr::{read_volatile, write_volatile};

use nxk_bootsplash::{self as splash, Format, Splash};

const CAR: usize = 0x6000_6000;
const RST_DEVICES_L: usize = CAR + 0x004;
const CLK_OUT_ENB_L: usize = CAR + 0x010;
/// DISP1 + HOST1X clocks on and out of reset required (else bus hangs).
const DISP1_HOST1X: u32 = (1 << 27) | (1 << 28);

// Display controller A (32-bit word indices).
const DC: usize = 0x5420_0000;
const CMD_DISPLAY_WINDOW_HEADER: usize = 0x42;
const WIN_OPTIONS: usize = 0x700;
const WIN_COLOR_DEPTH: usize = 0x703;
const WIN_SIZE: usize = 0x705;
const WIN_LINE_STRIDE: usize = 0x70A;
const WINBUF_START_ADDR: usize = 0x800;
const WINDOW_A_SELECT: u32 = 1 << 4;
const WIN_ENABLE: u32 = 1 << 30;

/// GPIO_IN of port X (bank 5, port 3): Vol+ on pin 6, Vol- on pin 7, low
/// when pressed (hekate `btn_read_vol`, which leaves them GPIO inputs).
const GPIO_IN_X: usize = 0x6000_D53C;
const VOL_UP: u32 = 1 << 6;
const VOL_DOWN: u32 = 1 << 7;

/// Same colors in both formats (equal components).
const BG: u32 = 0xFF00_0000;
const FG: u32 = 0xFFD8_D8D8;
/// Last two pixels: magic, then the next text row (text) or the progress (splash).
const TEXT_MAGIC: u32 = 0x304B_584E; // "NXK0"
const SPLASH_MAGIC: u32 = 0x314B_584E; // "NXK1"

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
const GW: usize = 8;
const GH: usize = 16;

struct Fb {
    base: usize,
    w: usize,
    h: usize,
    /// One row stride, in pixels.
    stride: usize,
    fmt: Format,
    /// The boot screen is shown: log lines are not drawn.
    splash: bool,
    col: usize,
    row: usize,
}

/// Core 0 only, before the jump (secondaries and PSCI never log).
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
/// pixels, format).
fn probe() -> Option<(usize, usize, usize, usize, Format)> {
    if rd(CLK_OUT_ENB_L) & DISP1_HOST1X != DISP1_HOST1X || rd(RST_DEVICES_L) & DISP1_HOST1X != 0 {
        return None;
    }
    wr(dc(CMD_DISPLAY_WINDOW_HEADER), WINDOW_A_SELECT);
    if rd(dc(WIN_OPTIONS)) & WIN_ENABLE == 0 {
        return None;
    }
    let fmt = Format::from_depth(rd(dc(WIN_COLOR_DEPTH)))?;
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
    sane.then_some((base, w, h, pitch / 4, fmt))
}

/// Vol+ alone held: logs as text. Both read pressed on an unwired port
/// (the emulator's GPIO_IN is 0), which keeps the boot screen.
fn verbose() -> bool {
    rd(GPIO_IN_X) & (VOL_UP | VOL_DOWN) == VOL_DOWN
}

/// Take over the screen if on: the boot screen, or a cleared text console.
pub fn init() {
    let Some((base, w, h, stride, fmt)) = probe() else {
        return;
    };
    let mut f = Fb {
        base,
        w,
        h,
        stride,
        fmt,
        splash: !verbose(),
        col: 0,
        row: 0,
    };
    if f.splash {
        f.splash(splash::AT_MONITOR);
    } else {
        f.clear();
    }
    unsafe { *core::ptr::addr_of_mut!(FB) = Some(f) };
}

/// Boot screen shown (else text, or no screen).
pub fn splash_shown() -> bool {
    fb().is_some_and(|f| f.splash)
}

/// Something went wrong: back to text over the boot screen, so it can be read.
pub fn alert() {
    if let Some(f) = fb().filter(|f| f.splash) {
        f.splash = false;
        f.clear();
    }
}

pub fn write_str(s: &str) {
    let Some(f) = fb().filter(|f| !f.splash) else {
        return;
    };
    for b in s.bytes() {
        match b {
            b'\n' => f.newline(),
            b'\r' => f.col = 0,
            0x20..=0x7E => f.put(b),
            _ => {}
        }
    }
}

/// Tell nxk how to go on (just before the jump).
pub fn handoff() {
    let Some(f) = fb() else { return };
    let (magic, next) = if f.splash {
        (SPLASH_MAGIC, splash::AT_KERNEL as usize)
    } else if f.col == 0 {
        (TEXT_MAGIC, f.row)
    } else {
        (TEXT_MAGIC, (f.row + 1) % (f.h / GH))
    };
    if f.splash {
        f.splash_bar(splash::AT_KERNEL);
    }
    wr(f.addr(f.w - 2, f.h - 1), magic);
    wr(f.addr(f.w - 1, f.h - 1), next as u32);
}

impl Fb {
    fn addr(&self, x: usize, y: usize) -> usize {
        self.base + (y * self.stride + x) * 4
    }

    /// Image rectangle in color `c` (A B G R).
    fn fill(&self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        let v = self.fmt.pixel(c);
        for row in y..(y + h).min(self.h) {
            let line = self.addr(0, row);
            for col in x..(x + w).min(self.w) {
                wr(line + col * 4, v);
            }
        }
    }

    /// Landscape rectangle of the boot screen, turned on a portrait panel.
    fn fill_view(&self, x: u32, y: u32, w: u32, h: u32, c: u32) {
        let (x, y, w, h) = if self.h > self.w {
            splash::turn(x, y, w, h, self.h as u32)
        } else {
            (x, y, w, h)
        };
        self.fill(x as usize, y as usize, w as usize, h as usize, c);
    }

    fn view(&self) -> Splash {
        let (w, h) = (self.w.max(self.h) as u32, self.w.min(self.h) as u32);
        Splash::new(w, h)
    }

    fn splash(&self, p: u32) {
        self.view()
            .draw(p, &mut |x, y, w, h, c| self.fill_view(x, y, w, h, c));
    }

    fn splash_bar(&self, p: u32) {
        self.view()
            .bar(p, &mut |x, y, w, h, c| self.fill_view(x, y, w, h, c));
    }

    fn clear(&mut self) {
        self.fill(0, 0, self.w, self.h, BG);
        (self.col, self.row) = (0, 0);
    }

    fn clear_row(&self, row: usize) {
        self.fill(0, row * GH, self.w, GH, BG);
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

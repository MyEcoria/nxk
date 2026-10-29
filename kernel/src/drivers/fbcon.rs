//! Fallback console on the loader-left screen (hekate seamless display).
//! Off unless window A scans a 32-bit DRAM image; cleaned to coherency.
//! Keeps nxk-bl31's boot screen (moving its bar) unless something fails.

use nxk_bootsplash::{self as splash, Format, Splash};

use crate::mmio::{read32, write32};

const CAR: usize = 0x6000_6000;
const RST_DEVICES_L: usize = CAR + 0x004;
const CLK_OUT_ENB_L: usize = CAR + 0x010;
/// DISP1 (bit 27) and HOST1X (bit 28): reading the DC with clock off or in
/// reset would hang the bus.
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

/// Same colors in both formats (equal components).
const BG: u32 = 0xFF00_0000;
const FG: u32 = 0xFFD8_D8D8;
/// Last two pixels: magic, then the next text row (text) or the progress
/// (boot screen), as nxk-bl31 leaves them.
const TEXT_MAGIC: u32 = 0x304B_584E; // "NXK0"
const SPLASH_MAGIC: u32 = 0x314B_584E; // "NXK1"

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
const GW: usize = 8;
const GH: usize = 16;
const CACHE_LINE: usize = 64;

struct Fb {
    base: usize,
    w: usize,
    h: usize,
    /// Row stride, in pixels.
    stride: usize,
    fmt: Format,
    /// The boot screen is shown: messages are not drawn.
    splash: bool,
    col: usize,
    row: usize,
}

/// Core 0 alone before the MMU, then under the console lock; panic paths
/// write lock-free, like the UART.
static mut FB: Option<Fb> = None;

fn fb() -> Option<&'static mut Fb> {
    unsafe { (*core::ptr::addr_of_mut!(FB)).as_mut() }
}

fn dc(i: usize) -> usize {
    DC + i * 4
}

/// Window A on with a 32-bit DRAM image: (base, width, height, stride px, format).
fn probe() -> Option<(usize, usize, usize, usize, Format)> {
    unsafe {
        if read32(CLK_OUT_ENB_L) & DISP1_HOST1X != DISP1_HOST1X
            || read32(RST_DEVICES_L) & DISP1_HOST1X != 0
        {
            return None;
        }
        write32(dc(CMD_DISPLAY_WINDOW_HEADER), WINDOW_A_SELECT);
        if read32(dc(WIN_OPTIONS)) & WIN_ENABLE == 0 {
            return None;
        }
        let fmt = Format::from_depth(read32(dc(WIN_COLOR_DEPTH)))?;
        let size = read32(dc(WIN_SIZE));
        let (w, h) = ((size & 0x1FFF) as usize, ((size >> 16) & 0x1FFF) as usize);
        let pitch = (read32(dc(WIN_LINE_STRIDE)) & 0xFFFF) as usize;
        let base = read32(dc(WINBUF_START_ADDR)) as usize;
        let sane = (64..=4096).contains(&w)
            && (64..=4096).contains(&h)
            && pitch >= w * 4
            && pitch.is_multiple_of(4)
            && base.is_multiple_of(4)
            && base >= 0x8000_0000
            && base + pitch * h <= 0x1_0000_0000;
        sane.then_some((base, w, h, pitch / 4, fmt))
    }
}

/// Call with MMU off, core 0 alone, before any message.
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
        splash: false,
        col: 0,
        row: 0,
    };
    let (magic, next) = (f.pixel(w - 2, h - 1), f.pixel(w - 1, h - 1));
    if magic == SPLASH_MAGIC {
        f.splash = true;
        f.set(w - 2, h - 1, fmt.pixel(splash::BG));
        f.set(w - 1, h - 1, fmt.pixel(splash::BG));
        f.splash_bar(splash::AT_KERNEL);
    } else if magic == TEXT_MAGIC && (next as usize) < h / GH {
        f.set(w - 2, h - 1, BG);
        f.set(w - 1, h - 1, BG);
        f.row = next as usize;
        f.clear_row(f.row);
    } else {
        f.clear();
    }
    unsafe { *core::ptr::addr_of_mut!(FB) = Some(f) };
}

/// Physical image range, page-rounded: kept out of free memory while the
/// console writes to it.
pub fn region() -> Option<(u64, u64)> {
    fb().map(|f| {
        let (b, e) = (f.base as u64, (f.base + f.stride * 4 * f.h) as u64);
        (b & !0xFFF, (e + 0xFFF) & !0xFFF)
    })
}

/// Shut the screen console off (image outside mapped DRAM).
pub fn disable() {
    unsafe { *core::ptr::addr_of_mut!(FB) = None };
}

/// The boot screen's bar at `p` thousandths (a boot stage reached).
pub fn progress(p: u32) {
    if let Some(f) = fb().filter(|f| f.splash) {
        f.splash_bar(p);
    }
}

/// Something went wrong: the boot screen gives way to text. True if it
/// was shown (the caller may then replay the messages so far).
pub fn alert() -> bool {
    let Some(f) = fb().filter(|f| f.splash) else {
        return false;
    };
    f.splash = false;
    f.clear();
    true
}

pub fn write_str(s: &str) {
    write_bytes(s.as_bytes());
}

pub fn write_bytes(s: &[u8]) {
    let Some(f) = fb().filter(|f| !f.splash) else {
        return;
    };
    for &b in s {
        match b {
            b'\n' => f.newline(),
            b'\r' => f.col = 0,
            0x20..=0x7E => f.put(b),
            0xC0..=0xFF => f.put(b'?'),
            _ => {}
        }
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack)) };
}

impl Fb {
    fn addr(&self, x: usize, y: usize) -> usize {
        self.base + (y * self.stride + x) * 4
    }

    fn pixel(&self, x: usize, y: usize) -> u32 {
        unsafe { read32(self.addr(x, y)) }
    }

    fn set(&self, x: usize, y: usize, v: u32) {
        unsafe { write32(self.addr(x, y), v) };
        clean(self.addr(x, y), 4);
    }

    /// Image rectangle, `v` as stored.
    fn fill(&self, x: usize, y: usize, w: usize, h: usize, v: u32) {
        let w = w.min(self.w.saturating_sub(x));
        for row in y..(y + h).min(self.h) {
            let line = self.addr(x, row);
            for k in 0..w {
                unsafe { write32(line + k * 4, v) };
            }
            clean(line, w * 4);
        }
    }

    /// The boot screen's bar, drawn as nxk-bl31 does (turned on a portrait panel).
    fn splash_bar(&self, p: u32) {
        let view = Splash::new(self.w.max(self.h) as u32, self.w.min(self.h) as u32);
        view.bar(p, &mut |x, y, w, h, c| {
            let (x, y, w, h) = if self.h > self.w {
                splash::turn(x, y, w, h, self.h as u32)
            } else {
                (x, y, w, h)
            };
            self.fill(
                x as usize,
                y as usize,
                w as usize,
                h as usize,
                self.fmt.pixel(c),
            );
        });
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
                unsafe {
                    write32(
                        line + dx * 4,
                        if bits & (0x80 >> dx) != 0 { FG } else { BG },
                    )
                };
            }
            clean(line, GW * 4);
        }
        self.col += 1;
    }

    /// Next row; at the bottom wrap to the top (no scrolling: copying the
    /// whole image per row would be slow).
    fn newline(&mut self) {
        self.col = 0;
        self.row = (self.row + 1) % (self.h / GH);
        self.clear_row(self.row);
    }
}

/// Clean `[addr, addr+len)` to the point of coherency (no-op with MMU off:
/// nothing is cached then).
fn clean(addr: usize, len: usize) {
    let mut a = addr & !(CACHE_LINE - 1);
    while a < addr + len {
        unsafe { core::arch::asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += CACHE_LINE;
    }
}

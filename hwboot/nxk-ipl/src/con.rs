//! nxk-ipl's log on the screen, since UART-A's pads are internal on a
//! console: every line is kept from the start, and drawn as text (8x16
//! font, portrait, as nxk-bl31 draws its own) once window A is up, when
//! Vol+ is held or when something fails.

use crate::display::FB;
use crate::reg::wr;

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
const GW: u32 = 8;
const GH: u32 = 16;
const W: u32 = 720;
const H: u32 = 1280;
const BG: u32 = 0xFF00_0000;
const FG: u32 = 0xFFD8_D8D8;

const LOG_LEN: usize = 6144;

struct Con {
    log: [u8; LOG_LEN],
    len: usize,
    /// Window A shows our framebuffer.
    ready: bool,
    /// The log is drawn (else the boot screen is shown).
    text: bool,
    col: u32,
    row: u32,
}

static mut CON: Con = Con {
    log: [0; LOG_LEN],
    len: 0,
    ready: false,
    text: false,
    col: 0,
    row: 0,
};

fn con() -> &'static mut Con {
    unsafe { &mut *core::ptr::addr_of_mut!(CON) }
}

/// One byte of the log.
pub fn byte(b: u8) {
    let c = con();
    if c.len < LOG_LEN {
        c.log[c.len] = b;
        c.len += 1;
    }
    if c.text {
        c.draw(b);
    }
}

/// Window A is up: text can be drawn from now on.
pub fn ready() {
    con().ready = true;
}

/// The log is shown instead of the boot screen.
pub fn is_text() -> bool {
    con().text
}

/// Text from now on, the log so far replayed (no-op before window A).
pub fn show() {
    let c = con();
    if !c.ready || c.text {
        return;
    }
    c.text = true;
    fill(0, 0, W, H);
    (c.col, c.row) = (0, 0);
    for i in 0..c.len {
        let b = c.log[i];
        c.draw(b);
    }
}

fn fill(x: u32, y: u32, w: u32, h: u32) {
    for row in y..y + h {
        for col in x..x + w {
            wr(FB + (row * W + col) * 4, BG);
        }
    }
}

impl Con {
    fn draw(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            0x20..=0x7E => self.put(b),
            _ => {}
        }
    }

    fn put(&mut self, ch: u8) {
        if self.col == W / GW {
            self.newline();
        }
        let x0 = self.col * GW;
        for (dy, &bits) in FONT[ch as usize * GH as usize..][..GH as usize].iter().enumerate() {
            let line = FB + ((self.row * GH + dy as u32) * W + x0) * 4;
            for dx in 0..GW {
                wr(line + dx * 4, if bits & (0x80 >> dx) != 0 { FG } else { BG });
            }
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row = (self.row + 1) % (H / GH);
        fill(0, self.row * GH, W, GH);
    }
}

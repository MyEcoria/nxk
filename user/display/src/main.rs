//! display : ecran (controleur d'affichage DC-A du Tegra X1).
//!
//! Le pilote programme la fenetre A sur une image en memoire (contigue, sous
//! 4 GiB, non cachee : lue directement par le controleur) et la partage avec
//! ses clients, qui y dessinent eux-memes. Il offre aussi un terminal texte
//! (police 8x16) pour les messages.
//!
//! Sur la console, le panneau (DSI, retroeclairage) est allume par le
//! chargeur d'amorcage : le pilote reprend l'affichage tel quel et n'y
//! change que la fenetre.

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

// Registres (indices de mots de 32 bits).
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
    w: usize,
    h: usize,
    col: usize,
    row: usize,
    /// Octets UTF-8 en attente d'un caractere complet.
    pending: [u8; 4],
    npending: usize,
}

impl Screen {
    fn pixels(&self) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.fb.as_ptr() as *mut u32, self.w * self.h) }
    }

    fn clear(&mut self) {
        self.pixels().fill(BG);
        self.col = 0;
        self.row = 0;
    }

    fn glyph(&mut self, cp: u32) {
        let g = if cp < 256 { cp } else { b'?' as u32 } as usize;
        let (x0, y0, w) = (self.col * 8, self.row * 16, self.w);
        let px = self.pixels();
        for (dy, &bits) in FONT[g * 16..g * 16 + 16].iter().enumerate() {
            let line = &mut px[(y0 + dy) * w + x0..(y0 + dy) * w + x0 + 8];
            for (dx, p) in line.iter_mut().enumerate() {
                *p = if bits & (0x80 >> dx) != 0 { FG } else { BG };
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row == self.h / 16 {
            // Defilement d'une ligne de texte.
            let (w, line) = (self.w, 16 * self.w);
            let px = self.pixels();
            px.copy_within(line.., 0);
            let n = px.len();
            px[n - line..].fill(BG);
            let _ = w;
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
                if self.col == self.w / 8 {
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
                if let Some(c) = core::str::from_utf8(&self.pending[..need]).ok().and_then(|s| s.chars().next()) {
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
    let dc = Mmio::map(drv::dev_slot(0), DC, 0x40000).expect("display : registres");
    let active = dc.r32(DISP_DISP_ACTIVE * 4);
    let (mut w, mut h) = ((active & 0x1FFF) as usize, ((active >> 16) & 0x1FFF) as usize);
    if w < 16 || h < 16 {
        (w, h) = (1280, 720); // affichage non configure : mode par defaut
        wr(&dc, DISP_DISP_ACTIVE, (h as u32) << 16 | w as u32);
    }
    let fb = SharedMemory::new(w * h * 4, CONTIGUOUS | DMA32 | UNCACHED).expect("display : image");
    let pa = fb.paddr(0).expect("display : adresse de l'image") as u32;
    let mut scr = Screen { fb, w, h, col: 0, row: 0, pending: [0; 4], npending: 0 };
    scr.clear();

    wr(&dc, CMD_DISPLAY_WINDOW_HEADER, 1 << 4); // fenetre A
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
    wr(&dc, CMD_DISPLAY_COMMAND, 1 << 5); // affichage continu
    wr(&dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(&dc, CMD_STATE_CONTROL, 1 | 2);

    scr.write(alloc::format!("nxk - écran {}x{}, terminal {}x{}\n", w, h, w / 8, h / 16).as_bytes());
    println!("[display] ecran {}x{} (fenetre A, image a {:#x})", w, h, pa);

    let ep = drv::publish("display").expect("display : publication");
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    // Copie de handle partie avec la reponse precedente : fermee ensuite.
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
                    m.mr[..4].copy_from_slice(&[w as u64, h as u64, w as u64 * 4, proto::FORMAT_RGBA8888]);
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

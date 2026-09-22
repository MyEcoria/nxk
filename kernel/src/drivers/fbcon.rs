//! Console de secours sur l'ecran laisse allume par le chargeur d'amorcage.
//!
//! Lance par hekate (entree L4T), nxk trouve le panneau encore allume : hekate
//! garde l'affichage « sans couture », la fenetre A du controleur DC-A balaie
//! toujours une image en DRAM (720x1280, 32 bits par pixel). Sans adaptateur
//! UART, c'est la seule sortie visible avant que le service `display` ne
//! reprenne l'ecran : le noyau y recopie ses messages (police 8x16 du service
//! d'affichage, sens portrait comme le menu texte de hekate).
//!
//! Rien ne s'active si la fenetre A n'est pas allumee sur une image 32 bits
//! en DRAM (emulateur, chargeur qui a eteint l'ecran). Le moniteur nxk-bl31
//! ecrit ses lignes d'abord et laisse dans les deux derniers pixels la ligne
//! ou reprendre (`HANDOFF_MAGIC`) ; sinon l'ecran est efface.
//!
//! L'image est de la DRAM ordinaire : une fois la MMU active (cache WB), chaque
//! ecriture est nettoyee jusqu'au point de coherence, le controleur
//! d'affichage lisant la memoire sans passer par les caches du CPU.

use crate::mmio::{read32, write32};

const CAR: usize = 0x6000_6000;
const RST_DEVICES_L: usize = CAR + 0x004;
const CLK_OUT_ENB_L: usize = CAR + 0x010;
/// DISP1 (bit 27) et HOST1X (bit 28) : lire le DC horloge coupee ou en reset
/// bloquerait le bus.
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
/// B8G8R8A8 (hekate) et R8G8B8A8 (service `display`).
const DEPTH_32BPP: [u32; 2] = [12, 13];

/// Couleurs identiques dans les deux formats (composantes egales).
const BG: u32 = 0xFF00_0000;
const FG: u32 = 0xFFD8_D8D8;
/// Deux derniers pixels : `HANDOFF_MAGIC` puis la ligne de texte suivante.
pub const HANDOFF_MAGIC: u32 = 0x304B_584E; // "NXK0"

const FONT: &[u8; 4096] = include_bytes!("../../../build/font.bin");
const GW: usize = 8;
const GH: usize = 16;
const CACHE_LINE: usize = 64;

struct Fb {
    base: usize,
    w: usize,
    h: usize,
    /// Pas d'une ligne, en pixels.
    stride: usize,
    col: usize,
    row: usize,
}

/// Coeur 0 seul avant la MMU, puis sous le verrou de la console ; les
/// chemins de panique ecrivent sans verrou, comme sur l'UART.
static mut FB: Option<Fb> = None;

fn fb() -> Option<&'static mut Fb> {
    unsafe { (*core::ptr::addr_of_mut!(FB)).as_mut() }
}

fn dc(i: usize) -> usize {
    DC + i * 4
}

/// Fenetre A allumee sur une image 32 bits en DRAM : (base, largeur, hauteur,
/// pas en pixels).
fn probe() -> Option<(usize, usize, usize, usize)> {
    unsafe {
        if read32(CLK_OUT_ENB_L) & DISP1_HOST1X != DISP1_HOST1X
            || read32(RST_DEVICES_L) & DISP1_HOST1X != 0
        {
            return None;
        }
        write32(dc(CMD_DISPLAY_WINDOW_HEADER), WINDOW_A_SELECT);
        if read32(dc(WIN_OPTIONS)) & WIN_ENABLE == 0
            || !DEPTH_32BPP.contains(&(read32(dc(WIN_COLOR_DEPTH)) & 0x7F))
        {
            return None;
        }
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
        sane.then_some((base, w, h, pitch / 4))
    }
}

/// A appeler MMU coupee, coeur 0 seul, avant tout message.
pub fn init() {
    let Some((base, w, h, stride)) = probe() else {
        return;
    };
    let mut f = Fb {
        base,
        w,
        h,
        stride,
        col: 0,
        row: 0,
    };
    let (magic, next) = (f.pixel(w - 2, h - 1), f.pixel(w - 1, h - 1));
    f.set(w - 2, h - 1, BG);
    f.set(w - 1, h - 1, BG);
    if magic == HANDOFF_MAGIC && (next as usize) < h / GH {
        f.row = next as usize;
        f.clear_row(f.row);
    } else {
        for r in 0..h / GH {
            f.clear_row(r);
        }
    }
    unsafe { *core::ptr::addr_of_mut!(FB) = Some(f) };
}

/// Plage physique de l'image, arrondie aux pages : a soustraire de la
/// memoire libre tant que la console y ecrit.
pub fn region() -> Option<(u64, u64)> {
    fb().map(|f| {
        let (b, e) = (f.base as u64, (f.base + f.stride * 4 * f.h) as u64);
        (b & !0xFFF, (e + 0xFFF) & !0xFFF)
    })
}

/// Coupe la console d'ecran (image hors de la DRAM projetee).
pub fn disable() {
    unsafe { *core::ptr::addr_of_mut!(FB) = None };
}

pub fn write_str(s: &str) {
    let Some(f) = fb() else { return };
    for b in s.bytes() {
        match b {
            b'\n' => f.newline(),
            b'\r' => f.col = 0,
            0x20..=0x7E => f.put(b),
            0xC0..=0xFF => f.put(b'?'), // premier octet UTF-8 : un caractere
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

    fn clear_row(&self, row: usize) {
        for y in row * GH..(row + 1) * GH {
            let line = self.addr(0, y);
            for x in 0..self.w {
                unsafe { write32(line + x * 4, BG) };
            }
            clean(line, self.w * 4);
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

    /// Ligne suivante ; en bas de l'ecran, on repart du haut (pas de
    /// defilement : recopier l'image entiere a chaque ligne serait lent).
    fn newline(&mut self) {
        self.col = 0;
        self.row = (self.row + 1) % (self.h / GH);
        self.clear_row(self.row);
    }
}

/// Nettoie `[addr, addr+len)` jusqu'au point de coherence (sans effet MMU
/// coupee : rien n'est alors en cache).
fn clean(addr: usize, len: usize) {
    let mut a = addr & !(CACHE_LINE - 1);
    while a < addr + len {
        unsafe { core::arch::asm!("dc cvac, {}", in(reg) a, options(nostack)) };
        a += CACHE_LINE;
    }
}

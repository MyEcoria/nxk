//! UART Tegra X1 : coeur 16550 avec registres espaces de 4 octets.
//!
//! UART-A est le port de debogage de la console (`-serial` n°1 cote QEMU).
//! Sur le vrai SoC il faut d'abord ouvrir l'horloge et lever le reset via le
//! CAR ; l'emulateur ne les modelise pas pour ce bloc mais on effectue quand
//! meme la sequence pour rester fidele au materiel.

use crate::mmio::{write32, RegBlock};

pub const UART_A: usize = 0x7000_6000;
pub const UART_B: usize = 0x7000_6040;
pub const UART_C: usize = 0x7000_6200;
pub const UART_D: usize = 0x7000_6300;

// Registres (indices, espacement 1 << 2)
const RBR_THR_DLL: usize = 0;
const IER_DLM: usize = 1;
const IIR_FCR: usize = 2;
const LCR: usize = 3;
const MCR: usize = 4;
const LSR: usize = 5;

const LCR_DLAB: u32 = 1 << 7;
const LCR_WORD_8BIT: u32 = 0b11;
const FCR_ENABLE: u32 = 1 << 0;
const FCR_CLEAR_RX: u32 = 1 << 1;
const FCR_CLEAR_TX: u32 = 1 << 2;
const LSR_THRE: u32 = 1 << 5;
const LSR_TMTY: u32 = 1 << 6;
const LSR_RDR: u32 = 1 << 0;

// Horloge de reference du peripherique sur T210 (PLLP / 1).
const UART_CLK_HZ: u32 = 408_000_000;

// CAR : horloge + reset d'UART-A (CLK_OUT_ENB_L / RST_DEVICES_L, bit 6).
const CAR_BASE: usize = 0x6000_6000;
const CLK_ENB_L_SET: usize = CAR_BASE + 0x320;
const RST_DEV_L_CLR: usize = CAR_BASE + 0x304;
const CAR_BIT_UARTA: u32 = 1 << 6;

#[derive(Clone, Copy)]
pub struct Uart {
    regs: RegBlock,
}

impl Uart {
    pub const fn new(base: usize) -> Self {
        Self {
            regs: RegBlock::new(base, 2),
        }
    }

    /// Configure le port en 8N1 au debit demande, FIFO actives.
    pub fn init(&self, baud: u32) {
        unsafe {
            // Horloge puis sortie de reset (no-op sous l'emulateur, correct sur SoC).
            write32(CLK_ENB_L_SET, CAR_BIT_UARTA);
            write32(RST_DEV_L_CLR, CAR_BIT_UARTA);

            let r = &self.regs;
            r.write(IER_DLM, 0); // pas d'interruptions pour l'instant
            r.write(LCR, LCR_DLAB);

            // Formule Tegra : arrondi au plus proche sur 16 x baud.
            let divisor = (UART_CLK_HZ + baud * 8) / (baud * 16);
            r.write(RBR_THR_DLL, divisor & 0xFF);
            r.write(IER_DLM, (divisor >> 8) & 0xFF);

            r.write(LCR, LCR_WORD_8BIT); // 8N1, DLAB retombe
            r.write(IIR_FCR, FCR_ENABLE | FCR_CLEAR_RX | FCR_CLEAR_TX);
            r.write(MCR, 0);
            let _ = r.read(IIR_FCR); // vide l'etat d'interruption
        }
    }

    #[inline]
    pub fn put(&self, byte: u8) {
        unsafe {
            while self.regs.read(LSR) & LSR_THRE == 0 {
                core::hint::spin_loop();
            }
            self.regs.write(RBR_THR_DLL, byte as u32);
        }
    }

    #[inline]
    pub fn write_str(&self, s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
    }

    /// Attend la vidange complete du registre de decalage.
    pub fn flush(&self) {
        unsafe {
            while self.regs.read(LSR) & LSR_TMTY == 0 {
                core::hint::spin_loop();
            }
        }
    }

    /// Lecture non bloquante.
    pub fn try_get(&self) -> Option<u8> {
        unsafe {
            if self.regs.read(LSR) & LSR_RDR != 0 {
                Some(self.regs.read(RBR_THR_DLL) as u8)
            } else {
                None
            }
        }
    }
}

impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        Uart::write_str(self, s);
        Ok(())
    }
}

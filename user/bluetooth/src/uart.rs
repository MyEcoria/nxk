//! UART-D, the HCI transport (H4): 8N1, RTS/CTS handled by the UART
//! itself (Tegra MCR `CTS_EN` / `RTS_EN`, as Linux `serial-tegra` with
//! CRTSCTS), receive interrupt. Its source clock runs at 48 MHz (`clk`
//! `BAUD_3M`) and is divided here: 1 for 3 Mbaud, 26 for 115200.

use alloc::vec::Vec;

use libnx::drv::Mmio;
use libnx::time;

/// 16550 registers (hekate bdk/soc/uart.h), 4 bytes apart.
const THR: usize = 0x00;
const IER: usize = 0x04;
const FCR: usize = 0x08;
const LCR: usize = 0x0C;
const MCR: usize = 0x10;
const LSR: usize = 0x14;
const SPR: usize = 0x1C;
const IRDA_CSR: usize = 0x20;
const LCR_8N1: u32 = 3;
const LCR_DLAB: u32 = 1 << 7;
const FCR_EN: u32 = 1;
const FCR_RX_CLR: u32 = 1 << 1;
const FCR_TX_CLR: u32 = 1 << 2;
/// Receive interrupt from 4 bytes in the FIFO; the timeout one takes
/// the rest.
const FCR_RX_TRIG_4: u32 = 1 << 6;
const MCR_RTS: u32 = 1 << 1;
const MCR_CTS_EN: u32 = 1 << 5;
const MCR_RTS_EN: u32 = 1 << 6;
const LSR_RDR: u32 = 1;
const LSR_OVERRUN: u32 = 1 << 1;
const LSR_FRAMING: u32 = 1 << 3;
const LSR_THRE: u32 = 1 << 5;
const LSR_TMTY: u32 = 1 << 6;
const IER_RHR: u32 = 1;
const IER_RLS: u32 = 1 << 2;
const IER_RX_TIMEOUT: u32 = 1 << 4;
const CLOCK_HZ: u64 = 48_000_000;
/// Bytes written per wait for an empty FIFO (Tegra's holds 32, the
/// emulator's 16).
const TX_CHUNK: usize = 16;
/// The chip holds CTS while busy; longer than this, it is gone.
const TX_STALL_NS: u64 = 50_000_000;
/// Bytes read per call: a stuck line must not keep the service here.
const RX_MAX: usize = 4096;

pub struct Uart {
    m: Mmio,
    pub baud: u64,
    /// Overrun and framing errors seen.
    pub errors: u64,
}

impl Uart {
    pub const fn new(m: Mmio) -> Self {
        Uart {
            m,
            baud: 0,
            errors: 0,
        }
    }

    fn tx_idle(&self) {
        let end = time::now_ns() + 5_000_000;
        while self.m.r32(LSR) & LSR_TMTY == 0 && time::now_ns() < end {}
    }

    /// hekate `uart_init` order, with the divisor latch set for `baud`
    /// and flow control by the UART. The FIFOs are emptied.
    pub fn set_baud(&mut self, baud: u64) {
        self.tx_idle();
        let div = ((CLOCK_HZ / 16 + baud / 2) / baud).max(1) as u32;
        let m = &self.m;
        let ier = m.r32(IER);
        m.w32(IER, 0);
        m.w32(LCR, LCR_DLAB | LCR_8N1);
        m.w32(THR, div & 0xFF);
        m.w32(IER, div >> 8);
        m.w32(LCR, LCR_8N1);
        let _ = m.r32(SPR);
        m.w32(IRDA_CSR, 0);
        m.w32(FCR, FCR_EN);
        let _ = m.r32(SPR);
        time::sleep_us(20);
        m.w32(FCR, FCR_EN | FCR_RX_CLR | FCR_TX_CLR | FCR_RX_TRIG_4);
        m.w32(MCR, MCR_CTS_EN | MCR_RTS_EN | MCR_RTS);
        let _ = m.r32(SPR);
        m.w32(IER, ier);
        self.baud = baud;
    }

    /// Receive interrupts on (data, timeout, line errors).
    pub fn irq_on(&self) {
        self.m.w32(IER, IER_RHR | IER_RLS | IER_RX_TIMEOUT);
    }

    pub fn irq_off(&self) {
        self.m.w32(IER, 0);
    }

    /// What the FIFO holds, appended to `out`.
    pub fn read(&mut self, out: &mut Vec<u8>) -> usize {
        let mut n = 0;
        while n < RX_MAX {
            let lsr = self.m.r32(LSR);
            if lsr & (LSR_OVERRUN | LSR_FRAMING) != 0 {
                self.errors += 1;
            }
            if lsr & LSR_RDR == 0 {
                break;
            }
            out.push(self.m.r32(THR) as u8);
            n += 1;
        }
        n
    }

    /// All of `data`, or false if the chip held CTS too long.
    pub fn write(&self, data: &[u8]) -> bool {
        for chunk in data.chunks(TX_CHUNK) {
            let end = time::now_ns() + TX_STALL_NS;
            while self.m.r32(LSR) & LSR_THRE == 0 {
                if time::now_ns() > end {
                    return false;
                }
            }
            for &b in chunk {
                self.m.w32(THR, b as u32);
            }
        }
        true
    }
}

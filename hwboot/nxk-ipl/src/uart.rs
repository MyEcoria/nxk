//! Debug UART (hekate bdk/soc/uart.c): UART-A at 115200 baud, as
//! nxk-bl31 and nxk use it; on a retail Switch its pads are internal.

use crate::clock;
use crate::hdr::*;
use crate::reg::{rd, wr, PINMUX_AUX, UART};
use crate::timer::usleep;

const A: u32 = UART;
const BAUD: u32 = 115_200;

fn r(off: u32) -> u32 {
    rd(A + off)
}
fn w(off: u32, v: u32) {
    wr(A + off, v)
}

// uart_t register offsets.
const THR_DLAB: u32 = 0x00;
const IER_DLAB: u32 = 0x04;
const IIR_FCR: u32 = 0x08;
const LCR: u32 = 0x0C;
const MCR: u32 = 0x10;
const LSR: u32 = 0x14;
const SPR: u32 = 0x1C;

/// `pinmux_config_uart(UART_A)`, `clock_enable_uart(UART_A)` and
/// `uart_init(UART_A, 115200, UART_AO_TX_AO_RX)`.
pub fn init_debug() {
    wr(PINMUX_AUX + 0xE4, 0);
    wr(PINMUX_AUX + 0xE8, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    wr(PINMUX_AUX + 0xEC, 0);
    wr(PINMUX_AUX + 0xF0, PINMUX_INPUT_ENABLE | PINMUX_TRISTATE);
    clock::enable(&clock::UARTA);

    wait_xfer(UART_TX_IDLE);
    // clock_uart_use_src_div: not 1 or 3 Mbaud, so the plain divider.
    let src = clock::car(CLK_RST_CONTROLLER_CLK_SOURCE_UARTA) & 0xE000_0000;
    clock::set_car(CLK_RST_CONTROLLER_CLK_SOURCE_UARTA, src | clock::src_div(2));
    let div = (8 * BAUD + 408_000_000) / (16 * BAUD);
    w(IER_DLAB, 0);
    w(LCR, UART_LCR_DLAB | UART_LCR_WORD_LENGTH_8);
    w(THR_DLAB, div & 0xFF);
    w(IER_DLAB, div >> 8 & 0xFF);
    w(LCR, UART_LCR_WORD_LENGTH_8);
    let _ = r(SPR);
    w(IIR_FCR, UART_IIR_FCR_EN_FIFO);
    let _ = r(SPR);
    usleep(20);
    w(MCR, 0);
    usleep(96);
    w(IIR_FCR, UART_IIR_FCR_EN_FIFO | UART_IIR_FCR_TX_CLR | UART_IIR_FCR_RX_CLR);
    w(MCR, 0);
    usleep(3 * ((BAUD + 999_999) / BAUD));
    wait_xfer(UART_TX_IDLE | UART_RX_RDYR);
}

fn wait_xfer(which: u32) {
    if which & UART_TX_IDLE != 0 {
        while r(LSR) & UART_LSR_TMTY == 0 {}
    }
    if which & UART_RX_RDYR != 0 {
        while r(LSR) & UART_LSR_RDR != 0 {
            let _ = r(THR_DLAB);
        }
    }
}

pub fn putc(c: u8) {
    while r(LSR) & UART_LSR_THRE == 0 {}
    w(THR_DLAB, c as u32);
}

pub fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

struct Out;

impl core::fmt::Write for Out {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        puts(s);
        Ok(())
    }
}

pub fn print(args: core::fmt::Arguments) {
    let _ = core::fmt::Write::write_fmt(&mut Out, args);
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::uart::print(format_args!("{}\n", format_args!($($arg)*)))
    };
}

pub fn puthex(v: u32) {
    puts("0x");
    for i in (0..8).rev() {
        putc(b"0123456789abcdef"[(v >> (i * 4) & 0xF) as usize]);
    }
}

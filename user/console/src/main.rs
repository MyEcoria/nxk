//! Console driver: Tegra X1 UART-A in user space (one page, one IRQ).
//! Line editing included: programs only get full lines. A terminal
//! driver (USB) mirrors it: its bytes are typed, it reads all output.
//! Nothing waits on the UART: output lands in one ring that the TX-empty
//! interrupt drains to UART-A and the terminal driver reads at its own
//! pace; the kernel log is drained into it too. The screen reads the
//! same ring without taking from it (TERMINAL page). Everything but the
//! shell's session (its handle's badge) comes between LOG marks.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use libnx::abi::err::{ENOSYS, EPERM};
use libnx::abi::proto::console as proto;
use libnx::abi::slots;
use libnx::drv::Mmio;
use libnx::ipc::{self, Msg, Received};
use libnx::sys;

const UART_A: u64 = 0x7000_6000;
/// Output kept for the terminal driver while it does not read (oldest
/// bytes dropped): recent history for a host that attaches later.
const RING: usize = 16 * 1024;
/// TX FIFO bytes written per TX-empty interrupt (16550 depth).
const FIFO: usize = 16;

// 16550 registers, 4-byte spacing.
const THR_RBR: usize = 0x00;
const IER: usize = 0x04;
const LSR: usize = 0x14;
const LSR_RDR: u32 = 1 << 0;
const LSR_THRE: u32 = 1 << 5;
const IER_RX: u32 = 1 << 0;
const IER_TX: u32 = 1 << 1;

/// Notification bits: UART interrupt, kernel log data.
const EV_UART: u64 = 1;
const EV_KLOG: u64 = 2;

/// Output ring: `head` counts every byte ever written; readers keep
/// their own position and skip what was overwritten.
struct Ring {
    buf: Vec<u8>,
    head: u64,
}

impl Ring {
    fn push(&mut self, b: u8) {
        self.buf[self.head as usize % RING] = b;
        self.head += 1;
    }

    /// Unread bytes from `pos`, up to the ring's wrap point.
    fn pending(&self, pos: &mut u64) -> &[u8] {
        *pos = (*pos).max(self.head.saturating_sub(RING as u64));
        let start = *pos as usize % RING;
        let n = ((self.head - *pos) as usize).min(RING - start);
        &self.buf[start..start + n]
    }
}

struct Console {
    uart: Mmio,
    out: Ring,
    /// Next byte for UART-A; TX-empty interrupt armed.
    uart_pos: u64,
    tx_irq: bool,
    /// Next byte for the terminal driver, its notification (0: none)
    /// and bits, and whether it was signaled since it last caught up.
    term_pos: u64,
    term_notif: u32,
    term_bits: u64,
    term_signaled: bool,
    line: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    /// Client waiting for a line.
    reader: Option<u64>,
}

impl Console {
    /// Into the ring, '\n' as CRLF.
    fn push(&mut self, data: &[u8]) {
        for &b in data {
            if b == b'\n' {
                self.out.push(b'\r');
            }
            self.out.push(b);
        }
    }

    /// Output to the UART and the terminal.
    fn out(&mut self, data: &[u8]) {
        self.push(data);
        self.uart_tx();
        self.term_wake();
    }

    /// A log (not the session's output): between the LOG marks.
    fn log(&mut self, data: &[u8]) {
        if !data.is_empty() {
            self.push(proto::LOG_START);
            self.push(data);
            self.out(proto::LOG_END);
        }
    }

    /// Fill the TX FIFO if it is empty; the TX-empty interrupt stays
    /// armed while bytes remain.
    fn uart_tx(&mut self) {
        let mut pos = self.uart_pos;
        if self.uart.r32(LSR) & LSR_THRE != 0 {
            let chunk = self.out.pending(&mut pos);
            let n = chunk.len().min(FIFO);
            for &b in &chunk[..n] {
                self.uart.w32(THR_RBR, b as u32);
            }
            pos += n as u64;
        }
        self.uart_pos = pos;
        let more = !self.out.pending(&mut pos).is_empty();
        if more != self.tx_irq {
            self.tx_irq = more;
            self.uart.w32(IER, IER_RX | if more { IER_TX } else { 0 });
        }
    }

    /// Signal the terminal driver once per batch of new output.
    fn term_wake(&mut self) {
        if self.term_notif != 0 && !self.term_signaled && self.term_pos < self.out.head {
            self.term_signaled = sys::notify_signal(self.term_notif, self.term_bits).is_ok();
        }
    }

    /// Up to `max` pending bytes for the terminal (none: it caught up).
    fn term_take(&mut self, max: usize) -> Msg {
        let mut pos = self.term_pos;
        let chunk = self.out.pending(&mut pos);
        let n = chunk.len().min(max);
        let m = ipc::pack(0, &chunk[..n]);
        self.term_pos = pos + n as u64;
        if self.term_pos == self.out.head {
            self.term_signaled = false;
        }
        m
    }

    /// Kernel log: `skip` bytes it already printed on UART-A go to the
    /// terminal only, the rest to both.
    fn klog(&mut self, mut skip: u64) {
        let mut buf = [0u8; 256];
        loop {
            let n = sys::klog_read(slots::DRV_KLOG, &mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            let quiet = skip.min(n as u64) as usize;
            skip -= quiet as u64;
            let caught_up = self.uart_pos >= self.out.head;
            self.push(proto::LOG_START);
            self.push(&buf[..quiet]);
            if caught_up {
                self.uart_pos = self.out.head;
            }
            self.push(&buf[quiet..n]);
            self.out(proto::LOG_END);
        }
    }

    /// Typed bytes (UART or terminal): line editing and echo.
    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            match b {
                b'\r' | b'\n' => {
                    self.out(b"\n");
                    let l = core::mem::take(&mut self.line);
                    self.lines.push_back(l);
                }
                0x08 | 0x7F => {
                    // One whole UTF-8 char (continuation bytes included).
                    let mut erased = false;
                    while let Some(c) = self.line.pop() {
                        erased = true;
                        if c & 0xC0 != 0x80 {
                            break;
                        }
                    }
                    if erased {
                        self.out(b"\x08 \x08");
                    }
                }
                0x20..=0x7E | 0x80..=0xFF if self.line.len() < 1000 => {
                    self.line.push(b);
                    self.out(&[b]);
                }
                _ => {}
            }
        }
        if let Some(tok) = self.reader {
            if let Some(l) = self.lines.pop_front() {
                let _ = ipc::reply_to(tok, &ipc::pack(0, &l));
                self.reader = None;
            }
        }
    }

    /// UART interrupt: received bytes edited, then the FIFO refilled.
    fn uart_irq(&mut self, buf: &mut [u8]) {
        let mut n = 0;
        while self.uart.r32(LSR) & LSR_RDR != 0 {
            buf[n] = self.uart.r32(THR_RBR) as u8;
            n += 1;
            if n == buf.len() {
                self.feed(&buf[..n]);
                n = 0;
            }
        }
        self.feed(&buf[..n]);
        self.uart_tx();
    }

    /// One request; `None`: reply deferred (token kept).
    fn request(&mut self, rx: &Received, buf: &mut [u8]) -> Option<Msg> {
        let terminal = rx.badge == proto::BADGE_TERMINAL;
        let screen = rx.badge == proto::BADGE_SCREEN;
        Some(match rx.msg.label() {
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, buf);
                if rx.badge == proto::BADGE_SESSION {
                    self.out(&buf[..n]);
                } else {
                    self.log(&buf[..n]);
                }
                Msg::new(0, 0, 0)
            }
            proto::READ_LINE => match self.lines.pop_front() {
                Some(l) => ipc::pack(0, &l),
                None => {
                    // Deferred reply: sent with the next line.
                    self.reader = Some(rx.token);
                    return None;
                }
            },
            proto::INPUT if terminal => {
                let n = ipc::unpack(&rx.msg, buf);
                self.feed(&buf[..n]);
                Msg::new(0, 0, 0)
            }
            proto::OUTPUT if terminal => {
                let max = match (rx.msg.words(), rx.msg.mr[0] as usize) {
                    (0, _) | (_, 0) => ipc::MAX_BYTES,
                    (_, n) => n.min(ipc::MAX_BYTES),
                };
                self.term_take(max)
            }
            proto::ATTACH if terminal && rx.msg.handles() == 1 => {
                if self.term_notif != 0 {
                    let _ = sys::handle_close(self.term_notif);
                }
                self.term_notif = ipc::received_handle(0);
                self.term_bits = rx.msg.mr[0];
                self.term_signaled = false;
                self.term_wake();
                Msg::new(0, 0, 0)
            }
            proto::TAIL if screen => {
                let mut pos = rx.msg.mr[0];
                let max = match rx.msg.mr[1] as usize {
                    0 => ipc::MAX_BYTES - 8,
                    n => n.min(ipc::MAX_BYTES - 8),
                };
                let chunk = self.out.pending(&mut pos);
                let n = chunk.len().min(max);
                ipc::pack_at(0, &[pos + n as u64], &chunk[..n])
            }
            proto::INPUT | proto::OUTPUT | proto::ATTACH | proto::TAIL => Msg::error(-EPERM),
            _ => Msg::error(-ENOSYS),
        })
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let ep = slots::DRV_SERVICE;
    let mut c = Console {
        uart: Mmio::map(slots::DRV_MMIO, UART_A, 0x1000).expect("mmio map"),
        out: Ring {
            buf: vec![0; RING],
            head: 0,
        },
        uart_pos: 0,
        tx_irq: false,
        term_pos: 0,
        term_notif: 0,
        term_bits: 0,
        term_signaled: false,
        line: Vec::with_capacity(256),
        lines: VecDeque::new(),
        reader: None,
    };

    let notif = sys::notify_create().expect("notify create");
    sys::irq_bind(slots::DRV_IRQ, notif, EV_UART).expect("irq bind");
    sys::notify_bind(notif).expect("notify bind");
    c.uart.w32(IER, IER_RX);
    // The kernel log drains here from now on; its boot part is already
    // on UART-A. Without the capability the kernel keeps printing.
    if let Ok(backlog) = sys::klog_bind(slots::DRV_KLOG, notif, EV_KLOG) {
        c.klog(backlog);
    }

    let mut buf = [0u8; ipc::MAX_BYTES];
    let mut rx = ipc::recv(ep).expect("recv");
    loop {
        let reply = if rx.msg.is_notify() {
            let ev = rx.msg.mr[0];
            if ev & EV_UART != 0 {
                c.uart_irq(&mut buf);
                let _ = sys::irq_ack(slots::DRV_IRQ);
            }
            if ev & EV_KLOG != 0 {
                c.klog(0);
            }
            None
        } else {
            c.request(&rx, &mut buf)
        };
        rx = match reply.map(|m| ipc::reply_recv(ep, &m)) {
            Some(Ok(r)) => r,
            _ => ipc::recv(ep).expect("recv"),
        };
    }
}

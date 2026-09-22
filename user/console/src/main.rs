//! Console driver: Tegra X1 UART-A in user space (one page, one IRQ).
//! Line editing included: programs only get full lines. A terminal
//! driver (USB) mirrors it: its bytes are typed, it reads all output.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::err::{EBUSY, ENOSYS, EPERM};
use libnx::abi::proto::console as proto;
use libnx::abi::slots;
use libnx::ipc::{self, Msg, Received};
use libnx::sys;

const UART_A: u64 = 0x7000_6000;
/// Output kept for the terminal driver while it does not read (oldest
/// bytes dropped): recent history for a host that attaches later.
const MIRROR_BYTES: usize = 16 * 1024;

// 16550 registers, 4-byte spacing.
const THR_RBR: usize = 0x00;
const IER: usize = 0x04;
const LSR: usize = 0x14;
const LSR_RDR: u32 = 1 << 0;
const LSR_THRE: u32 = 1 << 5;
const IER_RX: u32 = 1 << 0;

struct Uart {
    base: usize,
}

impl Uart {
    fn rd(&self, r: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.base + r) as *const u32) }
    }
    fn wr(&self, r: usize, v: u32) {
        unsafe { core::ptr::write_volatile((self.base + r) as *mut u32, v) }
    }
    fn put(&self, b: u8) {
        while self.rd(LSR) & LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        self.wr(THR_RBR, b as u32);
    }
    fn get(&self) -> Option<u8> {
        (self.rd(LSR) & LSR_RDR != 0).then(|| self.rd(THR_RBR) as u8)
    }
}

struct Console {
    uart: Uart,
    line: Vec<u8>,
    lines: VecDeque<Vec<u8>>,
    /// Client waiting for a line.
    reader: Option<u64>,
    mirror: VecDeque<u8>,
    /// Terminal driver waiting for output.
    mirror_reader: Option<u64>,
}

impl Console {
    /// Output to the UART and the mirror, '\n' as CRLF.
    fn out(&mut self, data: &[u8]) {
        for &b in data {
            if b == b'\n' {
                self.emit(b'\r');
            }
            self.emit(b);
        }
        if !self.mirror.is_empty() {
            if let Some(tok) = self.mirror_reader.take() {
                let m = self.take_mirror();
                let _ = ipc::reply_to(tok, &m);
            }
        }
    }

    fn emit(&mut self, b: u8) {
        self.uart.put(b);
        if self.mirror.len() == MIRROR_BYTES {
            self.mirror.pop_front();
        }
        self.mirror.push_back(b);
    }

    fn take_mirror(&mut self) -> Msg {
        let n = self.mirror.len().min(ipc::MAX_BYTES);
        let m = ipc::pack(0, &self.mirror.make_contiguous()[..n]);
        self.mirror.drain(..n);
        m
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

    /// One request; `None`: reply deferred (token kept).
    fn request(&mut self, rx: &Received, buf: &mut [u8]) -> Option<Msg> {
        let terminal = rx.badge == proto::BADGE_TERMINAL;
        Some(match rx.msg.label() {
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, buf);
                self.out(&buf[..n]);
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
                if self.mirror_reader.is_some() {
                    Msg::error(-EBUSY)
                } else if self.mirror.is_empty() {
                    self.mirror_reader = Some(rx.token);
                    return None;
                } else {
                    self.take_mirror()
                }
            }
            proto::INPUT | proto::OUTPUT => Msg::error(-EPERM),
            _ => Msg::error(-ENOSYS),
        })
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let ep = slots::DRV_SERVICE;
    let base = sys::mmio_map(slots::DRV_MMIO, UART_A, 0x1000).expect("mmio map");
    let mut c = Console {
        uart: Uart {
            base: base as usize,
        },
        line: Vec::with_capacity(256),
        lines: VecDeque::new(),
        reader: None,
        mirror: VecDeque::with_capacity(MIRROR_BYTES),
        mirror_reader: None,
    };

    let notif = sys::notify_create().expect("notify create");
    sys::irq_bind(slots::DRV_IRQ, notif, 1).expect("irq bind");
    sys::notify_bind(notif).expect("notify bind");
    c.uart.wr(IER, IER_RX);

    let mut buf = [0u8; ipc::MAX_BYTES];
    let mut rx = ipc::recv(ep).expect("recv");
    loop {
        let reply = if rx.msg.is_notify() {
            // Receive interrupt: drain the FIFO and edit.
            let mut n = 0;
            while let Some(b) = c.uart.get() {
                buf[n] = b;
                n += 1;
                if n == buf.len() {
                    c.feed(&buf[..n]);
                    n = 0;
                }
            }
            c.feed(&buf[..n]);
            let _ = sys::irq_ack(slots::DRV_IRQ);
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

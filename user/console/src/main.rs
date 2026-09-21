//! Pilote console : UART-A du Tegra X1, en espace utilisateur.
//!
//! Il n'a acces qu'a une page de registres (projetee par le noyau) et a une
//! seule ligne d'interruption. Sa boucle attend a la fois les requetes des
//! clients et l'interruption de reception, grace a une notification liee :
//! un seul thread, un seul appel bloquant.
//!
//! Il assure aussi l'edition de ligne (echo, effacement) : les programmes ne
//! recoivent que des lignes completes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::proto::console as proto;
use libnx::abi::slots;
use libnx::ipc::{self, Msg};
use libnx::sys;

const UART_A: u64 = 0x7000_6000;

// Registres 16550, espacement de 4 octets.
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
    fn write(&self, data: &[u8]) {
        for &b in data {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
    }
    fn get(&self) -> Option<u8> {
        (self.rd(LSR) & LSR_RDR != 0).then(|| self.rd(THR_RBR) as u8)
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let ep = slots::DRV_SERVICE;
    let base = sys::mmio_map(slots::DRV_MMIO, UART_A, 0x1000).expect("mmio");
    let uart = Uart { base: base as usize };

    let notif = sys::notify_create().expect("notification");
    sys::irq_bind(slots::DRV_IRQ, notif, 1).expect("irq_bind");
    sys::notify_bind(notif).expect("notify_bind");
    uart.wr(IER, IER_RX);

    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut lines: VecDeque<Vec<u8>> = VecDeque::new();
    let mut reader: Option<u64> = None; // client en attente d'une ligne
    let mut buf = [0u8; ipc::MAX_BYTES];

    let mut rx = ipc::recv(ep).expect("recv");
    loop {
        if rx.msg.is_notify() {
            // Interruption de reception : vidage de la FIFO et edition.
            while let Some(b) = uart.get() {
                match b {
                    b'\r' | b'\n' => {
                        uart.write(b"\n");
                        lines.push_back(core::mem::take(&mut line));
                    }
                    0x08 | 0x7F => {
                        // Un caractere UTF-8 entier (octets de continuation compris).
                        let mut erased = false;
                        while let Some(c) = line.pop() {
                            erased = true;
                            if c & 0xC0 != 0x80 {
                                break;
                            }
                        }
                        if erased {
                            uart.write(b"\x08 \x08");
                        }
                    }
                    0x20..=0x7E | 0x80..=0xFF => {
                        if line.len() < 1000 {
                            line.push(b);
                            uart.put(b);
                        }
                    }
                    _ => {}
                }
            }
            let _ = sys::irq_ack(slots::DRV_IRQ);
            if let Some(tok) = reader {
                if let Some(l) = lines.pop_front() {
                    let _ = ipc::reply_to(tok, &ipc::pack(0, &l));
                    reader = None;
                }
            }
            rx = ipc::recv(ep).expect("recv");
            continue;
        }

        let reply = match rx.msg.label() {
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                uart.write(&buf[..n]);
                Msg::new(0, 0, 0)
            }
            proto::READ_LINE => match lines.pop_front() {
                Some(l) => ipc::pack(0, &l),
                None => {
                    // Reponse differee : on la portera a la prochaine ligne.
                    reader = Some(rx.token);
                    rx = ipc::recv(ep).expect("recv");
                    continue;
                }
            },
            _ => Msg::new(libnx::abi::err::ENOSYS as u32, 0, 0),
        };
        rx = match ipc::reply_recv(ep, &reply) {
            Ok(r) => r,
            Err(_) => ipc::recv(ep).expect("recv"),
        };
    }
}

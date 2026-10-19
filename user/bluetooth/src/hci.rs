//! HCI over H4 (Bluetooth Core spec Vol 4 Part E, Vol 4 Part A):
//! packets cut out of the UART byte stream, commands sent within the
//! controller's credits, ACL data out.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::time;

use crate::uart::Uart;

const H4_CMD: u8 = 0x01;
const H4_ACL: u8 = 0x02;
const H4_EVT: u8 = 0x04;

pub const OP_INQUIRY: u16 = 0x0401;
pub const OP_INQUIRY_CANCEL: u16 = 0x0402;
pub const OP_CREATE_CONN: u16 = 0x0405;
pub const OP_DISCONNECT: u16 = 0x0406;
pub const OP_ACCEPT_CONN: u16 = 0x0409;
pub const OP_REJECT_CONN: u16 = 0x040A;
pub const OP_LINK_KEY_REPLY: u16 = 0x040B;
pub const OP_LINK_KEY_NEG_REPLY: u16 = 0x040C;
pub const OP_PIN_REPLY: u16 = 0x040D;
pub const OP_AUTH_REQUESTED: u16 = 0x0411;
pub const OP_SET_ENCRYPTION: u16 = 0x0413;
pub const OP_REMOTE_NAME: u16 = 0x0419;
pub const OP_IO_CAP_REPLY: u16 = 0x042B;
pub const OP_USER_CONFIRM_REPLY: u16 = 0x042C;
pub const OP_SNIFF_MODE: u16 = 0x0803;
pub const OP_DEFAULT_LINK_POLICY: u16 = 0x080F;
pub const OP_SET_EVENT_MASK: u16 = 0x0C01;
pub const OP_RESET: u16 = 0x0C03;
pub const OP_WRITE_LOCAL_NAME: u16 = 0x0C13;
pub const OP_READ_LOCAL_NAME: u16 = 0x0C14;
pub const OP_WRITE_CONN_ACCEPT_TIMEOUT: u16 = 0x0C16;
pub const OP_READ_PAGE_TIMEOUT: u16 = 0x0C17;
pub const OP_WRITE_PAGE_TIMEOUT: u16 = 0x0C18;
pub const OP_WRITE_SCAN_ENABLE: u16 = 0x0C1A;
pub const OP_WRITE_CLASS: u16 = 0x0C24;
pub const OP_WRITE_INQUIRY_MODE: u16 = 0x0C45;
pub const OP_WRITE_SSP_MODE: u16 = 0x0C56;
pub const OP_READ_VERSION: u16 = 0x1001;
pub const OP_READ_BUFFER_SIZE: u16 = 0x1005;
pub const OP_READ_BDADDR: u16 = 0x1009;
/// Broadcom vendor commands (Linux `btbcm.c`, `hci_bcm.c`).
pub const OP_BCM_WRITE_BDADDR: u16 = 0xFC01;
pub const OP_BCM_UART_BAUD: u16 = 0xFC18;
pub const OP_BCM_SLEEP_MODE: u16 = 0xFC27;
pub const OP_BCM_DOWNLOAD_MINIDRIVER: u16 = 0xFC2E;

pub const EV_INQUIRY_COMPLETE: u8 = 0x01;
pub const EV_INQUIRY_RESULT: u8 = 0x02;
pub const EV_CONN_COMPLETE: u8 = 0x03;
pub const EV_CONN_REQUEST: u8 = 0x04;
pub const EV_DISCONN_COMPLETE: u8 = 0x05;
pub const EV_AUTH_COMPLETE: u8 = 0x06;
pub const EV_REMOTE_NAME: u8 = 0x07;
pub const EV_ENCRYPT_CHANGE: u8 = 0x08;
pub const EV_CMD_COMPLETE: u8 = 0x0E;
pub const EV_CMD_STATUS: u8 = 0x0F;
pub const EV_HW_ERROR: u8 = 0x10;
pub const EV_ROLE_CHANGE: u8 = 0x12;
pub const EV_NUM_COMPLETED: u8 = 0x13;
pub const EV_MODE_CHANGE: u8 = 0x14;
pub const EV_PIN_REQUEST: u8 = 0x16;
pub const EV_LINK_KEY_REQUEST: u8 = 0x17;
pub const EV_LINK_KEY_NOTIFY: u8 = 0x18;
pub const EV_INQUIRY_RESULT_RSSI: u8 = 0x22;
pub const EV_EXT_INQUIRY_RESULT: u8 = 0x2F;
pub const EV_IO_CAP_REQUEST: u8 = 0x31;
pub const EV_IO_CAP_RESPONSE: u8 = 0x32;
pub const EV_USER_CONFIRM: u8 = 0x33;
pub const EV_SSP_COMPLETE: u8 = 0x36;

/// ACL packet boundary flags: first packet (flushable), continuation.
pub const PB_START: u16 = 0b10;
pub const PB_CONT: u16 = 0b01;

pub enum Packet {
    /// Event code, parameters.
    Event(u8, Vec<u8>),
    /// Handle with its flags, data.
    Acl(u16, Vec<u8>),
}

pub fn le16(b: &[u8]) -> u16 {
    b[0] as u16 | (b[1] as u16) << 8
}

pub struct Hci {
    pub uart: Uart,
    raw: Vec<u8>,
    /// Packets received and not handled yet.
    pub queue: VecDeque<Packet>,
    /// Commands the controller takes now, and those waiting for room.
    credits: u8,
    pending: VecDeque<(u16, Vec<u8>)>,
    /// Bytes dropped to find the start of a packet again.
    pub resyncs: u64,
    pub tx_stalls: u64,
    /// Bytes received since the start (a dead line shows 0).
    pub rx_seen: u64,
    /// The first bytes received, for a failure report.
    pub rx_log: Vec<u8>,
    /// Hardware Error events received: the core wants a power cycle.
    pub hw_errors: u64,
}

const RX_LOG: usize = 24;

impl Hci {
    pub fn new(uart: Uart) -> Self {
        Hci {
            uart,
            raw: Vec::new(),
            queue: VecDeque::new(),
            credits: 1,
            pending: VecDeque::new(),
            resyncs: 0,
            tx_stalls: 0,
            rx_seen: 0,
            rx_log: Vec::new(),
            hw_errors: 0,
        }
    }

    /// Forget what was received and queued (new baud rate, power cycle).
    pub fn flush(&mut self) {
        self.raw.clear();
        self.queue.clear();
        self.pending.clear();
        self.credits = 1;
    }

    /// Bytes from the UART, cut into packets.
    pub fn pump(&mut self) {
        let at = self.raw.len();
        let n = self.uart.read(&mut self.raw);
        self.rx_seen += n as u64;
        let room = RX_LOG.saturating_sub(self.rx_log.len());
        self.rx_log
            .extend_from_slice(&self.raw[at..at + n.min(room)]);
        while let Some(&t) = self.raw.first() {
            let need = match t {
                H4_EVT if self.raw.len() >= 3 => 3 + self.raw[2] as usize,
                H4_ACL if self.raw.len() >= 5 => 5 + le16(&self.raw[3..5]) as usize,
                H4_EVT | H4_ACL => break,
                _ => {
                    self.raw.remove(0);
                    self.resyncs += 1;
                    continue;
                }
            };
            if self.raw.len() < need {
                break;
            }
            let pkt = if t == H4_EVT {
                if self.raw[1] == EV_HW_ERROR {
                    self.hw_errors += 1;
                }
                Packet::Event(self.raw[1], self.raw[3..need].to_vec())
            } else {
                Packet::Acl(le16(&self.raw[1..3]), self.raw[5..need].to_vec())
            };
            self.raw.drain(..need);
            self.queue.push_back(pkt);
        }
    }

    fn write_cmd(&mut self, op: u16, params: &[u8]) {
        let mut b = Vec::with_capacity(4 + params.len());
        b.extend_from_slice(&[H4_CMD, op as u8, (op >> 8) as u8, params.len() as u8]);
        b.extend_from_slice(params);
        if !self.uart.write(&b) {
            self.tx_stalls += 1;
        }
    }

    /// A command now if the controller has room for it, else once a
    /// Command Complete or Status gives some (`credit`).
    pub fn command(&mut self, op: u16, params: &[u8]) {
        if self.credits > 0 && self.pending.is_empty() {
            self.credits -= 1;
            self.write_cmd(op, params);
        } else {
            self.pending.push_back((op, params.to_vec()));
        }
    }

    /// Room for `n` commands, as a Command Complete or Status said.
    pub fn credit(&mut self, n: u8) {
        self.credits = n;
        while self.credits > 0 {
            let Some((op, p)) = self.pending.pop_front() else {
                break;
            };
            self.credits -= 1;
            self.write_cmd(op, &p);
        }
    }

    /// One ACL packet (fits the controller's buffer: callers check).
    pub fn acl(&mut self, handle: u16, data: &[u8]) {
        let h = handle | PB_START << 12;
        let mut b = Vec::with_capacity(5 + data.len());
        b.extend_from_slice(&[
            H4_ACL,
            h as u8,
            (h >> 8) as u8,
            data.len() as u8,
            (data.len() >> 8) as u8,
        ]);
        b.extend_from_slice(data);
        if !self.uart.write(&b) {
            self.tx_stalls += 1;
        }
    }

    /// Bring-up: `op` sent and its Command Complete waited for; other
    /// packets stay queued. Ok: the return parameters past the status.
    pub fn call(&mut self, op: u16, params: &[u8], timeout_ms: u64) -> Result<Vec<u8>, String> {
        self.write_cmd(op, params);
        let end = time::now_ns() + timeout_ms * 1_000_000;
        loop {
            self.pump();
            let mut found = None;
            for (i, p) in self.queue.iter().enumerate() {
                let Packet::Event(code, d) = p else { continue };
                if *code == EV_CMD_COMPLETE && d.len() >= 4 && le16(&d[1..3]) == op {
                    found = Some((
                        i,
                        if d[3] == 0 {
                            Ok(d[4..].to_vec())
                        } else {
                            Err(d[3])
                        },
                    ));
                    break;
                }
                if *code == EV_CMD_STATUS && d.len() >= 4 && le16(&d[2..4]) == op && d[0] != 0 {
                    found = Some((i, Err(d[0])));
                    break;
                }
            }
            if let Some((i, r)) = found {
                self.queue.remove(i);
                return r.map_err(|s| format!("command {:#06x}: status {:#04x}", op, s));
            }
            if time::now_ns() > end {
                return Err(format!(
                    "command {:#06x}: no answer at {} baud",
                    op, self.uart.baud
                ));
            }
            time::sleep_us(200);
        }
    }
}

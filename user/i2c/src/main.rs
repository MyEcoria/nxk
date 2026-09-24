//! i2c: six Tegra X1 controllers; short transfers, init on first use,
//! write then read after a stop or a repeated start. Sequences from
//! hekate (bdk/soc/i2c.c).

#![no_std]
#![no_main]

use libnx::abi::err::{EINVAL, EIO, ETIMEDOUT};
use libnx::abi::proto::{clk, i2c as proto};
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::println;

const CNFG: usize = 0x00;
const CMD_ADDR0: usize = 0x04;
const CMD_DATA1: usize = 0x0C;
const CMD_DATA2: usize = 0x10;
const STATUS: usize = 0x1C;
const INT_STATUS: usize = 0x68;
const CLK_DIVISOR: usize = 0x6C;
const BUS_CLEAR_CONFIG: usize = 0x84;
const BUS_CLEAR_STATUS: usize = 0x88;
const CONFIG_LOAD: usize = 0x8C;
const TX_FIFO: usize = 0x50;
const RX_FIFO: usize = 0x54;
const PACKET_TRANSFER_STATUS: usize = 0x58;
const FIFO_CONTROL: usize = 0x5C;
const FIFO_STATUS: usize = 0x60;

const CMD1_READ: u32 = 1 << 6;
const NORMAL_MODE_GO: u32 = 1 << 9;
const PACKET_MODE_GO: u32 = 1 << 10;
const NEW_MASTER_FSM: u32 = 1 << 11;
const DEBOUNCE_CNT_4T: u32 = 2 << 12;
const STATUS_NOACK: u32 = 0xF;
const STATUS_BUSY: u32 = 1 << 8;
const RX_FIFO_FLUSH: u32 = 1 << 0;
const TX_FIFO_FLUSH: u32 = 1 << 1;
const RX_FIFO_FULL_CNT: u32 = 0xF;
/// Packet header words: protocol, then payload size - 1, then flags and
/// address.
const PACKET_PROT_I2C: u32 = 1 << 4;
const HEADER_REP_START: u32 = 1 << 16;
const HEADER_READ: u32 = 1 << 19;

/// Controllers 1-6: (mapped page, offset within the page).
const BUSES: [(usize, usize); 6] = [
    (0, 0x000),
    (0, 0x400),
    (0, 0x500),
    (0, 0x700),
    (1, 0x000),
    (1, 0x100),
];

struct Bus {
    regs: Mmio,
    ready: bool,
}

fn load_cfg(b: &Mmio) {
    b.w32(CONFIG_LOAD, (1 << 5) | (1 << 2) | 1);
    b.wait(CONFIG_LOAD, 1, 0, 20);
}

fn init(b: &Mmio) {
    b.w32(CLK_DIVISOR, (5 << 16) | 1);
    b.w32(BUS_CLEAR_CONFIG, (9 << 16) | 2 | 1);
    load_cfg(b);
    b.wait(INT_STATUS, 1 << 11, 1 << 11, 250);
    let _ = b.r32(BUS_CLEAR_STATUS);
    b.w32(INT_STATUS, b.r32(INT_STATUS));
}

fn run(b: &Mmio, cnfg: u32) -> Result<(), i64> {
    b.w32(CNFG, cnfg);
    load_cfg(b);
    b.w32(CNFG, (b.r32(CNFG) & !NORMAL_MODE_GO) | NORMAL_MODE_GO);
    // 8 bytes at 100 kHz: under a millisecond.
    if !b.wait(STATUS, STATUS_BUSY, 0, 100_000) {
        return Err(-ETIMEDOUT);
    }
    if b.r32(STATUS) & STATUS_NOACK != 0 {
        return Err(-EIO);
    }
    Ok(())
}

fn write(b: &Mmio, addr: u32, data: &[u8]) -> Result<(), i64> {
    let mut w = [0u8; 8];
    w[..data.len()].copy_from_slice(data);
    b.w32(CMD_ADDR0, addr << 1);
    b.w32(CMD_DATA1, u32::from_le_bytes(w[..4].try_into().unwrap()));
    b.w32(CMD_DATA2, u32::from_le_bytes(w[4..].try_into().unwrap()));
    run(
        b,
        ((data.len() as u32 - 1) << 1) | DEBOUNCE_CNT_4T | NEW_MASTER_FSM,
    )
}

fn read(b: &Mmio, addr: u32, out: &mut [u8]) -> Result<(), i64> {
    b.w32(CMD_ADDR0, (addr << 1) | 1);
    run(
        b,
        ((out.len() as u32 - 1) << 1) | DEBOUNCE_CNT_4T | NEW_MASTER_FSM | CMD1_READ,
    )?;
    let mut w = [0u8; 8];
    w[..4].copy_from_slice(&b.r32(CMD_DATA1).to_le_bytes());
    w[4..].copy_from_slice(&b.r32(CMD_DATA2).to_le_bytes());
    out.copy_from_slice(&w[..out.len()]);
    Ok(())
}

/// Write, repeated start, read: packet mode, as hekate i2c_xfer_packet
/// (normal mode always ends the write with a stop).
fn write_read(b: &Mmio, addr: u32, data: &[u8], out: &mut [u8]) -> Result<(), i64> {
    b.w32(CMD_ADDR0, (addr << 1) | 1);
    b.w32(CNFG, DEBOUNCE_CNT_4T | NEW_MASTER_FSM | CMD1_READ);
    b.w32(FIFO_CONTROL, RX_FIFO_FLUSH | TX_FIFO_FLUSH);
    load_cfg(b);
    b.w32(CNFG, (b.r32(CNFG) & !NORMAL_MODE_GO) | PACKET_MODE_GO);
    let r = packets(b, addr, data, out);
    let r = if b.r32(STATUS) & STATUS_NOACK != 0 {
        Err(-EIO)
    } else {
        r
    };
    // Let the stop go out before leaving packet mode.
    libnx::time::sleep_us(20);
    b.w32(CNFG, b.r32(CNFG) & !(PACKET_MODE_GO | NORMAL_MODE_GO));
    r
}

/// Wait for `done`, giving up early on a NACK.
fn packet_wait(b: &Mmio, done: impl Fn(&Mmio) -> bool) -> Result<(), i64> {
    let mut nack = false;
    let ok = libnx::time::poll(200_000, || {
        nack = b.r32(STATUS) & STATUS_NOACK != 0;
        nack || done(b)
    });
    match (ok, nack) {
        (_, true) => Err(-EIO),
        (false, _) => Err(-ETIMEDOUT),
        _ => Ok(()),
    }
}

fn packets(b: &Mmio, addr: u32, data: &[u8], out: &mut [u8]) -> Result<(), i64> {
    b.w32(TX_FIFO, PACKET_PROT_I2C);
    b.w32(TX_FIFO, data.len() as u32 - 1);
    b.w32(TX_FIFO, HEADER_REP_START | addr << 1);
    for c in data.chunks(4) {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        b.w32(TX_FIFO, u32::from_le_bytes(w));
    }
    // Bytes sent (minus one), in bits 15:4.
    let sent = data.len() as u32 - 1;
    packet_wait(b, |b| (b.r32(PACKET_TRANSFER_STATUS) >> 4) & 0xFFF == sent)?;
    b.w32(TX_FIFO, PACKET_PROT_I2C);
    b.w32(TX_FIFO, out.len() as u32 - 1);
    b.w32(TX_FIFO, HEADER_READ | addr << 1);
    for c in out.chunks_mut(4) {
        packet_wait(b, |b| b.r32(FIFO_STATUS) & RX_FIFO_FULL_CNT != 0)?;
        let w = b.r32(RX_FIFO).to_le_bytes();
        c.copy_from_slice(&w[..c.len()]);
    }
    Ok(())
}

libnx::entry!(main);

fn main() -> i32 {
    let pages = [
        Mmio::map(drv::dev_slot(0), 0x7000_C000, 0x1000).expect("i2c: registers"),
        Mmio::map(drv::dev_slot(1), 0x7000_D000, 0x1000).expect("i2c: registers"),
    ];
    let mut buses: [Bus; 6] = core::array::from_fn(|k| Bus {
        regs: pages[BUSES[k].0].offset(BUSES[k].1),
        ready: false,
    });
    let ep = drv::publish("i2c").expect("i2c: publish");
    println!("[i2c] six I2C buses ready");

    let mut data = [0u8; 16];
    drv::serve(ep, |rx| {
        if rx.msg.label() != proto::TRANSFER {
            return Some(Msg::error(-EINVAL));
        }
        let h = ipc::header(&rx.msg, 0);
        let (bus, addr, rlen) = (
            (h & 0xFF) as usize,
            ((h >> 8) & 0x7F) as u32,
            ((h >> 16) & 0xFF) as usize,
        );
        let rep_start = h & proto::REP_START != 0;
        let wlen = ipc::unpack_at(&rx.msg, 1, &mut data);
        if !(1..=6).contains(&bus)
            || wlen > 8
            || rlen > 8
            || (wlen == 0 && rlen == 0)
            || rep_start && (wlen == 0 || rlen == 0)
        {
            return Some(Msg::error(-EINVAL));
        }
        let b = &mut buses[bus - 1];
        if !b.ready {
            let _ = libnx::clk::enable(clk::I2C1 + bus as u64 - 1);
            init(&b.regs);
            b.ready = true;
        }
        let mut out = [0u8; 8];
        let r = (|| {
            if rep_start {
                return write_read(&b.regs, addr, &data[..wlen], &mut out[..rlen]);
            }
            if wlen > 0 {
                write(&b.regs, addr, &data[..wlen])?;
            }
            if rlen > 0 {
                read(&b.regs, addr, &mut out[..rlen])?;
            }
            Ok::<(), i64>(())
        })();
        Some(match r {
            Ok(()) => ipc::pack(0, &out[..rlen]),
            Err(e) => Msg::error(e),
        })
    })
}

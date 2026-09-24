//! i2c: six Tegra X1 controllers; short transfers, init on first use.
//! Sequences from hekate (bdk/soc/i2c.c): normal mode, packet mode for
//! a write and a read joined by a repeated start.

#![no_std]
#![no_main]

use libnx::abi::err::{EINVAL, EIO, ETIMEDOUT};
use libnx::abi::proto::{clk, i2c as proto};
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::{println, time};

const CNFG: usize = 0x00;
const CMD_ADDR0: usize = 0x04;
const CMD_DATA1: usize = 0x0C;
const CMD_DATA2: usize = 0x10;
const STATUS: usize = 0x1C;
const TX_FIFO: usize = 0x50;
const RX_FIFO: usize = 0x54;
const FIFO_CONTROL: usize = 0x5C;
const FIFO_STATUS: usize = 0x60;
const INT_STATUS: usize = 0x68;
const CLK_DIVISOR: usize = 0x6C;
const BUS_CLEAR_CONFIG: usize = 0x84;
const BUS_CLEAR_STATUS: usize = 0x88;
const CONFIG_LOAD: usize = 0x8C;

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
const TX_FIFO_EMPTY_CNT: u32 = 0xF << 4;
const INT_ARB_LOST: u32 = 1 << 2;
const INT_NO_ACK: u32 = 1 << 3;
/// Packet header word 0 (I2C protocol) and I2C-specific header bits.
const PACKET_PROT_I2C: u32 = 1 << 4;
const HEADER_REP_START: u32 = 1 << 16;
const HEADER_READ: u32 = 1 << 19;
/// hekate's packet timeout.
const PACKET_US: u64 = 200_000;
/// Bus clock of controllers 1-6 in kHz (clk's source divisors, as
/// hekate bdk/soc/clock.c: 100 kHz on 1, 4 and 6, 400 kHz on the others).
const KHZ: [u64; 6] = [100, 400, 400, 100, 400, 100];

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

/// Wait for `bytes` (addresses included) to cross the bus: one sleep for
/// their time, 9 bits each plus start and stop. `poll`'s growing naps
/// cost an interrupt each, about 10 per touch read (polled at 2 Hz).
fn on_bus(khz: u64, bytes: usize) {
    let us = (9 * bytes as u64 + 3) * 1000 / khz;
    if us > 20 {
        time::sleep_us(us);
    }
}

fn spin_us(us: u64) {
    let end = time::now_ns() + us * 1000;
    while time::now_ns() < end {
        core::hint::spin_loop();
    }
}

fn run(b: &Mmio, cnfg: u32, khz: u64, bytes: usize) -> Result<(), i64> {
    b.w32(CNFG, cnfg);
    load_cfg(b);
    b.w32(CNFG, (b.r32(CNFG) & !NORMAL_MODE_GO) | NORMAL_MODE_GO);
    on_bus(khz, bytes);
    // 8 bytes at 100 kHz: under a millisecond.
    if !b.wait(STATUS, STATUS_BUSY, 0, 100_000) {
        return Err(-ETIMEDOUT);
    }
    if b.r32(STATUS) & STATUS_NOACK != 0 {
        return Err(-EIO);
    }
    Ok(())
}

fn write(b: &Mmio, khz: u64, addr: u32, data: &[u8]) -> Result<(), i64> {
    let mut w = [0u8; 8];
    w[..data.len()].copy_from_slice(data);
    b.w32(CMD_ADDR0, addr << 1);
    b.w32(CMD_DATA1, u32::from_le_bytes(w[..4].try_into().unwrap()));
    b.w32(CMD_DATA2, u32::from_le_bytes(w[4..].try_into().unwrap()));
    run(
        b,
        ((data.len() as u32 - 1) << 1) | DEBOUNCE_CNT_4T | NEW_MASTER_FSM,
        khz,
        1 + data.len(),
    )
}

fn read(b: &Mmio, khz: u64, addr: u32, out: &mut [u8]) -> Result<(), i64> {
    b.w32(CMD_ADDR0, (addr << 1) | 1);
    run(
        b,
        ((out.len() as u32 - 1) << 1) | DEBOUNCE_CNT_4T | NEW_MASTER_FSM | CMD1_READ,
        khz,
        1 + out.len(),
    )?;
    let mut w = [0u8; 8];
    w[..4].copy_from_slice(&b.r32(CMD_DATA1).to_le_bytes());
    w[4..].copy_from_slice(&b.r32(CMD_DATA2).to_le_bytes());
    out.copy_from_slice(&w[..out.len()]);
    Ok(())
}

/// Write `data` then read `out`, a repeated start between: packet mode,
/// both packets queued at once (the TX FIFO holds 8 words).
fn write_read(b: &Mmio, khz: u64, addr: u32, data: &[u8], out: &mut [u8]) -> Result<(), i64> {
    b.w32(CMD_ADDR0, (addr << 1) | 1);
    b.w32(CNFG, DEBOUNCE_CNT_4T | NEW_MASTER_FSM | CMD1_READ);
    b.w32(FIFO_CONTROL, RX_FIFO_FLUSH | TX_FIFO_FLUSH);
    b.w32(INT_STATUS, INT_NO_ACK | INT_ARB_LOST);
    load_cfg(b);
    b.w32(CNFG, (b.r32(CNFG) & !NORMAL_MODE_GO) | PACKET_MODE_GO);
    let failed = || b.r32(INT_STATUS) & (INT_NO_ACK | INT_ARB_LOST) != 0;
    let packet = |len: usize, header: u32| {
        b.w32(TX_FIFO, PACKET_PROT_I2C);
        b.w32(TX_FIFO, len as u32 - 1);
        b.w32(TX_FIFO, header | addr << 1);
    };
    let r = (|| {
        packet(data.len(), HEADER_REP_START);
        for c in data.chunks(4) {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            b.w32(TX_FIFO, u32::from_le_bytes(w));
        }
        if !time::poll(PACKET_US, || {
            b.r32(FIFO_STATUS) & TX_FIFO_EMPTY_CNT >= 3 << 4
        }) {
            return Err(-ETIMEDOUT);
        }
        packet(out.len(), HEADER_READ);
        on_bus(khz, 2 + data.len() + out.len());
        for c in out.chunks_mut(4) {
            if !time::poll(PACKET_US, || {
                failed() || b.r32(FIFO_STATUS) & RX_FIFO_FULL_CNT != 0
            }) {
                return Err(-ETIMEDOUT);
            }
            if failed() {
                return Err(-EIO);
            }
            c.copy_from_slice(&b.r32(RX_FIFO).to_le_bytes()[..c.len()]);
        }
        Ok(())
    })();
    // Let the stop go out (a few bit times), then leave packet mode.
    spin_us(20);
    b.modify(CNFG, PACKET_MODE_GO | NORMAL_MODE_GO, 0);
    if r.is_ok() && failed() {
        return Err(-EIO);
    }
    r
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
        let (bus, addr, rlen, joined) = (
            (h & 0xFF) as usize,
            ((h >> 8) & 0x7F) as u32,
            ((h >> 16) & 0xFF) as usize,
            h & proto::REPEATED_START != 0,
        );
        let wlen = ipc::unpack_at(&rx.msg, 1, &mut data);
        if !(1..=6).contains(&bus)
            || wlen > 8
            || rlen > 8
            || (wlen == 0 && rlen == 0)
            || (joined && (wlen == 0 || rlen == 0))
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
        let khz = KHZ[bus - 1];
        let r = (|| {
            if joined {
                return write_read(&b.regs, khz, addr, &data[..wlen], &mut out[..rlen]);
            }
            if wlen > 0 {
                write(&b.regs, khz, addr, &data[..wlen])?;
            }
            if rlen > 0 {
                read(&b.regs, khz, addr, &mut out[..rlen])?;
            }
            Ok::<(), i64>(())
        })();
        Some(match r {
            Ok(()) => ipc::pack(0, &out[..rlen]),
            Err(e) => Msg::error(e),
        })
    })
}

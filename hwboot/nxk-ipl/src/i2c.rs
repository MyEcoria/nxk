//! I2C masters in normal mode (hekate bdk/soc/i2c.c): I2C5 reaches the
//! PMIC and the CPU/GPU regulators, I2C1 the other power chips.

use crate::reg::{rd, wr, I2C};
use crate::timer::{now_ms, usleep};

pub const I2C_1: u32 = 0;
pub const I2C_5: u32 = 4;
const BASE_OFFSETS: [u32; 6] = [0x0, 0x400, 0x500, 0x700, 0x1000, 0x1100];

const CNFG: u32 = 0x00;
const CMD1_READ: u32 = 1 << 6;
const NORMAL_MODE_GO: u32 = 1 << 9;
const NEW_MASTER_FSM: u32 = 1 << 11;
const DEBOUNCE_CNT_4T: u32 = 2 << 12;
const CMD_ADDR0: u32 = 0x04;
const CMD_DATA1: u32 = 0x0C;
const CMD_DATA2: u32 = 0x10;
const STATUS: u32 = 0x1C;
const STATUS_NOACK: u32 = 0xF;
const STATUS_BUSY: u32 = 1 << 8;
const INT_STATUS: u32 = 0x68;
const BUS_CLEAR_DONE: u32 = 1 << 11;
const CLK_DIVISOR: u32 = 0x6C;
const BUS_CLEAR_CONFIG: u32 = 0x84;
const BC_ENABLE: u32 = 1 << 0;
const BC_TERMINATE: u32 = 1 << 1;
const BUS_CLEAR_STATUS: u32 = 0x88;
const CONFIG_LOAD: u32 = 0x8C;
const MSTR_CONFIG_LOAD: u32 = 1 << 0;
const TIMEOUT_CONFIG_LOAD: u32 = 1 << 2;

/// The transfer got no answer, or did not end in time.
#[derive(Debug)]
pub struct Nack;

fn base(idx: u32) -> u32 {
    I2C + BASE_OFFSETS[idx as usize]
}

fn load_cfg_wait(b: u32) {
    wr(b + CONFIG_LOAD, 1 << 5 | TIMEOUT_CONFIG_LOAD | MSTR_CONFIG_LOAD);
    for _ in 0..20 {
        if rd(b + CONFIG_LOAD) & MSTR_CONFIG_LOAD == 0 {
            break;
        }
        usleep(1);
    }
}

/// Start the configured transfer and wait for its end.
fn go(b: u32) -> Result<(), Nack> {
    wr(b + CNFG, rd(b + CNFG) & !NORMAL_MODE_GO | NORMAL_MODE_GO);
    // 8 bytes at 100 kHz take 0.74 ms.
    let timeout = now_ms().wrapping_add(100);
    while rd(b + STATUS) & STATUS_BUSY != 0 {
        if now_ms() > timeout {
            return Err(Nack);
        }
    }
    if rd(b + STATUS) & STATUS_NOACK != 0 {
        return Err(Nack);
    }
    Ok(())
}

/// `_i2c_send_normal`: up to 8 bytes.
fn send_normal(idx: u32, dev: u32, buf: &[u8]) -> Result<(), Nack> {
    let size = buf.len() as u32;
    if size == 0 || size > 8 {
        return Err(Nack);
    }
    let b = base(idx);
    let word = |part: &[u8]| part.iter().rev().fold(0u32, |w, &x| w << 8 | x as u32);
    wr(b + CMD_ADDR0, dev << 1);
    if size > 4 {
        wr(b + CMD_DATA1, word(&buf[..4]));
        wr(b + CMD_DATA2, word(&buf[4..]));
    } else {
        wr(b + CMD_DATA1, word(buf));
    }
    wr(b + CNFG, (size - 1) << 1 | DEBOUNCE_CNT_4T | NEW_MASTER_FSM);
    load_cfg_wait(b);
    go(b)
}

/// `_i2c_recv_normal`: up to 8 bytes.
fn recv_normal(idx: u32, dev: u32, buf: &mut [u8]) -> Result<(), Nack> {
    let size = buf.len() as u32;
    if size == 0 || size > 8 {
        return Err(Nack);
    }
    let b = base(idx);
    wr(b + CMD_ADDR0, dev << 1 | 1);
    wr(b + CNFG, (size - 1) << 1 | DEBOUNCE_CNT_4T | NEW_MASTER_FSM | CMD1_READ);
    load_cfg_wait(b);
    go(b)?;
    let lo = rd(b + CMD_DATA1);
    let hi = if size > 4 { rd(b + CMD_DATA2) } else { 0 };
    for (i, x) in buf.iter_mut().enumerate() {
        *x = if i < 4 { lo >> (8 * i) } else { hi >> (8 * (i - 4)) } as u8;
    }
    Ok(())
}

/// `i2c_init`: dividers, then a bus clear.
pub fn init(idx: u32) {
    let b = base(idx);
    wr(b + CLK_DIVISOR, 5 << 16 | 1);
    wr(b + BUS_CLEAR_CONFIG, 9 << 16 | BC_TERMINATE | BC_ENABLE);
    load_cfg_wait(b);
    for _ in 0..10 {
        if rd(b + INT_STATUS) & BUS_CLEAR_DONE != 0 {
            break;
        }
        usleep(25);
    }
    let _ = rd(b + BUS_CLEAR_STATUS);
    wr(b + INT_STATUS, rd(b + INT_STATUS));
}

/// `i2c_send_buf_small`: register `reg`, then up to 7 bytes.
pub fn send_buf_small(idx: u32, dev: u32, reg: u32, buf: &[u8]) -> Result<(), Nack> {
    if buf.len() > 7 {
        return Err(Nack);
    }
    let mut tmp = [0u8; 8];
    tmp[0] = reg as u8;
    tmp[1..=buf.len()].copy_from_slice(buf);
    send_normal(idx, dev, &tmp[..buf.len() + 1])
}

/// `i2c_recv_buf_small`.
pub fn recv_buf_small(buf: &mut [u8], idx: u32, dev: u32, reg: u32) -> Result<(), Nack> {
    send_normal(idx, dev, &[reg as u8])?;
    recv_normal(idx, dev, buf)
}

/// `i2c_send_byte`.
pub fn send_byte(idx: u32, dev: u32, reg: u32, val: u8) -> Result<(), Nack> {
    send_buf_small(idx, dev, reg, &[val])
}

/// `i2c_recv_byte`: 0 when nobody answers, as hekate.
pub fn recv_byte(idx: u32, dev: u32, reg: u32) -> u8 {
    let mut tmp = [0u8];
    let _ = recv_buf_small(&mut tmp, idx, dev, reg);
    tmp[0]
}

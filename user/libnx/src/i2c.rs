//! I2C service client.

use nxk_abi::proto::i2c as proto;

use crate::ipc;

#[derive(Clone, Copy)]
pub struct I2c {
    ep: u32,
}

impl I2c {
    pub fn connect() -> Result<Self, i64> {
        Ok(Self {
            ep: crate::ns::wait_for("i2c")?,
        })
    }

    fn xfer(&self, bus: u8, addr: u8, w: &[u8], r: &mut [u8], flags: u64) -> Result<(), i64> {
        let head = [proto::header(bus as u64, addr as u64, r.len() as u64) | flags];
        let m = ipc::call_checked(self.ep, &ipc::pack_at(proto::TRANSFER, &head, w))?;
        if ipc::unpack(&m, r) != r.len() {
            return Err(-nxk_abi::err::EIO);
        }
        Ok(())
    }

    /// Write `w` then read `r.len()` bytes (8 at most each) on
    /// device `addr` of bus `bus` (1 to 6).
    pub fn transfer(&self, bus: u8, addr: u8, w: &[u8], r: &mut [u8]) -> Result<(), i64> {
        self.xfer(bus, addr, w, r, 0)
    }

    /// Like `transfer` (both `w` and `r` non-empty), with a repeated
    /// start instead of a stop between the write and the read.
    pub fn write_read(&self, bus: u8, addr: u8, w: &[u8], r: &mut [u8]) -> Result<(), i64> {
        self.xfer(bus, addr, w, r, proto::REPEATED_START)
    }

    /// Read an 8-bit register.
    pub fn read_reg(&self, bus: u8, addr: u8, reg: u8) -> Result<u8, i64> {
        let mut v = [0u8];
        self.transfer(bus, addr, &[reg], &mut v)?;
        Ok(v[0])
    }

    pub fn write_reg(&self, bus: u8, addr: u8, reg: u8, val: u8) -> Result<(), i64> {
        self.transfer(bus, addr, &[reg, val], &mut [])
    }
}

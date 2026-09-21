//! Client du service I2C.

use nxk_abi::proto::i2c as proto;

use crate::ipc;

#[derive(Clone, Copy)]
pub struct I2c {
    ep: u32,
}

impl I2c {
    pub fn connect() -> Result<Self, i64> {
        Ok(Self { ep: crate::ns::wait_for("i2c")? })
    }

    /// Ecrit `w` puis lit `r.len()` octets (8 au plus chacun) sur le
    /// peripherique `addr` du bus `bus` (1 a 6).
    pub fn transfer(&self, bus: u8, addr: u8, w: &[u8], r: &mut [u8]) -> Result<(), i64> {
        let head = [proto::header(bus as u64, addr as u64, r.len() as u64)];
        let m = ipc::call(self.ep, &ipc::pack_at(proto::TRANSFER, &head, w))?;
        if m.label() != 0 {
            return Err(-(m.label() as i64));
        }
        if ipc::unpack(&m, r) != r.len() {
            return Err(-nxk_abi::err::EIO);
        }
        Ok(())
    }

    /// Lit un registre de 8 bits.
    pub fn read_reg(&self, bus: u8, addr: u8, reg: u8) -> Result<u8, i64> {
        let mut v = [0u8];
        self.transfer(bus, addr, &[reg], &mut v)?;
        Ok(v[0])
    }

    pub fn write_reg(&self, bus: u8, addr: u8, reg: u8, val: u8) -> Result<(), i64> {
        self.transfer(bus, addr, &[reg, val], &mut [])
    }
}

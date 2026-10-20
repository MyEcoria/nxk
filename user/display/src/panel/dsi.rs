//! DSI A host: packets to and from the panel (hekate `_display_dsi_*`,
//! `display_dsi_read`). Every wait is bounded:
//! a model without a panel answers nothing.

use libnx::drv::Mmio;
use libnx::time;

use super::tables::*;

/// DSI data types.
pub const DCS_SHORT_WRITE: u32 = 0x05;
const DCS_READ: u32 = 0x06;
const SET_MAXIMUM_RETURN_PACKET_SIZE: u32 = 0x37;
/// DSI reply types.
const ESCAPE_CMD: u32 = 0x87;
const GEN_LONG_RD_RES: u32 = 0x1A;
const DCS_LONG_RD_RES: u32 = 0x1C;
const GEN_1_BYTE_RD_RES: u32 = 0x11;
const DCS_1_BYTE_RD_RES: u32 = 0x21;
const GEN_2_BYTE_RD_RES: u32 = 0x12;
const DCS_2_BYTE_RD_RES: u32 = 0x22;
/// MIPI DCS commands.
pub const DCS_GET_DISPLAY_ID: u32 = 0x04;
pub const DCS_ENTER_SLEEP: u32 = 0x10;
pub const DCS_EXIT_SLEEP: u32 = 0x11;
pub const DCS_DISPLAY_OFF: u32 = 0x28;
pub const DCS_DISPLAY_ON: u32 = 0x29;
/// A frame and the packet's transmission (hekate `display_end`).
pub const FRAME_US: u64 = 16_666 + 230;

#[derive(Clone, Copy)]
pub struct Dsi {
    pub dsi: Mmio,
}

impl Dsi {
    pub fn table(&self, t: Table) {
        for &(reg, v) in t {
            self.dsi.w32(reg * 4, v);
        }
    }

    /// DSI interface off, then settled.
    pub fn soft_reset(&self) {
        self.dsi.w32(DSI_POWER_CONTROL * 4, 0);
        time::sleep_us(10);
    }

    /// A short packet of data type `cmd` with `param`, triggered at once;
    /// `wait_us` is what the panel needs after it.
    pub fn send(&self, cmd: u32, param: u32, wait_us: u64) {
        self.dsi.w32(DSI_WR_DATA * 4, param << 8 | cmd);
        self.dsi.w32(DSI_TRIGGER * 4, TRIGGER_HOST);
        if wait_us != 0 {
            time::sleep_us(wait_us);
        }
    }

    /// A DCS short write (no parameter).
    pub fn dcs(&self, cmd: u32, wait_us: u64) {
        self.send(DCS_SHORT_WRITE, cmd, wait_us);
    }

    /// Up to `timeout_us` for the bits of `mask` to clear, then 5 us.
    fn wait_clear(&self, reg: usize, mask: u32, timeout_us: u64) {
        let _ = self.dsi.wait(reg * 4, mask, 0, timeout_us);
        time::sleep_us(5);
    }

    fn drain(&self, out: &mut [u32; 32]) -> usize {
        let n = (self.dsi.r32(DSI_STATUS * 4) & STATUS_RX_FIFO_SIZE) as usize;
        if n != 0 {
            self.dsi.w32(DSI_TRIGGER * 4, 0);
        }
        for w in out.iter_mut().take(n) {
            *w = self.dsi.r32(DSI_RD_DATA * 4);
        }
        n
    }

    /// The device's reply turned into the register's bytes (little-endian,
    /// `len` at most 4).
    fn reply(fifo: &[u32; 32], n: usize, len: u32) -> Option<u32> {
        if n < 3 || fifo[0] & 0xFF != ESCAPE_CMD {
            return None;
        }
        let bytes = match fifo[1] & 0xFF {
            GEN_LONG_RD_RES | DCS_LONG_RD_RES => ((fifo[1] >> 8) & 0xFFFF).min(len),
            GEN_1_BYTE_RD_RES | DCS_1_BYTE_RD_RES => 1,
            GEN_2_BYTE_RD_RES | DCS_2_BYTE_RD_RES => 2,
            _ => return None,
        };
        let mask = if bytes >= 4 {
            u32::MAX
        } else {
            (1 << (8 * bytes)) - 1
        };
        Some(fifo[2] & mask)
    }

    /// The bus handed to the panel for its reply (`IMM_BTA`), then read.
    fn collect(&self, len: u32) -> Option<u32> {
        let hc = self.dsi.r32(DSI_HOST_CONTROL * 4);
        self.dsi
            .w32(DSI_HOST_CONTROL * 4, hc | HOST_CONTROL_IMM_BTA);
        self.wait_clear(DSI_HOST_CONTROL, HOST_CONTROL_IMM_BTA, 150_000);
        time::sleep_us(5000);
        let mut fifo = [0u32; 32];
        let n = self.drain(&mut fifo);
        Self::reply(&fifo, n, len)
    }

    /// Register `cmd` of the panel, `len` bytes, in command mode.
    pub fn read(&self, cmd: u32, len: u32) -> Option<u32> {
        self.drain(&mut [0u32; 32]);
        self.send(SET_MAXIMUM_RETURN_PACKET_SIZE, len, 0);
        self.wait_clear(DSI_TRIGGER, TRIGGER_HOST | TRIGGER_VIDEO, 250_000);
        self.send(DCS_READ, cmd, 0);
        self.wait_clear(DSI_TRIGGER, TRIGGER_HOST | TRIGGER_VIDEO, 250_000);
        self.collect(len)
    }

    /// A DCS short write while the panel takes video (hekate
    /// `display_end`): host packets on for one frame, sent in its blanking
    /// without a trigger.
    pub fn dcs_running(&self, cmd: u32) {
        self.dsi.w32(DSI_VIDEO_MODE_CONTROL * 4, CMD_PKT_VID_ENABLE);
        self.dsi.w32(DSI_WR_DATA * 4, cmd << 8 | DCS_SHORT_WRITE);
        time::sleep_us(FRAME_US);
        self.dsi.w32(DSI_VIDEO_MODE_CONTROL * 4, 0);
    }
}

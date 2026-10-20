//! The display path's own bring-up, from hekate's `display_init` and
//! `display_end` (bdk/display/di.c) for the Erista (T210) 6.2" panels:
//! PLLD, the display controller, DSI A, the panel's rails and reset and
//! the panel-specific sequence, by panel id. `power_down` stops all of it
//! (panel asleep, reset low, AVDD off, clocks and PLLD off); `power_up_1`
//! and `power_up_2` bring it back from reset, split at the panel's sleep
//! out so that the caller can use that wait.

mod dsi;
mod soc;
mod tables;

use libnx::drv::{self, Mmio};
use libnx::i2c::I2c;
use libnx::{println, time};

use crate::dcgate;
use dsi::*;
use soc::Soc;
use tables::*;

const CAR: u64 = 0x6000_6000;
const GPIO: u64 = 0x6000_D000;
const PINMUX: u64 = 0x7000_3000;
const MIPI_CAL: u64 = 0x700E_3000;
const APB_MISC: u64 = 0x7000_0000;
/// `APB_MISC_GP_HIDREV`: the chip's major revision in bits 7:4.
const GP_HIDREV: usize = 0x804;
const HIDREV_T210: u32 = 1;
/// Slots after the display controller, PWM and DSI (init's driver table).
const SLOT_CAR: u32 = 3;
const SLOT_GPIO: u32 = 4;
const SLOT_PINMUX: u32 = 5;
const SLOT_MIPI_CAL: u32 = 6;
const SLOT_APB_MISC: u32 = 7;
/// CAR: the display controller, DSI and MIPI CAL clocked and not in reset.
const CAR_RST_DEVICES_L: usize = 0x004;
const CAR_CLK_OUT_ENB_L: usize = 0x010;
const CAR_RST_DEVICES_H: usize = 0x008;
const CAR_CLK_OUT_ENB_H: usize = 0x014;
const CAR_PLLD_BASE: usize = 0xD0;
/// Enabled (30) and locked (27).
const PLLD_RUNNING: u32 = (1 << 30) | (1 << 27);
const CLK_L_DISP1: u32 = 1 << 27;
const CLK_H_DSI_CAL: u32 = (1 << 16) | (1 << 24);

const MIPI_CAL_START: u32 = 0x2A00_0001;
const BIAS_CFG2_CAL: u32 = 0x10010;
const BIAS_CFG1_T210: u32 = 0x300;
const DISP_CLOCK_DIV3: u32 = 4;
const BTA_TIMING_PANEL: u32 = 0x50204;
/// Panel id reads: tries, and the pause between.
const ID_TRIES: u32 = 3;

/// Panels (hekate `di.h`): the decoded id is the board byte over the vendor.
const ID_JDI: u16 = 0x10;
const ID_INL_P062CCA_AZ1: u16 = 0x0F20;
const ID_AUO_A062TAN01: u16 = 0x0F30;
/// What a panel that does not answer decodes to (hekate's 0xCCCCCC).
const ID_UNKNOWN: u16 = 0xCCCC;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Jdi,
    Inl,
    Auo,
    /// Spare parts and clones: sleep out only.
    Other,
}

impl Kind {
    fn of(id: u16) -> Self {
        match id {
            _ if id & 0xFF == ID_JDI => Kind::Jdi,
            ID_INL_P062CCA_AZ1 => Kind::Inl,
            ID_AUO_A062TAN01 => Kind::Auo,
            _ => Kind::Other,
        }
    }

    /// Sleep out to the panel being ready, us.
    fn sleep_out_us(self) -> u64 {
        match self {
            Kind::Other => 120_000,
            _ => 180_000,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Jdi => "JDI",
            Kind::Inl => "InnoLux P062CCA",
            Kind::Auo => "AUO A062TAN",
            Kind::Other => "other",
        }
    }
}

/// The panel id from its raw reply (vendor, model, board).
fn decode_id(raw: u32) -> u16 {
    (((raw >> 8) & 0xFF00) | (raw & 0xFF)) as u16
}

#[derive(Clone, Copy)]
pub struct Panel {
    soc: Soc,
    dsi: Dsi,
    mipi: Mmio,
    dc: Mmio,
    id: u16,
    cold: bool,
}

fn now_us() -> u64 {
    time::now_ns() / 1000
}

impl Panel {
    /// The panel path, if the service holds every page it needs and the
    /// chip is a T210. A path stopped (the display controller or DSI held
    /// in reset or without a clock: a loader that left no picture, a
    /// wake that failed) is brought up from scratch here, before anything
    /// reads the controller; a running one is adopted, its panel id read
    /// while it takes video. `None`: left as the loader set it.
    pub fn open(dc: Mmio, dsi: Mmio) -> Option<Self> {
        let map = |slot, base| Mmio::map(drv::dev_slot(slot), base, 0x1000).ok();
        let misc = map(SLOT_APB_MISC, APB_MISC)?;
        if (misc.r32(GP_HIDREV) >> 4) & 0xF != HIDREV_T210 {
            println!("[display] panel: not a Tegra X1, left to the loader");
            return None;
        }
        let car = map(SLOT_CAR, CAR)?;
        let soc = Soc {
            car,
            gpio: map(SLOT_GPIO, GPIO)?,
            pinmux: map(SLOT_PINMUX, PINMUX)?,
            i2c: I2c::connect().ok()?,
        };
        let mut p = Panel {
            soc,
            dsi: Dsi { dsi, dc },
            mipi: map(SLOT_MIPI_CAL, MIPI_CAL)?,
            dc,
            id: 0,
            cold: false,
        };
        // Clocked, PLLD locked, the controller in continuous display and
        // DSI sending video: what the loader (or a wake that got to its end) leaves.
        let up = car.r32(CAR_CLK_OUT_ENB_L) & CLK_L_DISP1 != 0
            && car.r32(CAR_RST_DEVICES_L) & CLK_L_DISP1 == 0
            && car.r32(CAR_CLK_OUT_ENB_H) & CLK_H_DSI_CAL == CLK_H_DSI_CAL
            && car.r32(CAR_RST_DEVICES_H) & CLK_H_DSI_CAL == 0
            && car.r32(CAR_PLLD_BASE) & PLLD_RUNNING == PLLD_RUNNING
            && dc.r32(DC_CMD_DISPLAY_COMMAND * 4) & DISP_CTRL_MODE_MASK == DISP_CTRL_MODE_C
            && dsi.r32(DSI_CONTROL * 4) & DSI_CONTROL_VIDEO != 0;
        if up {
            let raw = p.dsi.read_running(DCS_GET_DISPLAY_ID, 3)?;
            p.id = decode_id(raw);
            println!(
                "[display] panel id {:#x} ({}), adopted",
                p.id,
                Kind::of(p.id).name()
            );
        } else {
            println!("[display] panel: display path down, bringing it up");
            let ready = p.power_up_1();
            p.power_up_2(ready);
            p.cold = true;
        }
        Some(p)
    }

    /// The path was brought up by `open` (the loader left it down).
    pub fn cold_started(&self) -> bool {
        self.cold
    }

    fn kind(&self) -> Kind {
        Kind::of(self.id)
    }

    /// The panel's id, three tries in command mode. `None`: no answer.
    fn read_id(&self) -> Option<u16> {
        for _ in 0..ID_TRIES {
            if let Some(raw) = self.dsi.read(DCS_GET_DISPLAY_ID, 3) {
                return Some(decode_id(raw));
            }
            time::sleep_us(5000);
        }
        None
    }

    /// Rails, clocks, PLLD in low-power mode, the controller and DSI
    /// initialised, the panel reset, identified, set up and told to leave
    /// sleep. Returns the time (us, `now_us`) it will be ready.
    pub fn power_up_1(&mut self) -> u64 {
        let t0 = now_us();
        if self.soc.dsi_avdd(true).is_err() {
            println!("[display] panel: DSI supply (LDO0) did not answer");
        }
        self.soc.clocks_on();
        self.soc.rails_on();
        self.mipi.w32(MIPI_BIAS_PAD_CFG2 * 4, 0);
        self.mipi.w32(MIPI_BIAS_PAD_CFG0 * 4, 0);
        if !self.soc.plld(3, 20, true) {
            println!("[display] panel: PLLD did not lock");
        }
        for &(r, v) in DC_INIT {
            self.dc.w32(r * 4, v);
        }
        self.dsi.table(DSI_SEQ_RESET0);
        self.dsi.dsi.w32(DSI_INIT_SEQ_DATA_15 * 4, 0);
        self.dsi.table(DSI_SEQ_RESET1);
        self.dsi.table(DSI_INIT);
        time::sleep_us(10);
        self.dsi.soft_reset();
        self.dsi.table(DSI_TIMING_LP);
        time::sleep_ms(10);
        self.soc.reset(true);
        time::sleep_ms(60);
        self.dsi.dsi.w32(DSI_BTA_TIMING * 4, BTA_TIMING_PANEL);
        match self.read_id() {
            Some(id) => {
                if self.id != 0 && id != self.id {
                    println!("[display] panel id changed: {:#x} -> {:#x}", self.id, id);
                }
                self.id = id;
            }
            None => {
                // Spare-part panels answer nothing (hekate runs them on its default).
                println!("[display] panel: no answer to the id read");
                if self.id == 0 {
                    self.id = ID_UNKNOWN;
                }
            }
        }
        if self.kind() == Kind::Jdi {
            self.dsi.table(PANEL_INIT_JDI);
        }
        self.dsi.dcs(DCS_EXIT_SLEEP, 0);
        println!(
            "[display] panel up, phase 1: {} ms (id {:#x})",
            (now_us() - t0) / 1000,
            self.id
        );
        now_us() + self.kind().sleep_out_us()
    }

    /// The panel out of sleep (`ready`, from `power_up_1`): display on,
    /// PLLD and DSI in high-speed mode, the pads calibrated, the video
    /// started, and the controller's window state replayed.
    pub fn power_up_2(&mut self, ready: u64) {
        let t0 = now_us();
        time::sleep_us(ready.saturating_sub(t0));
        if matches!(self.kind(), Kind::Inl | Kind::Auo) {
            self.dsi.dsi.w32(DSI_WR_DATA * 4, EXTC_UNLOCK.0);
            self.dsi.dsi.w32(DSI_WR_DATA * 4, EXTC_UNLOCK.1);
            self.dsi.dsi.w32(DSI_TRIGGER * 4, TRIGGER_HOST);
            time::sleep_us(5000);
            let power = if self.kind() == Kind::Inl {
                POWER_CONTROL_INL
            } else {
                POWER_CONTROL_AUO
            };
            self.dsi.dsi.w32(DSI_WR_DATA * 4, 0x0739);
            self.dsi.dsi.w32(DSI_WR_DATA * 4, power);
            self.dsi.dsi.w32(DSI_WR_DATA * 4, POWER_CONTROL_TAIL);
            self.dsi.dsi.w32(DSI_TRIGGER * 4, TRIGGER_HOST);
            time::sleep_us(5000);
        }
        self.dsi.dcs(DCS_DISPLAY_ON, 20_000);
        if !self.soc.plld(1, 24, false) {
            println!("[display] panel: PLLD (high speed) did not lock");
        }
        self.dsi.table(DSI_VIDEO_NON_BURST);
        self.dc.w32(DC_DISP_DISP_CLOCK_CONTROL * 4, DISP_CLOCK_DIV3);
        self.dsi.table(DSI_HOST_MODE);
        time::sleep_ms(10);
        self.calibrate();
        time::sleep_ms(10);
        for &(r, v) in DC_VIDEO_MODE {
            self.dc.w32(r * 4, v);
        }
        self.soc.backlight_pad();
        dcgate::resume(&self.dc);
        println!("[display] panel up, phase 2: {} ms", (now_us() - t0) / 1000);
    }

    /// MIPI pad calibration, twice (hekate: the T210B01 needs it, and the
    /// T210 does no harm).
    fn calibrate(&self) {
        let m = |t: Table| t.iter().for_each(|&(r, v)| self.mipi.w32(r * 4, v));
        m(MIPI_PAD_CAL);
        for _ in 0..2 {
            self.mipi.w32(MIPI_BIAS_PAD_CFG2 * 4, BIAS_CFG2_CAL);
            self.mipi.w32(MIPI_BIAS_PAD_CFG1 * 4, BIAS_CFG1_T210);
            self.dsi.table(DSI_PAD_CAL);
            m(MIPI_DSI_CAL);
            m(MIPI_CAL_UNUSED);
            self.mipi.w32(MIPI_CAL_CTRL * 4, MIPI_CAL_START);
        }
    }

    /// The panel and everything under it off, the backlight already dark
    /// and the windows already stopped: display off, the controller
    /// stopped, PLLD back in low-power mode, the panel's own power-down
    /// sequence, sleep in, reset low, AVDD off, clocks and PLLD stopped,
    /// DSI's supply off.
    pub fn power_down(&mut self) {
        let t0 = now_us();
        // The controller is ours from here: window writes only reach the shadow.
        dcgate::suspend();
        self.dsi.dcs_running(DCS_DISPLAY_OFF);
        self.dc.w32(DC_CMD_STATE_ACCESS * 4, 0x5);
        for &(r, v) in DC_VIDEO_DISABLE {
            self.dc.w32(r * 4, v);
        }
        if !self.soc.plld(3, 20, true) {
            println!("[display] panel: PLLD (low power) did not lock");
        }
        self.dsi.soft_reset();
        self.dsi.table(DSI_TIMING_LP);
        time::sleep_ms(10);
        match self.kind() {
            Kind::Jdi => self.dsi.table(PANEL_DEINIT_JDI),
            Kind::Auo => {
                self.dsi.table(PANEL_DEINIT_AUO);
                time::sleep_us(5000);
            }
            Kind::Inl | Kind::Other => {}
        }
        self.dsi.dcs(DCS_ENTER_SLEEP, 50_000);
        self.soc.reset(false);
        time::sleep_ms(10);
        self.soc.rails_off();
        time::sleep_ms(10);
        // Pads down while the block still has its clock.
        self.dsi.dsi.w32(DSI_PAD_CONTROL_0 * 4, PAD_POWERED_DOWN);
        self.dsi.soft_reset();
        dcgate::suspend_with(|| self.soc.clocks_off());
        let _ = self.soc.dsi_avdd(false);
        self.soc.plld_off();
        println!("[display] panel down in {} ms", (now_us() - t0) / 1000);
    }
}

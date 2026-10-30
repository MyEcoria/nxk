//! Game card reader: the host side of the Lotus3 ASIC, an SDHCI
//! (SDMMC2) with an 8-bit 1.8 V bus at HS100 timing, tuned by CMD21.
//! Sequence and values follow hekate (`sdmmc_storage_init_gc`,
//! `sdmmc_init`, `sdmmc_tuning_execute`); Atmosphere's gc ASIC accessor
//! gives the same speed mode and the CMD60 to CMD63 vendor commands.
//!
//! Only the commands that cannot change the ASIC are here: tuning and
//! CMD13. CMD60's operation 0x01 loads firmware, which burns OTP fuses.

use crate::*;

/// SDMMC2 inside SDMMC1's register page.
pub const SDMMC2_OFFSET: usize = 0x200;
pub const SDMMC2: u64 = SDMMC1 + SDMMC2_OFFSET as u64;

const VENDOR_TUNING_CNTRL1: usize = 0x1C4;
/// APB_MISC pad control of SDMMC2 (hekate `APB_MISC_GP_EMMC2_PAD_CFGPADCTRL`).
const GP_EMMC2_PAD_CFGPADCTRL: usize = 0xA9C;
/// SDMMC2's pinmux pads (Mariko only): DAT0-7, CLK and CMD.
const PINMUX_SDMMC2: [usize; 10] = [
    0x294, 0x298, 0x29C, 0x2A0, 0x2A4, 0x2A8, 0x2AC, 0x2B0, 0x2B4, 0x2BC,
];
const PINMUX_SCHMT: u32 = 1 << 12;

const HOST_BUS_4BIT: u8 = 1 << 1;
const HOST_BUS_8BIT: u8 = 1 << 5;
const CLOCK_INT_EN: u16 = 1 << 0;
const CLOCK_INT_STABLE: u16 = 1 << 1;
const PROG_CLOCK_MODE: u16 = 1 << 5;
const CLOCK_DIV_MASK: u16 = 0xFFC0;
const CTRL_UHS_MASK: u16 = 7;
const CTRL_UHS_SDR104: u16 = 3;
const CTRL_EXEC_TUNING: u16 = 1 << 6;
const CTRL_TUNED_CLK: u16 = 1 << 7;
const INT_DATA_AVAIL: u32 = 1 << 5;
const SWRST_ALL: u8 = 1;
const SWRST_CMD_DATA: u8 = 6;
/// Bus power on at 1.8 V.
const POWER_1V8: u8 = 0x0B;
/// 256 hardware tuning iterations, 1x multiplier (hekate, HS100).
const TUNING_256: u32 = 4 << 13;
const TUNING_ITERATIONS: u32 = 256;
/// Tuning block of an 8-bit bus.
const TUNING_BLOCK: u16 = 128;
const CMD_SEND_STATUS: u8 = 13;
const CMD_SEND_TUNING_BLOCK: u8 = 21;
/// Response R1 with index and CRC checks, and the same with data.
const RSP_R1: u16 = 0x1A;
const DATA_PRESENT: u16 = 1 << 5;
/// 199.68 MHz source divided by 2: 99.84 MHz.
const HS100_DIV: u16 = 1 << 8;
const TRANSFER_READ: u16 = 1 << 4;
const PRESENT_CMD_INHIBIT: u32 = 1 << 0;
const PRESENT_DAT_INHIBIT: u32 = 1 << 1;

/// Trim, tap and the pad offsets of SDMMC2 (hekate: `trim_values`,
/// `_sdmmc_config_tap_val`, `_sdmmc_autocal_config_offset`).
struct Trim {
    trim: u32,
    tap: u32,
}

fn trim(mariko: bool) -> Trim {
    if mariko {
        Trim { trim: 13, tap: 11 }
    } else {
        Trim { trim: 8, tap: 0 }
    }
}

/// The ASIC's controller, clocked and out of reset (`clk::SDMMC2`).
pub struct Gc<R: Regs, H: Host> {
    pub regs: R,
    pub host: H,
    mariko: bool,
}

impl<R: Regs, H: Host> Gc<R, H> {
    /// `regs` is SDMMC2's block (offsets from 0), `mariko`: T210B01.
    pub fn new(regs: R, host: H, mariko: bool) -> Self {
        Gc { regs, host, mariko }
    }

    fn commit(&self) {
        let _ = self.regs.r16(CLOCK_CONTROL);
    }

    fn reset(&self, what: u8) {
        self.regs.w8(SOFTWARE_RESET, what);
        let end = self.host.now_us() + 100_000;
        while self.regs.r8(SOFTWARE_RESET) & what != 0 && self.host.now_us() < end {}
    }

    fn card_clock(&self, on: bool) {
        let v = self.regs.r16(CLOCK_CONTROL);
        self.regs.w16(
            CLOCK_CONTROL,
            if on {
                v | CLOCK_CARD_EN
            } else {
                v & !CLOCK_CARD_EN
            },
        );
    }

    /// 8 card clocks at 99.84 MHz are well under 1 us.
    fn settle(&self) {
        self.commit();
        self.host.sleep_us(1);
    }

    /// Pad calibration as hekate `_sdmmc_autocal_execute` (SDMMC2 is an
    /// eMMC-style pad: E_PWRD off while it runs). 5/5 offsets at 1.8 V.
    /// A timeout loads hekate's standard drive strengths.
    fn calibrate<B: Regs>(&self, misc: &B) {
        let clock = self.regs.r16(CLOCK_CONTROL);
        self.regs.w16(CLOCK_CONTROL, clock & !CLOCK_CARD_EN);
        self.regs.modify(AUTO_CAL_CONFIG, 0x7F7F, 5 << 8 | 5);
        self.regs.modify(SDMEMCOMPPADCTRL, 0, PADCTRL_E_INPUT_PWRD);
        self.settle();
        self.regs
            .modify(AUTO_CAL_CONFIG, 0, AUTOCAL_ENABLE | AUTOCAL_START);
        self.commit();
        self.host.sleep_us(2);
        let idle = || self.regs.r32(AUTO_CAL_STATUS) & AUTOCAL_ACTIVE == 0;
        self.host.wait(10_000, &idle, false);
        if !idle() {
            self.regs.modify(AUTO_CAL_CONFIG, AUTOCAL_ENABLE, 0);
            let v = misc.r32(GP_EMMC2_PAD_CFGPADCTRL);
            misc.w32(
                GP_EMMC2_PAD_CFGPADCTRL,
                if self.mariko {
                    (v & 0xF808_0FFF) | 0x00A0_A000
                } else {
                    (v & 0xFFFF_C003) | 0x1040
                },
            );
            let _ = misc.r32(GP_EMMC2_PAD_CFGPADCTRL);
            self.host.log(format_args!(
                "[gc] pad calibration timed out: standard drive\n"
            ));
        }
        self.regs.modify(SDMEMCOMPPADCTRL, PADCTRL_E_INPUT_PWRD, 0);
        self.regs.w16(CLOCK_CONTROL, clock);
    }

    /// HS100 timing: tap, SDR104 host mode at 1.8 V, divider 2.
    fn timing_hs100(&self) {
        let was = self.regs.r16(CLOCK_CONTROL) & CLOCK_CARD_EN != 0;
        self.card_clock(false);
        self.regs
            .modify(VENDOR_TUNING_CNTRL0, TUNING_TAP_HW_UPDATED, 0);
        self.regs
            .modify(VENDOR_CLOCK_CNTRL, 0xFF_0000, trim(self.mariko).tap << 16);
        self.reset(SWRST_CMD_DATA);
        let h2 = self.regs.r16(HOST_CONTROL2) & !CTRL_UHS_MASK;
        self.regs.w16(HOST_CONTROL2, h2 | CTRL_UHS_SDR104 | VDD_180);
        let c = self.regs.r16(CLOCK_CONTROL) & !CLOCK_DIV_MASK;
        self.regs.w16(CLOCK_CONTROL, c | HS100_DIV);
        if was {
            self.card_clock(true);
        }
    }

    /// Pads out of park (Erista) or with their Schmitt triggers on
    /// (Mariko), as hekate `_sdmmc_config_emmc`, then the controller:
    /// reset, trims, calibration, internal clock, 8-bit bus, 1.8 V, HS100
    /// and the card clock running.
    pub fn start<B: Regs>(&self, misc: &B, pinmux: &B) -> Result<()> {
        if self.mariko {
            for off in PINMUX_SDMMC2 {
                pinmux.modify(off, 0, PINMUX_SCHMT);
            }
        } else {
            misc.modify(GP_EMMC2_PAD_CFGPADCTRL, !0xF800_3FFF, 0);
            let _ = misc.r32(GP_EMMC2_PAD_CFGPADCTRL);
        }
        self.reset(SWRST_ALL);

        self.regs.modify(IO_SPARE, 0, 1 << 19);
        self.regs.modify(VENDOR_IO_TRIM_CNTRL, 1 << 2, 0);
        self.regs.modify(
            VENDOR_CLOCK_CNTRL,
            0x1F00_0004,
            trim(self.mariko).trim << 24,
        );
        self.regs.modify(SDMEMCOMPPADCTRL, 0xF, 7);
        self.commit();
        self.calibrate(misc);

        self.regs.modify16(CLOCK_CONTROL, 0, CLOCK_INT_EN);
        self.commit();
        let stable = || self.regs.r16(CLOCK_CONTROL) & CLOCK_INT_STABLE != 0;
        self.host.wait(2_000_000, &stable, false);
        if !stable() {
            return Err(-ETIMEDOUT);
        }
        self.regs.modify16(HOST_CONTROL2, PRESET_VAL_EN, 0);
        self.regs.modify16(CLOCK_CONTROL, PROG_CLOCK_MODE, 0);
        self.regs.w8(TIMEOUT_CONTROL, 0x0E);

        let h = self.regs.r8(HOST_CONTROL) & !(HOST_BUS_4BIT | HOST_BUS_8BIT);
        self.regs.w8(HOST_CONTROL, h | HOST_BUS_8BIT);
        self.regs.w8(POWER_CONTROL, POWER_1V8);
        self.timing_hs100();
        // Status bits visible, no interrupt: the commands are polled.
        self.regs.w16(INT_ENABLE, 0xFFFF);
        self.regs.w16(INT_ENABLE + 2, 0xFFFF);
        self.regs.w16(INT_SIGNAL, 0);
        self.regs.w16(INT_SIGNAL + 2, 0);
        self.card_clock(true);
        self.commit();
        Ok(())
    }

    fn wait_inhibit(&self, data: bool) -> Result<()> {
        let mask = PRESENT_CMD_INHIBIT | if data { PRESENT_DAT_INHIBIT } else { 0 };
        let end = self.host.now_us() + 2_000_000;
        while self.regs.r32(PRESENT_STATE) & mask != 0 {
            if self.host.now_us() > end {
                self.reset(SWRST_CMD_DATA);
                return Err(-ETIMEDOUT);
            }
        }
        Ok(())
    }

    /// One tuning block read (hekate `_sdmmc_tuning_execute_once`): the
    /// card clock is stopped while the command is posted.
    fn tuning_once(&self) -> Result<()> {
        self.wait_inhibit(true)?;
        self.regs.w16(BLOCK_SIZE, TUNING_BLOCK);
        self.regs.w16(BLOCK_COUNT, 1);
        self.regs.w16(TRANSFER_MODE, TRANSFER_READ);
        self.regs.w32(INT_STATUS, 0xFFFF_FFFF);
        self.card_clock(false);
        self.regs.w32(ARGUMENT, 0);
        self.regs.w16(
            COMMAND,
            (CMD_SEND_TUNING_BLOCK as u16) << 8 | RSP_R1 | DATA_PRESENT,
        );
        self.settle();
        self.reset(SWRST_CMD_DATA);
        self.card_clock(true);
        self.commit();
        let got = || self.regs.r32(INT_STATUS) & INT_DATA_AVAIL != 0;
        self.host.wait(5_000, &got, false);
        let ok = got();
        if ok {
            self.regs.w32(INT_STATUS, INT_DATA_AVAIL);
        } else {
            self.reset(SWRST_CMD_DATA);
        }
        self.settle();
        if ok {
            Ok(())
        } else {
            Err(-ETIMEDOUT)
        }
    }

    /// Hardware tuning on CMD21 (hekate `sdmmc_tuning_execute`, HS100):
    /// the controller walks its taps and reports one that read the tuning
    /// pattern. Only a live ASIC answers it.
    pub fn tune(&self) -> Result<()> {
        self.regs.w32(VENDOR_TUNING_CNTRL1, 0);
        self.regs.modify(VENDOR_TUNING_CNTRL0, 0xE000, TUNING_256);
        self.regs.modify(VENDOR_TUNING_CNTRL0, 0x1FC0, 1 << 6);
        self.regs
            .modify(VENDOR_TUNING_CNTRL0, 0, TUNING_TAP_HW_UPDATED);
        self.regs.modify16(HOST_CONTROL2, 0, CTRL_EXEC_TUNING);
        for _ in 0..TUNING_ITERATIONS {
            let _ = self.tuning_once();
            if self.regs.r16(HOST_CONTROL2) & CTRL_EXEC_TUNING == 0 {
                break;
            }
        }
        // A tuning left running would hold the command engine.
        let h2 = self.regs.r16(HOST_CONTROL2);
        if h2 & CTRL_EXEC_TUNING != 0 {
            self.regs.w16(HOST_CONTROL2, h2 & !CTRL_EXEC_TUNING);
            self.reset(SWRST_CMD_DATA);
        }
        if h2 & CTRL_TUNED_CLK != 0 && h2 & CTRL_EXEC_TUNING == 0 {
            Ok(())
        } else {
            Err(-EIO)
        }
    }

    /// CMD13 with RCA 0: the ASIC's R1 status word, if it answers at all.
    pub fn status(&self) -> Result<u32> {
        self.wait_inhibit(false)?;
        self.regs.w32(INT_STATUS, 0xFFFF_FFFF);
        self.card_clock(true);
        self.settle();
        self.regs.w32(ARGUMENT, 0);
        self.regs
            .w16(COMMAND, (CMD_SEND_STATUS as u16) << 8 | RSP_R1);
        let hit = || self.regs.r32(INT_STATUS) & (INT_CMD_DONE | INT_ERROR) != 0;
        self.host.wait(100_000, &hit, false);
        let st = self.regs.r32(INT_STATUS);
        let r = if st & INT_ERROR != 0 {
            Err(-EIO)
        } else if st & INT_CMD_DONE != 0 {
            Ok(self.regs.r32(RESPONSE))
        } else {
            Err(-ETIMEDOUT)
        };
        self.regs.w32(INT_STATUS, st);
        if r.is_err() {
            self.reset(SWRST_CMD_DATA);
        }
        self.settle();
        r
    }

    /// Card clock and bus power off, before the ASIC loses its rail.
    pub fn stop(&self) {
        self.regs.w16(CLOCK_CONTROL, 0);
        self.regs.w8(POWER_CONTROL, 0);
        self.commit();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::RefCell;
    use core::fmt;
    use std::vec::Vec;

    /// Register file that reads back what was written, with the
    /// self-clearing and status bits a real controller would raise.
    struct Fake {
        mem: RefCell<[u8; 0x1000]>,
        tuned: bool,
    }

    impl Fake {
        fn new(tuned: bool) -> Self {
            Fake {
                mem: RefCell::new([0; 0x1000]),
                tuned,
            }
        }
        fn get(&self, off: usize, n: usize) -> u32 {
            let m = self.mem.borrow();
            (0..n).fold(0, |a, i| a | (m[off + i] as u32) << (8 * i))
        }
        fn put(&self, off: usize, n: usize, v: u32) {
            let mut m = self.mem.borrow_mut();
            for i in 0..n {
                m[off + i] = (v >> (8 * i)) as u8;
            }
        }
    }

    impl Regs for &Fake {
        fn r8(&self, off: usize) -> u8 {
            self.get(off, 1) as u8
        }
        fn w8(&self, off: usize, v: u8) {
            // Resets complete at once.
            if off == SOFTWARE_RESET {
                self.put(off, 1, 0);
            } else {
                self.put(off, 1, v as u32);
            }
        }
        fn r16(&self, off: usize) -> u16 {
            let v = self.get(off, 2) as u16;
            // Internal clock stable as soon as it is enabled.
            if off == CLOCK_CONTROL && v & CLOCK_INT_EN != 0 {
                v | CLOCK_INT_STABLE
            } else {
                v
            }
        }
        fn w16(&self, off: usize, v: u16) {
            if off == HOST_CONTROL2 && v & CTRL_EXEC_TUNING != 0 && self.tuned {
                // Tuning done at once, a tap found.
                self.put(off, 2, (v & !CTRL_EXEC_TUNING | CTRL_TUNED_CLK) as u32);
            } else if off == COMMAND {
                self.put(INT_STATUS, 4, INT_DATA_AVAIL | INT_CMD_DONE);
                self.put(RESPONSE, 4, 0x900);
            } else {
                self.put(off, 2, v as u32);
            }
        }
        fn r32(&self, off: usize) -> u32 {
            self.get(off, 4)
        }
        fn w32(&self, off: usize, v: u32) {
            if off == INT_STATUS {
                let cur = self.get(off, 4);
                self.put(off, 4, cur & !v);
            } else {
                self.put(off, 4, v);
            }
        }
    }

    struct Clock {
        now: RefCell<u64>,
        log: RefCell<Vec<std::string::String>>,
    }

    impl Host for &Clock {
        fn now_us(&self) -> u64 {
            *self.now.borrow_mut() += 1;
            *self.now.borrow()
        }
        fn sleep_us(&self, us: u64) {
            *self.now.borrow_mut() += us;
        }
        fn log(&self, args: fmt::Arguments) {
            self.log.borrow_mut().push(std::format!("{}", args));
        }
    }

    fn rig(tuned: bool) -> (Fake, Fake, Fake, Clock) {
        (
            Fake::new(tuned),
            Fake::new(true),
            Fake::new(true),
            Clock {
                now: RefCell::new(0),
                log: RefCell::new(Vec::new()),
            },
        )
    }

    #[test]
    fn start_programs_an_8_bit_1v8_hs100_host() {
        let (sd, misc, pins, clock) = rig(true);
        let gc = Gc::new(&sd, &clock, false);
        gc.start(&&misc, &&pins).unwrap();
        assert_eq!(sd.get(HOST_CONTROL, 1) as u8 & HOST_BUS_8BIT, HOST_BUS_8BIT);
        assert_eq!(sd.get(POWER_CONTROL, 1), POWER_1V8 as u32);
        let h2 = sd.get(HOST_CONTROL2, 2) as u16;
        assert_eq!(h2 & CTRL_UHS_MASK, CTRL_UHS_SDR104);
        assert_ne!(h2 & VDD_180, 0);
        let clk = sd.get(CLOCK_CONTROL, 2) as u16;
        assert_eq!(clk & CLOCK_DIV_MASK, HS100_DIV);
        assert_ne!(clk & CLOCK_CARD_EN, 0);
        // Erista trim 8, tap 0.
        assert_eq!(sd.get(VENDOR_CLOCK_CNTRL, 4) >> 24 & 0x1F, 8);
        assert_eq!(sd.get(VENDOR_CLOCK_CNTRL, 4) >> 16 & 0xFF, 0);
    }

    #[test]
    fn mariko_uses_its_trim_and_tap() {
        let (sd, misc, pins, clock) = rig(true);
        let gc = Gc::new(&sd, &clock, true);
        gc.start(&&misc, &&pins).unwrap();
        assert_eq!(sd.get(VENDOR_CLOCK_CNTRL, 4) >> 24 & 0x1F, 13);
        assert_eq!(sd.get(VENDOR_CLOCK_CNTRL, 4) >> 16 & 0xFF, 11);
        assert_ne!(pins.get(0x294, 4) & PINMUX_SCHMT, 0);
    }

    #[test]
    fn tuning_reports_a_found_tap() {
        let (sd, misc, pins, clock) = rig(true);
        let gc = Gc::new(&sd, &clock, false);
        gc.start(&&misc, &&pins).unwrap();
        assert_eq!(gc.tune(), Ok(()));
        // 256 tries and the 1x multiplier were programmed.
        let t = sd.get(VENDOR_TUNING_CNTRL0, 4);
        assert_eq!(t & 0xE000, TUNING_256);
    }

    #[test]
    fn no_asic_means_no_tuned_clock() {
        let (sd, misc, pins, clock) = rig(false);
        let gc = Gc::new(&sd, &clock, false);
        gc.start(&&misc, &&pins).unwrap();
        // The fake never raises the tuned bit; the tries run out.
        assert_eq!(gc.tune(), Err(-EIO));
    }

    #[test]
    fn status_returns_the_r1_word() {
        let (sd, misc, pins, clock) = rig(true);
        let gc = Gc::new(&sd, &clock, false);
        gc.start(&&misc, &&pins).unwrap();
        assert_eq!(gc.status(), Ok(0x900));
    }
}

//! clk: device clocks/resets (CAR); values from hekate (bdk/soc/clock.c).
//! Also reports PMC power-partition status (STATUS), and brings up the
//! USB2 PLLs and wake detectors (PMC USB_AO) for the OTG device
//! controller (hekate bdk/usb/usbd.c), and unclamps the GPU (PMC
//! GPU_RG_CNTRL) once its rail is on.

#![no_std]
#![no_main]

use libnx::abi::err::{EINVAL, EIO, ENODEV};
use libnx::abi::proto::clk as proto;
use libnx::drv::{self, Mmio};
use libnx::ipc::Msg;
use libnx::{println, time};

const CAR: u64 = 0x6000_6000;
const AHUB_PAGE: u64 = 0x702D_0000;
/// CLR register of a SET/CLR pair.
const CLR: usize = 4;

/// PMC page (registers from 0x400, hekate bdk/soc/pmc_t210.h).
const PMC_PAGE: u64 = 0x7000_E000;
const PMC_CLAMP_STATUS: usize = 0x400 + 0x2C;
const PMC_PWRGATE_TOGGLE: usize = 0x400 + 0x30;
const PMC_REMOVE_CLAMPING: usize = 0x400 + 0x34;
const PMC_PWRGATE_STATUS: usize = 0x400 + 0x38;
const PWRGATE_TOGGLE_START: u32 = 1 << 8;
/// UTMIP ID and VBUS wake detectors: power-down bits 2, 3.
const PMC_USB_AO: usize = 0x400 + 0xF0;
/// XUSB partitions (hekate bdk/soc/pmc.h, POWER_RAIL_XUSBx): A = SuperSpeed,
/// C = host.
const PART_XUSBA: u32 = 1 << 20;
const PART_XUSBC: u32 = 1 << 22;
/// PCIe partition (POWER_RAIL_PCIE = 3): off at boot, unlike the XUSB
/// ones the loader leaves on.
const PART_PCIE_ID: u32 = 3;
const PART_PCIE: u32 = 1 << PART_PCIE_ID;
const PART_AUD_ID: u32 = 27;
const PART_AUD: u32 = 1 << PART_AUD_ID;
/// I/O pad deep power-down (Atmosphère tegra_pmc.hpp: code in bits 31:30).
/// PEX bias and reference-clock pads: bits 4-6 (Linux `tegra210_io_pads`).
const PMC_IO_DPD_REQ: usize = 0x400 + 0x1B8;
const PMC_IO_DPD_STATUS: usize = 0x400 + 0x1BC;
const PMC_IO_DPD2_REQ: usize = 0x400 + 0x1C0;
const PMC_IO_DPD2_STATUS: usize = 0x400 + 0x1C4;
const PMC_PWR_DET: usize = 0x400 + 0x48;
const PMC_PWR_DET_VAL: usize = 0x400 + 0xE4;
/// Audio pads: deep power-down bits (IO_DPD, IO_DPD2) and the 3.3 V
/// enable of the audio-hv pad in PWR_DET_VAL.
const AUDIO_PAD: u32 = 1 << 17;
const AUDIO_HV_PAD: u32 = 1 << 29;
const AUDIO_HV_3V3: u32 = 1 << 18;
const IO_DPD_OFF: u32 = 1 << 30;
const PEX_PADS: u32 = 0x7 << 4;

/// CAR bank (L, H, U, V, W, X): state in RST_DEVICES_x and CLK_OUT_ENB_x,
/// driven via RST_DEV_x_SET and CLK_ENB_x_SET (+ CLR).
struct Bank {
    rst: usize,
    enb: usize,
    rst_set: usize,
    enb_set: usize,
}

const L: Bank = Bank {
    rst: 0x004,
    enb: 0x010,
    rst_set: 0x300,
    enb_set: 0x320,
};
const H: Bank = Bank {
    rst: 0x008,
    enb: 0x014,
    rst_set: 0x308,
    enb_set: 0x328,
};
const U: Bank = Bank {
    rst: 0x00C,
    enb: 0x018,
    rst_set: 0x310,
    enb_set: 0x330,
};
const V: Bank = Bank {
    rst: 0x358,
    enb: 0x360,
    rst_set: 0x430,
    enb_set: 0x440,
};
const W: Bank = Bank {
    rst: 0x35C,
    enb: 0x364,
    rst_set: 0x438,
    enb_set: 0x448,
};
const X: Bank = Bank {
    rst: 0x28C,
    enb: 0x280,
    rst_set: 0x290,
    enb_set: 0x284,
};
const Y: Bank = Bank {
    rst: 0x2A4,
    enb: 0x298,
    rst_set: 0x2A8,
    enb_set: 0x29C,
};

/// USB2 PLLs (38.4 MHz oscillator): PLLU 480 MHz, UTMIPLL 960 MHz.
const PLLU_BASE: usize = 0xC0;
const PLLU_MISC: usize = 0xCC;
const PLL_ENABLE: u32 = 1 << 30;
const PLL_LOCK: u32 = 1 << 27;
/// PLLU USB, HSIC, ICUSB and 48M outputs.
const PLLU_OUTPUTS: u32 = 0x2E0_0000;
const UTMIP_PLL_CFG0: usize = 0x480;
const UTMIP_PLL_CFG1: usize = 0x484;
const UTMIP_PLL_CFG2: usize = 0x488;
const UTMIP_XTAL_CLOCKEN: u32 = 1 << 30;
const UTMIPLL_HW_PWRDN_CFG0: usize = 0x52C;
const UTMIPLL_LOCK: u32 = 1 << 31;
/// XUSB_PADCTL reset (bank W bit 14): the pads are shared with XUSB.
const RST_DEV_W_CLR: usize = 0x43C;
const PADCTL_RESET: u32 = 1 << 14;
/// PEX UPHY reset (bank Y bit 13, reset only): the PCIe lanes sit on
/// the UPHY bricks the pad control block owns.
const UPHY_PEX_RESET: u32 = 1 << 13;
/// PCIe reference PLL (Linux drivers/clk/tegra/clk-pll.c,
/// `clk_plle_tegra210_enable`): 38.4 MHz oscillator, M 2, N 125,
/// CML divider code 14 -> 100 MHz for the UPHY bricks.
const PLLE_BASE: usize = 0xE8;
const PLLE_MISC: usize = 0xEC;
const PLLE_SS_CNTL: usize = 0x68;
const PLLE_AUX: usize = 0x48C;
const PLLE_DIVM: u32 = 2;
const PLLE_DIVN: u32 = 125;
const PLLE_DIVCML: u32 = 14;
const PLLE_ENABLE: u32 = 1 << 31;
const PLLE_LOCK_OVERRIDE: u32 = 1 << 30;
const PLLE_MISC_LOCK: u32 = 1 << 11;
const PLLE_MISC_LOCK_ENABLE: u32 = 1 << 9;
const PLLE_MISC_IDDQ_SW_CTRL: u32 = 1 << 14;
const PLLE_MISC_IDDQ_SW_VALUE: u32 = 1 << 13;
const PLLE_MISC_PTS: u32 = 1 << 8;
/// Bandgap and regulator control fields, driven to their maximum as
/// the reference sequence does.
const PLLE_MISC_VREG: u32 = (3 << 4) | (3 << 2);
const PLLE_AUX_SEQ_ENABLE: u32 = 1 << 24;
const PLLE_AUX_ENABLE_SWCTL: u32 = 1 << 4;
const PLLE_AUX_USE_LOCKDET: u32 = 1 << 3;
const PLLE_AUX_SS_SWCTL: u32 = 1 << 6;
/// Spread spectrum: interpolator reset, bypass, coefficients.
const PLLE_SS_INTERP_RESET: u32 = 1 << 11;
const PLLE_SS_BYPASS: u32 = 1 << 10;
const PLLE_SS_SSC_BYP: u32 = 1 << 12;
const PLLE_SS_CENTER: u32 = 1 << 14;
const PLLE_SS_INVERT: u32 = 1 << 15;
const PLLE_SS_COEFF_MASK: u32 = 0x1FF | (0xFF << 16) | (0x3F << 24);
const PLLE_SS_COEFF: u32 = 0x25 | (1 << 16) | (0x14 << 24);
/// Fuse registers visible (CFG_ALL_VISIBLE), set by hekate at boot.
const MISC_CLK_ENB: usize = 0x48;
const FUSE_VISIBLE: u32 = 1 << 28;
/// PLLP_OUT5, upper half of PLLP_OUTC: divider 408 MHz * 2 / (2 + 2) =
/// 204 MHz with override, enable and reset-release bits (Linux
/// clk-tegra-periph.c `PLL_OUT(5, PLLP_OUTC, 24, 0, 16)`).
const PLLP_OUTC: usize = 0x67C;
const PLLP_OUT5: u32 = (2 << 24) | (1 << 18) | (1 << 17) | (1 << 16);
/// PMC GPU_RG_CNTRL: bit 0 clamps the GPU rail's outputs.
const PMC_GPU_RG_CNTRL: usize = 0x400 + 0x2D4;

/// One clock: bank, bit, source register (0: none) with bits to keep
/// and the value to set; `reset`: it also has a reset line.
struct Clock {
    bank: &'static Bank,
    bit: u32,
    source: usize,
    keep: u32,
    src: u32,
    reset: bool,
}

/// Extra condition for `ST_POWERED` (and setup for ENABLE).
#[derive(PartialEq)]
enum Needs {
    Nothing,
    /// PLLU running and locked, UTMIPLL programmed.
    UsbPlls,
    /// Fuse registers visible.
    FuseVisible,
    /// PCIe partition ungated and PLLE locked: the reference clock of
    /// the UPHY lanes the Wi-Fi chip sits on.
    Pcie,
    /// In use by an earlier stage (PWM0 dims the Erista backlight): a
    /// running block is never reset nor stopped.
    Shared,
    /// GPU rail unclamped (its own sequence, `gpu_up`).
    Gpu,
    /// AUD partition on and PLLA locked (`audio_up`).
    Audio,
}

/// One device: its clocks in enable order plus PMC partitions that must
/// be powered (0: always-on domain).
struct Device {
    clocks: &'static [Clock],
    parts: u32,
    needs: Needs,
}

/// Source 6 (19.2 MHz oscillator): divider 4 -> 4.8 MHz (100 kHz),
/// 1 -> 19.2 MHz (400 kHz).
const fn i2c(bank: &'static Bank, bit: u32, source: usize, div: u32) -> Clock {
    Clock {
        bank,
        bit,
        source,
        keep: 0,
        src: (6 << 29) | (div - 1),
        reset: true,
    }
}

const I2C1: [Clock; 1] = [i2c(&L, 12, 0x124, 4)];
const I2C2: [Clock; 1] = [i2c(&H, 22, 0x198, 1)];
const I2C3: [Clock; 1] = [i2c(&U, 3, 0x1B8, 1)];
const I2C4: [Clock; 1] = [i2c(&V, 7, 0x3C4, 4)];
const I2C5: [Clock; 1] = [i2c(&H, 15, 0x128, 1)];
const I2C6: [Clock; 1] = [i2c(&X, 6, 0x65C, 4)];
/// SDMMC: PLLP 408 MHz / 8.5 = 48 MHz, the controller divides further itself.
const SDMMC1: [Clock; 1] = [Clock {
    bank: &L,
    bit: 14,
    source: 0x150,
    keep: 0,
    src: 15,
    reset: true,
}];
const SDMMC4: [Clock; 1] = [Clock {
    bank: &L,
    bit: 15,
    source: 0x164,
    keep: 0,
    src: 15,
    reset: true,
}];
/// xHCI host gates (hekate device-mode order); SuperSpeed source back on
/// always-on CLK_M, host source untouched.
const XUSB_HOST: [Clock; 3] = [
    Clock {
        bank: &W,
        bit: 15,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &W,
        bit: 28,
        source: 0x610,
        keep: 0x1FFF_FF00,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &U,
        bit: 25,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
];

/// USB2 OTG controller (USBD, bank L bit 22).
const USBD: [Clock; 1] = [Clock {
    bank: &L,
    bit: 22,
    source: 0,
    keep: 0,
    src: 0,
    reset: true,
}];
/// USB2 pad tracking: gate only, divider 6 (hekate).
const USB2_TRK: [Clock; 1] = [Clock {
    bank: &Y,
    bit: 18,
    source: 0x6CC,
    keep: 0xFFFF_FF00,
    src: 6,
    reset: false,
}];
/// PCIe root complex: AFI bridge then the controller itself
/// (hekate bdk/soc/clock.h, CLK_U_AFI 8 / CLK_U_PCIE 6). Sources are
/// left alone: both run off PLLP and PLLE as the reset values say.
const PCIE: [Clock; 2] = [
    Clock {
        bank: &U,
        bit: 8,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &U,
        bit: 6,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
];
/// PCIEXCLK (bank U bit 10): a reset line without a gate of its own.
const PCIEXCLK_RESET: u32 = 1 << 10;
/// hekate `_clock_pwm`: CLK_M (19.2 MHz) / 3 = 6.4 MHz, 25 kHz PWM.
const PWM: [Clock; 1] = [Clock {
    bank: &L,
    bit: 17,
    source: 0x110,
    keep: 0,
    src: (6 << 29) | 4,
    reset: true,
}];

/// GM20B (Linux tegra210.dtsi gpu@57000000): the GPU gate with its
/// reset (X 24), then PLLG_REF (X 29), the GPCPLL's reference.
const GPU: [Clock; 2] = [
    Clock {
        bank: &X,
        bit: 24,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &X,
        bit: 29,
        source: 0,
        keep: 0,
        src: 0,
        reset: false,
    },
];

/// Audio hub (Linux clk-tegra210.c, clk-tegra-periph.c; tegra210-odin DT):
/// D_AUDIO (V 10, AHUB), APBIF (V 11), APE (Y 6) and I2S1 (L 11) from
/// PLLA_OUT0 (source 0), then EXTERN1 (V 24, no reset), the codec's MCLK.
/// PLLA_OUT0 is 36.864 MHz: D_AUDIO and APE run it undivided, I2S1 at /24
/// (1.536 MHz, the 48 kHz 16-bit stereo bit clock), EXTERN1 at /3
/// (12.288 MHz = 256 fs). Divider field: 2 * ratio - 2.
const AUDIO: [Clock; 5] = [
    Clock {
        bank: &V,
        bit: 10,
        source: 0x3D0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &V,
        bit: 11,
        source: 0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &Y,
        bit: 6,
        source: 0x6C0,
        keep: 0,
        src: 0,
        reset: true,
    },
    Clock {
        bank: &L,
        bit: 11,
        source: 0x100,
        keep: 0,
        src: 2 * 24 - 2,
        reset: true,
    },
    Clock {
        bank: &V,
        bit: 24,
        source: 0x3EC,
        keep: 0,
        src: 2 * 3 - 2,
        reset: false,
    },
];

/// UART B (bank L bit 7) and C (bank H bit 23): PLLP_OUT0 (408 MHz)
/// through the 16x clock divider at 1 Mbaud (hekate `clock_uart_use_src_div`).
const UART_DIV_EN: u32 = 1 << 24;
const UART_DIV_1M: u32 = 49;
const UART_DIV_3M: u32 = 15;
const UARTB: [Clock; 1] = [Clock {
    bank: &L,
    bit: 7,
    source: 0x17C,
    keep: 0,
    src: UART_DIV_EN | UART_DIV_1M,
    reset: true,
}];
const UARTC: [Clock; 1] = [Clock {
    bank: &H,
    bit: 23,
    source: 0x1A0,
    keep: 0,
    src: UART_DIV_EN | UART_DIV_1M,
    reset: true,
}];

/// Fuse controller (bank H bit 7): status only, for readers of fuses.
const FUSE: [Clock; 1] = [Clock {
    bank: &H,
    bit: 7,
    source: 0,
    keep: 0,
    src: 0,
    reset: true,
}];

fn table(dev: u64) -> Option<Device> {
    let needs = match dev {
        proto::USBD => Needs::UsbPlls,
        proto::FUSE => Needs::FuseVisible,
        proto::PCIE => Needs::Pcie,
        proto::PWM => Needs::Shared,
        proto::GPU => Needs::Gpu,
        proto::AUDIO => Needs::Audio,
        _ => Needs::Nothing,
    };
    let (clocks, parts): (&'static [Clock], u32) = match dev {
        proto::I2C1 => (&I2C1, 0),
        proto::I2C2 => (&I2C2, 0),
        proto::I2C3 => (&I2C3, 0),
        proto::I2C4 => (&I2C4, 0),
        proto::I2C5 => (&I2C5, 0),
        proto::I2C6 => (&I2C6, 0),
        proto::SDMMC1 => (&SDMMC1, 0),
        proto::SDMMC4 => (&SDMMC4, 0),
        proto::XUSB_HOST => (&XUSB_HOST, PART_XUSBA | PART_XUSBC),
        proto::USBD => (&USBD, 0),
        proto::USB2_TRK => (&USB2_TRK, 0),
        proto::FUSE => (&FUSE, 0),
        proto::PCIE => (&PCIE, PART_PCIE),
        proto::PWM => (&PWM, 0),
        proto::GPU => (&GPU, 0),
        proto::AUDIO => (&AUDIO, PART_AUD),
        proto::UARTB => (&UARTB, 0),
        proto::UARTC => (&UARTC, 0),
        _ => return None,
    };
    Some(Device {
        clocks,
        parts,
        needs,
    })
}

/// Partitions powered and unclamped. Without PMC access, assume off:
/// a missing driver beats a hung bus.
fn powered(pmc: Option<&Mmio>, parts: u32) -> bool {
    parts == 0
        || pmc.is_some_and(|p| {
            p.r32(PMC_PWRGATE_STATUS) & parts == parts && p.r32(PMC_CLAMP_STATUS) & parts == 0
        })
}

fn status(car: &Mmio, pmc: Option<&Mmio>, d: &Device) -> u64 {
    let mut st = 0;
    if d.clocks
        .iter()
        .all(|c| car.r32(c.bank.enb) & (1 << c.bit) != 0)
    {
        st |= proto::ST_CLOCK;
    }
    if d.clocks
        .iter()
        .all(|c| !c.reset || car.r32(c.bank.rst) & (1 << c.bit) == 0)
    {
        st |= proto::ST_OUT_OF_RESET;
    }
    let extra = match d.needs {
        Needs::Nothing | Needs::Shared => true,
        Needs::UsbPlls => car.r32(PLLU_BASE) & (PLL_ENABLE | PLL_LOCK) == PLL_ENABLE | PLL_LOCK,
        Needs::FuseVisible => car.r32(MISC_CLK_ENB) & FUSE_VISIBLE != 0,
        Needs::Pcie => car.r32(PLLE_MISC) & PLLE_MISC_LOCK != 0,
        Needs::Gpu => pmc.is_some_and(|p| p.r32(PMC_GPU_RG_CNTRL) & 1 == 0),
        Needs::Audio => car.r32(PLLA_BASE) & PLL_LOCK != 0,
    };
    if powered(pmc, d.parts) && extra {
        st |= proto::ST_POWERED;
    }
    st
}

/// Wait up to `us` for `bits` in a CAR register.
fn wait_bits(car: &Mmio, reg: usize, bits: u32, us: u64) -> bool {
    car.wait(reg, bits, bits, us)
}

/// PLLU (if not already running) then UTMIPLL, as hekate's USB init.
/// UTMIPLL lock only bounds the wait there too.
fn usb_plls(car: &Mmio, pmc: Option<&Mmio>) -> i64 {
    let pllu = car.r32(PLLU_BASE);
    if pllu & (PLL_ENABLE | PLL_LOCK) != PLL_ENABLE | PLL_LOCK {
        // Reference clock off while programming: DIVM 2, DIVN 25, 480 MHz.
        car.modify(PLLU_MISC, 0, 1 << 29);
        let cfg = (pllu & 0xFFE0_0000) | (1 << 24) | (1 << 16) | (0x19 << 8) | 2;
        car.w32(PLLU_BASE, cfg);
        car.w32(PLLU_BASE, cfg | PLL_ENABLE);
        if !wait_bits(car, PLLU_BASE, PLL_LOCK, 1000) {
            println!("[clk] PLLU: no lock");
            return EIO;
        }
        time::sleep_us(10);
    }
    car.modify(PLLU_BASE, 0, PLLU_OUTPUTS);
    // UTMIPLL: IDDQ under software control and off, then program it
    // with the PHY crystal clock gated.
    car.modify(UTMIPLL_HW_PWRDN_CFG0, 3, 1);
    time::sleep_us(10);
    car.modify(UTMIP_PLL_CFG2, UTMIP_XTAL_CLOCKEN, 0);
    car.w32(
        UTMIP_PLL_CFG0,
        (car.r32(UTMIP_PLL_CFG0) & 0xFF00_00FF) | (25 << 16) | (1 << 8),
    );
    car.w32(
        UTMIP_PLL_CFG2,
        (car.r32(UTMIP_PLL_CFG2) & 0xFF00_003F) | (24 << 18),
    );
    car.w32(
        UTMIP_PLL_CFG1,
        (car.r32(UTMIP_PLL_CFG1) & 0x07FF_A000) | (1 << 15) | 375,
    );
    wait_bits(car, UTMIPLL_HW_PWRDN_CFG0, UTMIPLL_LOCK, 20);
    car.modify(UTMIP_PLL_CFG2, 0, UTMIP_XTAL_CLOCKEN);
    // Samplers A-D powered up (force-powerdown bits off, powerup on).
    car.w32(
        UTMIP_PLL_CFG2,
        (car.r32(UTMIP_PLL_CFG2) & 0xFEFF_FFEA) | 0x200_0000 | 0x28 | 2,
    );
    if let Some(p) = pmc {
        p.modify(PMC_USB_AO, 0xC, 0);
    }
    0
}

/// PCIe reference PLL, 100 MHz for the UPHY bricks. Sequence from
/// Linux `clk_plle_tegra210_enable`: software control, out of IDDQ,
/// dividers with spread spectrum bypassed, then enable, lock and
/// spread spectrum back on.
fn plle(car: &Mmio) -> i64 {
    // Driven by the hardware sequencer already (loader): leave it be.
    if car.r32(PLLE_AUX) & PLLE_AUX_SEQ_ENABLE != 0 {
        return 0;
    }
    car.modify(PLLE_BASE, PLLE_LOCK_OVERRIDE, 0);
    car.modify(
        PLLE_AUX,
        PLLE_AUX_SEQ_ENABLE,
        PLLE_AUX_ENABLE_SWCTL | PLLE_AUX_SS_SWCTL,
    );
    time::sleep_us(1);
    car.modify(
        PLLE_MISC,
        PLLE_MISC_IDDQ_SW_VALUE,
        PLLE_MISC_IDDQ_SW_CTRL | PLLE_MISC_LOCK_ENABLE | PLLE_MISC_PTS | PLLE_MISC_VREG,
    );
    time::sleep_us(5);

    // Dividers written with the modulation bypassed.
    car.modify(
        PLLE_SS_CNTL,
        0,
        PLLE_SS_BYPASS | PLLE_SS_INTERP_RESET | PLLE_SS_SSC_BYP,
    );
    let base = (PLLE_DIVCML << 24) | (PLLE_DIVN << 8) | PLLE_DIVM;
    car.w32(PLLE_BASE, base);
    time::sleep_us(1);
    car.w32(PLLE_BASE, base | PLLE_ENABLE);
    if !car.wait(PLLE_MISC, PLLE_MISC_LOCK, PLLE_MISC_LOCK, 1000) {
        println!("[clk] PLLE: no lock");
        return EIO;
    }

    // Spread spectrum back on: coefficients, then bypass off, then the
    // interpolator out of reset.
    let mut ss = car.r32(PLLE_SS_CNTL);
    ss &= !(PLLE_SS_CENTER | PLLE_SS_INVERT | PLLE_SS_COEFF_MASK);
    ss |= PLLE_SS_COEFF;
    car.w32(PLLE_SS_CNTL, ss);
    ss &= !(PLLE_SS_SSC_BYP | PLLE_SS_BYPASS);
    car.w32(PLLE_SS_CNTL, ss);
    time::sleep_us(1);
    ss &= !PLLE_SS_INTERP_RESET;
    car.w32(PLLE_SS_CNTL, ss);
    time::sleep_us(1);
    car.modify(PLLE_AUX, 0, PLLE_AUX_USE_LOCKDET);
    0
}

/// Ungate a PMC power partition (hekate `pmc_domain_pwrgate_set`, then
/// the clamp removal its own comment prescribes).
fn ungate(pmc: &Mmio, part_id: u32) -> bool {
    let mask = 1u32 << part_id;
    if pmc.r32(PMC_PWRGATE_STATUS) & mask == 0 {
        if !pmc.wait(PMC_PWRGATE_TOGGLE, PWRGATE_TOGGLE_START, 0, 5000) {
            return false;
        }
        pmc.w32(PMC_PWRGATE_TOGGLE, part_id | PWRGATE_TOGGLE_START);
        if !pmc.wait(PMC_PWRGATE_STATUS, mask, mask, 5000) {
            return false;
        }
        time::sleep_us(10);
    }
    pmc.w32(PMC_REMOVE_CLAMPING, mask);
    pmc.wait(PMC_CLAMP_STATUS, mask, 0, 5000)
}

/// PCIe root complex. Order from Linux `tegra_powergate_sequence_power_up`:
/// clocks running but held in reset, partition on, clamps off, resets
/// released. The AFI registers answer only at the end of it (a read
/// before that hangs the bus).
fn pcie_up(car: &Mmio, pmc: Option<&Mmio>) -> i64 {
    let Some(pmc) = pmc else {
        return ENODEV;
    };
    for c in PCIE.iter() {
        car.w32(c.bank.rst_set, 1 << c.bit);
    }
    car.w32(U.rst_set, PCIEXCLK_RESET);
    car.w32(Y.rst_set, UPHY_PEX_RESET);
    for c in PCIE.iter() {
        car.w32(c.bank.enb_set, 1 << c.bit);
    }
    time::sleep_us(2);

    if !ungate(pmc, PART_PCIE_ID) {
        // Clocks back off: a block left half-powered hangs its bus.
        for c in PCIE.iter() {
            car.w32(c.bank.enb_set + CLR, 1 << c.bit);
        }
        println!("[clk] PCIe partition stays gated");
        return EIO;
    }
    let r = plle(car);
    if r != 0 {
        return r;
    }
    pex_pads_on(pmc);
    // Pad control block shared with XUSB: released, never re-asserted.
    car.w32(RST_DEV_W_CLR, PADCTL_RESET);
    car.w32(Y.rst_set + CLR, UPHY_PEX_RESET);
    for c in PCIE.iter() {
        car.w32(c.bank.rst_set + CLR, 1 << c.bit);
    }
    car.w32(U.rst_set + CLR, PCIEXCLK_RESET);
    let _ = car.r32(U.rst);
    0
}

/// PLLA (Linux clk-tegra210.c `pll_a_freq_table`, 38.4 MHz reference):
/// M 3, N 57, P 2 and the sigma-delta fraction 0x0333, 368.64 MHz, then
/// PLLA_OUT0 at /10 = 36.864 MHz (divider field 2 * 10 - 2).
const PLLA_BASE: usize = 0xB0;
const PLLA_OUT: usize = 0xB4;
const PLLA_MISC1: usize = 0xB8;
const PLLA_MISC0: usize = 0xBC;
const PLLA_MISC2: usize = 0x5D8;
const PLLA_IDDQ: u32 = 1 << 25;
const PLLA_MISC0_DEFAULT: u32 = 0x1200_0020;
const PLLA_SDM_EN: u32 = 1 << 26;
const PLLA_DIVS: u32 = 3 | 57 << 8 | 2 << 20;
const PLLA_OUT0_ON: u32 = (2 * 10 - 2) << 8 | 1 << 1 | 1;
const LVL2_CLK_GATE_OVRC: usize = 0x3A0;
const LVL2_CLK_GATE_OVRE: usize = 0x554;
/// AHUB's I2S blocks (five, 0x100 apart from AHUB + 0x1000).
const I2S_CTRL: usize = 0xA0;
const I2S_CG: usize = 0x88;

/// Audio hub, in Linux's order: AUD partition on, pads out of deep
/// power-down and the audio-hv pad at 1.8 V (the DT's), PLLA, its
/// output, then the clocks with their resets held until all run, and the
/// MBIST workaround Linux applies after the partition comes up.
fn audio_up(car: &Mmio, pmc: Option<&Mmio>, ahub: Option<&Mmio>) -> i64 {
    let Some(pmc) = pmc else {
        return ENODEV;
    };
    if !ungate(pmc, PART_AUD_ID) {
        return EIO;
    }
    // Pads: the audio pads (I2S1 and MCLK sit on them) out of deep
    // power-down; audio-hv at 1.8 V from the PMIC's SD3 as the DT says.
    pmc.w32(PMC_IO_DPD_REQ, IO_DPD_OFF | AUDIO_PAD);
    pmc.w32(PMC_IO_DPD2_REQ, IO_DPD_OFF | AUDIO_HV_PAD);
    pmc.wait(PMC_IO_DPD_STATUS, AUDIO_PAD, 0, 250_000);
    pmc.wait(PMC_IO_DPD2_STATUS, AUDIO_HV_PAD, 0, 250_000);
    let det = pmc.r32(PMC_PWR_DET_VAL);
    println!(
        "[clk] audio-hv pad voltage {:#x} (3.3 V bit {})",
        det,
        det >> 18 & 1
    );
    if det & AUDIO_HV_3V3 != 0 {
        pmc.w32(PMC_PWR_DET, pmc.r32(PMC_PWR_DET) | AUDIO_HV_3V3);
        pmc.w32(PMC_PWR_DET_VAL, det & !AUDIO_HV_3V3);
        let _ = pmc.r32(PMC_PWR_DET_VAL);
        time::sleep_us(250);
    }
    if car.r32(PLLA_BASE) & PLL_LOCK == 0 {
        car.w32(PLLA_BASE, car.r32(PLLA_BASE) & !PLLA_IDDQ);
        let _ = car.r32(PLLA_BASE);
        time::sleep_us(5);
        car.w32(PLLA_MISC0, PLLA_MISC0_DEFAULT);
        car.w32(PLLA_MISC1, 0x0333);
        car.w32(PLLA_MISC2, car.r32(PLLA_MISC2) | PLLA_SDM_EN);
        car.w32(PLLA_BASE, PLLA_DIVS);
        car.w32(PLLA_BASE, PLLA_DIVS | PLL_ENABLE);
        if !wait_bits(car, PLLA_BASE, PLL_LOCK, 1_000) {
            println!("[clk] PLLA did not lock: {:#x}", car.r32(PLLA_BASE));
            return EIO;
        }
    }
    car.w32(PLLA_OUT, PLLA_OUT0_ON);
    time::sleep_us(10);
    // Clocks and resets, as `enable`'s loop, but all the resets are held
    // until every clock runs.
    for c in AUDIO.iter() {
        let m = 1u32 << c.bit;
        if c.reset {
            car.w32(c.bank.rst_set, m);
        }
        car.w32(c.bank.enb_set + CLR, m);
        if c.source != 0 {
            car.w32(c.source, (car.r32(c.source) & c.keep) | c.src);
        }
        car.w32(c.bank.enb_set, m);
    }
    time::sleep_us(5);
    for c in AUDIO.iter().filter(|c| c.reset) {
        car.w32(c.bank.rst_set + CLR, 1 << c.bit);
    }
    let _ = car.r32(V.rst);
    time::sleep_us(5);
    // MBIST workaround (clk-tegra210.c `tegra210_ape_mbist_war`).
    if let Some(ahub) = ahub {
        let (c, e) = (car.r32(LVL2_CLK_GATE_OVRC), car.r32(LVL2_CLK_GATE_OVRE));
        car.w32(LVL2_CLK_GATE_OVRC, c | 1 << 1);
        car.w32(LVL2_CLK_GATE_OVRE, e | 1 << 10 | 1 << 11);
        let _ = car.r32(LVL2_CLK_GATE_OVRE);
        time::sleep_us(1);
        for i in 0..5 {
            let b = 0x1000 + 0x100 * i;
            let ctrl = ahub.r32(b + I2S_CTRL);
            ahub.w32(b + I2S_CTRL, ctrl | 1 << 10);
            ahub.w32(b + I2S_CG, 0);
            let _ = ahub.r32(b + I2S_CG);
            ahub.w32(b + I2S_CG, 1);
            ahub.w32(b + I2S_CTRL, ctrl);
            let _ = ahub.r32(b + I2S_CTRL);
        }
        car.w32(LVL2_CLK_GATE_OVRC, c);
        car.w32(LVL2_CLK_GATE_OVRE, e);
        let _ = car.r32(LVL2_CLK_GATE_OVRE);
        time::sleep_us(1);
    }
    0
}

/// GPU, its rail already on. Order from NVIDIA's railgate code (Linux
/// `nvkm_device_tegra_power_up`): clocks running with the reset held,
/// clamps off, then the reset released, 10 us apart.
fn gpu_up(car: &Mmio, pmc: Option<&Mmio>) -> i64 {
    let Some(pmc) = pmc else {
        return ENODEV;
    };
    car.w32(PLLP_OUTC, (car.r32(PLLP_OUTC) & 0xFFFF) | PLLP_OUT5);
    for c in GPU.iter() {
        if c.reset {
            car.w32(c.bank.rst_set, 1 << c.bit);
        }
        car.w32(c.bank.enb_set, 1 << c.bit);
    }
    time::sleep_us(10);
    pmc.w32(PMC_GPU_RG_CNTRL, 0);
    let _ = pmc.r32(PMC_GPU_RG_CNTRL);
    time::sleep_us(10);
    car.w32(X.rst_set + CLR, 1 << GPU[0].bit);
    let _ = car.r32(X.rst);
    time::sleep_us(10);
    0
}

/// Reverse of `gpu_up`: clamps on, reset, clocks off; the rail may go
/// off afterwards.
fn gpu_down(car: &Mmio, pmc: Option<&Mmio>) {
    if let Some(pmc) = pmc {
        pmc.w32(PMC_GPU_RG_CNTRL, 1);
        let _ = pmc.r32(PMC_GPU_RG_CNTRL);
        time::sleep_us(10);
    }
    car.w32(X.rst_set, 1 << GPU[0].bit);
    time::sleep_us(10);
    disable(car, table(proto::GPU).as_ref().unwrap());
}

/// PEX bias and REFCLK pads out of deep power-down, as Linux's pinctrl
/// default state for the root complex does. Only requested if needed.
fn pex_pads_on(pmc: &Mmio) {
    let before = pmc.r32(PMC_IO_DPD_STATUS) & PEX_PADS;
    if before != 0 {
        pmc.w32(PMC_IO_DPD_REQ, IO_DPD_OFF | PEX_PADS);
        pmc.wait(PMC_IO_DPD_STATUS, PEX_PADS, 0, 250_000);
    }
    println!(
        "[clk] PEX pads deep power-down {:#x} -> {:#x}",
        before,
        pmc.r32(PMC_IO_DPD_STATUS) & PEX_PADS
    );
}

/// `PCIE_LINK`: the PCIEXCLK reset alone. Linux `tegra_pcie_pm_resume`
/// programs the AFI (crossbar, port enables) with it held, then releases
/// it before enabling the ports. Only once the root complex is up.
fn pcie_link(car: &Mmio, pmc: Option<&Mmio>, label: u32) -> (i64, u64) {
    let ready = table(proto::PCIE).is_some_and(|d| status(car, pmc, &d) == proto::ST_READY);
    match label {
        proto::ENABLE | proto::DISABLE if !ready => (EIO, 0),
        proto::ENABLE => {
            car.w32(U.rst_set + CLR, PCIEXCLK_RESET);
            let _ = car.r32(U.rst);
            (0, 0)
        }
        proto::DISABLE => {
            car.w32(U.rst_set, PCIEXCLK_RESET);
            let _ = car.r32(U.rst);
            (0, 0)
        }
        proto::STATUS if car.r32(U.rst) & PCIEXCLK_RESET == 0 => (0, proto::ST_OUT_OF_RESET),
        proto::STATUS => (0, 0),
        _ => (EINVAL, 0),
    }
}

fn enable(car: &Mmio, pmc: Option<&Mmio>, ahub: Option<&Mmio>, d: &Device) -> i64 {
    if d.needs == Needs::Audio {
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
        let r = audio_up(car, pmc, ahub);
        if r != 0 {
            return r;
        }
        let st = status(car, pmc, d);
        if st != proto::ST_READY {
            println!(
                "[clk] audio state {:#x}: PLLA {:#x}, V enb {:#x} rst {:#x}, L enb {:#x}, Y enb {:#x} rst {:#x}, parts {:#x}",
                st,
                car.r32(PLLA_BASE),
                car.r32(V.enb),
                car.r32(V.rst),
                car.r32(L.enb),
                car.r32(Y.enb),
                car.r32(Y.rst),
                pmc.map_or(0, |p| p.r32(PMC_PWRGATE_STATUS))
            );
        }
        return if st == proto::ST_READY { 0 } else { EIO };
    }
    if d.needs == Needs::Pcie {
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
        let r = pcie_up(car, pmc);
        if r != 0 {
            return r;
        }
        return if status(car, pmc, d) == proto::ST_READY {
            0
        } else {
            EIO
        };
    }
    if d.needs == Needs::Gpu {
        // Up already: a reset would lose the loaded falcons.
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
        let r = gpu_up(car, pmc);
        if r != 0 {
            return r;
        }
        return if status(car, pmc, d) == proto::ST_READY {
            0
        } else {
            EIO
        };
    }
    if d.parts != 0 {
        // PMC is not owned by clk: partition off, touch nothing.
        if !powered(pmc, d.parts) {
            return ENODEV;
        }
        // Already up (firmware loaded by an earlier stage): a reset
        // would lose it.
        if status(car, pmc, d) == proto::ST_READY {
            return 0;
        }
    }
    if d.needs == Needs::FuseVisible {
        // A reset would lose the sensed fuses: status only.
        return EINVAL;
    }
    if d.needs == Needs::Shared && status(car, pmc, d) == proto::ST_READY {
        return 0;
    }
    if d.needs == Needs::UsbPlls {
        let r = usb_plls(car, pmc);
        if r != 0 {
            return r;
        }
    }
    for c in d.clocks {
        let m = 1u32 << c.bit;
        if c.reset {
            car.w32(c.bank.rst_set, m); // assert reset
        }
        car.w32(c.bank.enb_set + CLR, m); // clock off
        if c.source != 0 {
            car.w32(c.source, (car.r32(c.source) & c.keep) | c.src);
        }
        car.w32(c.bank.enb_set, m); // clock on
        time::sleep_us(2);
        if c.reset {
            car.w32(c.bank.rst_set + CLR, m); // release reset
            let _ = car.r32(c.bank.rst);
        }
    }
    if d.needs == Needs::UsbPlls {
        car.w32(RST_DEV_W_CLR, PADCTL_RESET);
    }
    if status(car, pmc, d) == proto::ST_READY {
        0
    } else {
        EIO
    }
}

/// `UART_BAUD`: the divider of a running UART's source clock.
fn uart_baud(car: &Mmio, dev: u64, baud: u64) -> i64 {
    let (Some(d), Some(div)) = (
        table(dev).filter(|_| matches!(dev, proto::UARTB | proto::UARTC)),
        match baud {
            proto::BAUD_1M => Some(UART_DIV_1M),
            proto::BAUD_3M => Some(UART_DIV_3M),
            _ => None,
        },
    ) else {
        return EINVAL;
    };
    let c = &d.clocks[0];
    if car.r32(c.bank.enb) & (1 << c.bit) == 0 {
        return ENODEV;
    }
    car.w32(
        c.source,
        (car.r32(c.source) & 0xE000_0000) | UART_DIV_EN | div,
    );
    let _ = car.r32(c.source);
    0
}

fn disable(car: &Mmio, d: &Device) {
    for c in d.clocks.iter().rev() {
        let m = 1u32 << c.bit;
        if c.reset {
            car.w32(c.bank.rst_set, m);
        }
        car.w32(c.bank.enb_set + CLR, m);
    }
}

libnx::entry!(main);

fn main() -> i32 {
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("clk: CAR registers");
    let pmc = Mmio::map(drv::dev_slot(1), PMC_PAGE, 0x1000).ok();
    // The audio hub's I2S blocks, for the MBIST workaround after its
    // partition comes up.
    let ahub = Mmio::map(drv::dev_slot(2), AHUB_PAGE, 0x2000).ok();
    let ep = drv::publish("clk").expect("clk: publish");
    println!("[clk] clock service ready");
    drv::serve(ep, |rx| {
        if rx.msg.mr[0] == proto::PCIE_LINK {
            let (r, st) = pcie_link(&car, pmc.as_ref(), rx.msg.label());
            let mut m = Msg::new(r as u32, 1, 0);
            m.mr[0] = st;
            return Some(m);
        }
        if rx.msg.label() == proto::UART_BAUD {
            return Some(Msg::new(
                uart_baud(&car, rx.msg.mr[0], rx.msg.mr[1]) as u32,
                0,
                0,
            ));
        }
        let mut st = 0;
        let r = match (rx.msg.label(), table(rx.msg.mr[0])) {
            (proto::ENABLE, Some(d)) => enable(&car, pmc.as_ref(), ahub.as_ref(), &d),
            (proto::DISABLE, Some(d)) if d.needs == Needs::Gpu => {
                gpu_down(&car, pmc.as_ref());
                0
            }
            (proto::DISABLE, Some(d)) if !matches!(d.needs, Needs::FuseVisible | Needs::Shared) => {
                disable(&car, &d);
                0
            }
            (proto::STATUS, Some(d)) => {
                st = status(&car, pmc.as_ref(), &d);
                0
            }
            _ => EINVAL,
        };
        let mut m = Msg::new(r as u32, 2, 0);
        m.mr[0] = st;
        if rx.msg.mr[0] == proto::PCIE {
            m.mr[1] = pmc.as_ref().map_or(0, |p| p.r32(PMC_IO_DPD_STATUS)) as u64;
        }
        Some(m)
    })
}

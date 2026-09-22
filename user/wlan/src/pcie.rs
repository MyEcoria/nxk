//! Tegra X1 PCIe root complex: rail, module pins, AFI windows, link.
//! Registers and sequences from Linux `drivers/pci/controller/pci-tegra.c`
//! and `drivers/phy/tegra/xusb-tegra210.c` (T210 data), clocks and the
//! power partition through the `clk` service.

use libnx::abi::err::{EIO, ENODEV, ENXIO, ETIMEDOUT};
use libnx::abi::proto::{clk, power};
use libnx::drv::Mmio;
use libnx::ipc::{self, Msg};
use libnx::{ns, println, time};

/// Root port registers (port 0 at +0, port 1 at +0x1000), the PADS
/// block at +0x3000 and the AFI at +0x3800: one grant covers them all.
/// T210 drives the lanes from the pad control block, so PADS is unused.
pub const PCIE_BLOCK: u64 = 0x0100_0000;
pub const PCIE_BLOCK_LEN: u64 = 0x4000;
const PORT_REGS: [usize; 2] = [0x0000, 0x1000];
const AFI: usize = 0x3800;
/// Configuration space: a single 4 KiB window the AFI retargets.
pub const CFG_WINDOW: u64 = 0x0200_0000;
pub const CFG_WINDOW_LEN: u64 = 0x1000;
/// Downstream memory windows handed to the endpoint's BARs.
pub const MEM_WINDOW: u64 = 0x1300_0000;
pub const MEM_WINDOW_LEN: u64 = 0x1_0000;
pub const PREFETCH_WINDOW: u64 = 0x2000_0000;
pub const PREFETCH_WINDOW_LEN: u64 = 0x40_0000;
/// Pad control block: the UPHY bricks the PCIe lanes live on.
pub const PADCTL: u64 = 0x7009_F000;

// AFI registers (offsets from the AFI block).
const AFI_AXI_BAR_SZ: [usize; 6] = [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14];
const AFI_AXI_BAR_START: [usize; 6] = [0x18, 0x1C, 0x20, 0x24, 0x28, 0x2C];
const AFI_FPCI_BAR: [usize; 6] = [0x30, 0x34, 0x38, 0x3C, 0x40, 0x44];
const AFI_MSI_BAR_SZ: usize = 0x60;
const AFI_MSI_FPCI_BAR_ST: usize = 0x64;
const AFI_MSI_AXI_BAR_ST: usize = 0x68;
const AFI_CONFIGURATION: usize = 0xAC;
const AFI_CONFIGURATION_EN_FPCI: u32 = 1 << 0;
const AFI_CONFIGURATION_CLKEN_OVERRIDE: u32 = 1 << 31;
const AFI_FPCI_ERROR_MASKS: usize = 0xB0;
const AFI_INTR_MASK: usize = 0xB4;
const AFI_INTR_MASK_INT_MASK: u32 = 1 << 0;
const AFI_INTR_CODE: usize = 0xB8;
const AFI_INTR_CODE_MASK: u32 = 0xF;
/// Legacy INTx from the endpoint: the interrupt the driver waits for.
pub const AFI_INTR_LEGACY: u32 = 6;
const AFI_SM_INTR_ENABLE: usize = 0xC4;
const AFI_AFI_INTR_ENABLE: usize = 0xC8;
/// Error reporting: slave, decode, target, write and FPCI decode errors,
/// plus presence sense on T210.
const AFI_INTR_ERRORS: u32 = 0x3F;
const AFI_INTR_EN_PRSNT_SENSE: u32 = 1 << 8;
const AFI_PCIE_CONFIG: usize = 0x0F8;
const AFI_PCIE_CONFIG_PCIE_DISABLE_ALL: u32 = 0xE;
const AFI_PCIE_CONFIG_XBAR_MASK: u32 = 0xF << 20;
/// T210 lane split: port 0 x4, port 1 x1.
const AFI_PCIE_CONFIG_XBAR_X4_X1: u32 = 0x1 << 20;
const AFI_PCIE_CONFIG_CLKREQ_GPIO_ALL: u32 = 0x7 << 29;
const AFI_FUSE: usize = 0x104;
const AFI_FUSE_PCIE_T0_GEN2_DIS: u32 = 1 << 2;
const AFI_PEX_CTRL: [usize; 2] = [0x110, 0x118];
const AFI_PEX_CTRL_RST: u32 = 1 << 0;
const AFI_PEX_CTRL_CLKREQ_EN: u32 = 1 << 1;
const AFI_PEX_CTRL_REFCLK_EN: u32 = 1 << 3;
const AFI_PEX_CTRL_OVERRIDE_EN: u32 = 1 << 4;
const AFI_PEXBIAS_CTRL_0: usize = 0x168;

// Root port registers.
const RP_LINK_CONTROL_STATUS: usize = 0x090;
const RP_LINK_STATUS_DL_ACTIVE: u32 = 0x2000_0000;
const RP_LINK_CONTROL_STATUS_2: usize = 0x0B0;
const RP_VEND_XP: usize = 0xF00;
const RP_VEND_XP_DL_UP: u32 = 1 << 30;
const RP_VEND_XP_OPPORTUNISTIC_ACK: u32 = 1 << 27;
const RP_VEND_XP_OPPORTUNISTIC_UPDATEFC: u32 = 1 << 28;
const RP_VEND_XP_UPDATE_FC_MASK: u32 = 0xFF << 18;
/// T210 flow-control threshold (bits 25:18).
const RP_VEND_XP_UPDATE_FC: u32 = 0x0180_0000;
const RP_VEND_CTL0: usize = 0xF44;
const RP_VEND_CTL0_DSK_PULSE_MASK: u32 = 0xF << 12;
const RP_VEND_CTL0_DSK_PULSE: u32 = 0x9 << 12;
const RP_VEND_CTL1: usize = 0xF48;
const RP_VEND_CTL1_ERPT: u32 = 1 << 13;
const RP_VEND_XP_BIST: usize = 0xF4C;
const RP_VEND_XP_BIST_GOTO_L1_L2: u32 = 1 << 28;
const RP_VEND_CTL2: usize = 0xFA8;
const RP_VEND_CTL2_PCA_ENABLE: u32 = 1 << 7;
const RP_PRIV_MISC: usize = 0xFE0;
const RP_PRIV_MISC_PRSNT_MAP_EP_PRSNT: u32 = 0xE;
const RP_PRIV_MISC_PRSNT_MAP_EP_ABSNT: u32 = 0xF;
const RP_PRIV_MISC_CTLR_CLK_CLAMP_ENABLE: u32 = 1 << 23;
const RP_PRIV_MISC_TMS_CLK_CLAMP_ENABLE: u32 = 1 << 31;
const RP_PRIV_MISC_CTLR_CLAMP_MASK: u32 = 0x7F << 16;
const RP_PRIV_MISC_TMS_CLAMP_MASK: u32 = 0x7F << 24;
const RP_PRIV_MISC_CLAMP_THRESHOLD: u32 = (0xF << 16) | (0xF << 24);
/// Receiver tuning, T210 values (`tegra210_pcie.ectl`).
const RP_ECTL: [(usize, u32, u32); 8] = [
    (0xE84, 0xFFFF, 0x0000_000F),
    (0xE8C, 0xFFFF << 16, 0x0067 << 16),
    (0xE90, 0xFFFF_FFFF, 0x5501_0000),
    (0xE94, 0xFFFF, 0x0000_0001),
    (0xEA4, 0xFFFF, 0x0000_008F),
    (0xEAC, 0xFFFF << 16, 0x00C7 << 16),
    (0xEB0, 0xFFFF_FFFF, 0x5501_0000),
    (0xEB4, 0xFFFF, 0x0000_0001),
];

// UPHY PLL for the PCIe bricks (pad control block).
const PADCTL_USB3_PAD_MUX: usize = 0x028;
const PADCTL_UPHY_PLL_P0_CTL1: usize = 0x360;
const PLL_CTL1_IDDQ: u32 = 1 << 0;
const PLL_CTL1_SLEEP_MASK: u32 = 0x3 << 1;
const PLL_CTL1_ENABLE: u32 = 1 << 3;
const PLL_CTL1_PWR_OVRD: u32 = 1 << 4;
const PLL_CTL1_LOCKDET: u32 = 1 << 15;
const PLL_CTL1_MDIV_MASK: u32 = 0x3 << 16;
const PLL_CTL1_NDIV_MASK: u32 = 0xFF << 20;
/// 100 MHz reference: NDIV 0x19, MDIV 0 (the "USB" value, shared by PCIe).
const PLL_CTL1_NDIV: u32 = 0x19 << 20;
const PADCTL_UPHY_PLL_P0_CTL2: usize = 0x364;
const PLL_CTL2_CAL_EN: u32 = 1 << 0;
const PLL_CTL2_CAL_DONE: u32 = 1 << 1;
const PLL_CTL2_CAL_OVRD: u32 = 1 << 2;
const PLL_CTL2_CAL_CTRL_MASK: u32 = 0xFF_FFFF << 4;
const PLL_CTL2_CAL_CTRL: u32 = 0x136 << 4;
const PADCTL_UPHY_PLL_P0_CTL4: usize = 0x36C;
const PLL_CTL4_REFCLK_SEL_MASK: u32 = 0xF << 4;
const PLL_CTL4_REFCLKBUF_EN: u32 = 1 << 8;
const PLL_CTL4_TXCLKREF_SEL_MASK: u32 = 0x3 << 12;
const PLL_CTL4_TXCLKREF_SEL: u32 = 0x2 << 12;
const PLL_CTL4_TXCLKREF_EN: u32 = 1 << 15;
const PADCTL_UPHY_PLL_P0_CTL5: usize = 0x370;
const PLL_CTL5_DCO_CTRL_MASK: u32 = 0xFF << 16;
const PLL_CTL5_DCO_CTRL: u32 = 0x2A << 16;
const PADCTL_UPHY_PLL_P0_CTL8: usize = 0x37C;
const PLL_CTL8_RCAL_EN: u32 = 1 << 12;
const PLL_CTL8_RCAL_CLK_EN: u32 = 1 << 13;
const PLL_CTL8_RCAL_OVRD: u32 = 1 << 15;
const PLL_CTL8_RCAL_DONE: u32 = 1 << 31;
/// Seven UPHY lanes; PCIe owns the ones XUSB does not.
const UPHY_LANES: u32 = 7;

// PCI configuration space.
/// The root ports are NVIDIA bridges.
const VENDOR_NVIDIA: u16 = 0x10DE;
const CFG_VENDOR: usize = 0x00;
const CFG_COMMAND: usize = 0x04;
const CFG_COMMAND_IO: u32 = 1 << 0;
const CFG_COMMAND_MEMORY: u32 = 1 << 1;
const CFG_COMMAND_MASTER: u32 = 1 << 2;
/// INTx disable: cleared, the endpoint signals through the AFI.
const CFG_COMMAND_INTX_OFF: u32 = 1 << 10;
const CFG_BAR0: usize = 0x10;
const CFG_PRIMARY_BUS: usize = 0x18;

/// Module control pins (Switch): WIFI_EN on PH0, WIFI_RST on PH1
/// (hekate `PINMUX_AUX_WIFI_EN`/`_RST`, Switch device tree `wifi_en_ph0`
/// and `wifi_rst_ph1`).
pub const APB_MISC: u64 = 0x7000_0000;
pub const PINMUX: u64 = 0x7000_3000;
pub const GPIO: u64 = 0x6000_D000;
const PIN_WIFI_EN: usize = 0x1B4;
const PIN_WIFI_RST: usize = 0x1B8;
const GP_WIFI_EN_CFGPADCTRL: usize = 0xB64;
const GP_WIFI_RST_CFGPADCTRL: usize = 0xB68;
const PINMUX_TRISTATE: u32 = 1 << 4;
/// Function 1 of both pins: plain GPIO output.
const PINMUX_FUNC_RSVD1: u32 = 1;
const PORT_H: usize = 7;
const PIN_EN: u32 = 1 << 0;
const PIN_RST: u32 = 1 << 1;
const GPIO_CNF: usize = 0x00;
const GPIO_OE: usize = 0x10;
const GPIO_OUT: usize = 0x20;

/// Where bring-up stopped, with the value that said so: the only way
/// to tell a dead rail from an untrained link without a UART.
pub struct Fail {
    pub step: &'static str,
    pub err: i64,
    /// Register that decided it, when one did: 0 and 0xFFFFFFFF are both
    /// meaningful (nothing there vs a bus error), hence the option.
    pub value: Option<u32>,
}

impl Fail {
    fn new(step: &'static str, err: i64) -> Self {
        Self {
            step,
            err,
            value: None,
        }
    }

    fn read(step: &'static str, err: i64, value: u32) -> Self {
        Self {
            step,
            err,
            value: Some(value),
        }
    }
}

/// An endpoint found behind a root port.
pub struct Endpoint {
    pub port: usize,
    pub vendor: u16,
    pub device: u16,
    /// Physical addresses the BARs were placed at (0: unused).
    pub bars: [u64; 6],
}

pub struct Pcie {
    regs: Mmio,
    cfg: Mmio,
    padctl: Mmio,
    misc: Mmio,
    pinmux: Mmio,
    gpio: Mmio,
}

impl Pcie {
    pub fn new(regs: Mmio, cfg: Mmio, padctl: Mmio, misc: Mmio, pinmux: Mmio, gpio: Mmio) -> Self {
        Self {
            regs,
            cfg,
            padctl,
            misc,
            pinmux,
            gpio,
        }
    }

    fn afi(&self) -> Mmio {
        self.regs.offset(AFI)
    }

    fn port(&self, i: usize) -> Mmio {
        self.regs.offset(PORT_REGS[i])
    }

    fn gpio_set(&self, reg: usize, pins: u32, value: u32) {
        let off = (PORT_H >> 2) * 0x100 + (PORT_H & 3) * 4;
        self.gpio.modify(reg + off, pins & !value, value & pins);
        let _ = self.gpio.r32(reg + off);
    }

    /// Wi-Fi module out of reset: pins as GPIO outputs, enable high,
    /// reset released after it. The chip needs the rail before this.
    fn module_on(&self) {
        self.misc.w32(GP_WIFI_EN_CFGPADCTRL, 0);
        self.misc.w32(GP_WIFI_RST_CFGPADCTRL, 0);
        for pin in [PIN_WIFI_EN, PIN_WIFI_RST] {
            let v = self.pinmux.r32(pin);
            self.pinmux
                .w32(pin, (v & !(PINMUX_TRISTATE | 3)) | PINMUX_FUNC_RSVD1);
        }
        let both = PIN_EN | PIN_RST;
        self.gpio_set(GPIO_CNF, both, both);
        self.gpio_set(GPIO_OUT, both, 0);
        self.gpio_set(GPIO_OE, both, both);
        time::sleep_ms(1);
        self.gpio_set(GPIO_OUT, PIN_EN, PIN_EN);
        // Datasheet-style power-on: enable settles before reset lifts.
        time::sleep_ms(5);
        self.gpio_set(GPIO_OUT, PIN_RST, PIN_RST);
        time::sleep_ms(10);
    }

    /// Module pins back down, in reverse order.
    pub fn module_off(&self) {
        self.gpio_set(GPIO_OUT, PIN_RST, 0);
        time::sleep_ms(1);
        self.gpio_set(GPIO_OUT, PIN_EN, 0);
    }

    /// UPHY PLL of the PCIe bricks (`tegra210_pex_uphy_enable`).
    /// Already locked (loader, or XUSB SuperSpeed): left alone.
    fn uphy_pll(&self) -> Result<(), i64> {
        let p = &self.padctl;
        if p.r32(PADCTL_UPHY_PLL_P0_CTL1) & PLL_CTL1_LOCKDET != 0 {
            return Ok(());
        }
        p.modify(
            PADCTL_UPHY_PLL_P0_CTL2,
            PLL_CTL2_CAL_CTRL_MASK,
            PLL_CTL2_CAL_CTRL,
        );
        p.modify(
            PADCTL_UPHY_PLL_P0_CTL5,
            PLL_CTL5_DCO_CTRL_MASK,
            PLL_CTL5_DCO_CTRL,
        );
        // Power, calibration and resistor calibration under software control.
        p.modify(PADCTL_UPHY_PLL_P0_CTL1, 0, PLL_CTL1_PWR_OVRD);
        p.modify(PADCTL_UPHY_PLL_P0_CTL2, 0, PLL_CTL2_CAL_OVRD);
        p.modify(PADCTL_UPHY_PLL_P0_CTL8, 0, PLL_CTL8_RCAL_OVRD);

        p.modify(
            PADCTL_UPHY_PLL_P0_CTL4,
            PLL_CTL4_TXCLKREF_SEL_MASK | PLL_CTL4_REFCLK_SEL_MASK,
            PLL_CTL4_TXCLKREF_SEL | PLL_CTL4_TXCLKREF_EN,
        );
        p.modify(
            PADCTL_UPHY_PLL_P0_CTL1,
            PLL_CTL1_MDIV_MASK | PLL_CTL1_NDIV_MASK,
            PLL_CTL1_NDIV,
        );
        p.modify(
            PADCTL_UPHY_PLL_P0_CTL1,
            PLL_CTL1_IDDQ | PLL_CTL1_SLEEP_MASK,
            0,
        );
        time::sleep_us(20);
        p.modify(PADCTL_UPHY_PLL_P0_CTL4, 0, PLL_CTL4_REFCLKBUF_EN);

        // Frequency calibration, then its acknowledge.
        p.modify(PADCTL_UPHY_PLL_P0_CTL2, 0, PLL_CTL2_CAL_EN);
        if !p.wait(
            PADCTL_UPHY_PLL_P0_CTL2,
            PLL_CTL2_CAL_DONE,
            PLL_CTL2_CAL_DONE,
            100_000,
        ) {
            return Err(-ETIMEDOUT);
        }
        p.modify(PADCTL_UPHY_PLL_P0_CTL2, PLL_CTL2_CAL_EN, 0);
        if !p.wait(PADCTL_UPHY_PLL_P0_CTL2, PLL_CTL2_CAL_DONE, 0, 100_000) {
            return Err(-ETIMEDOUT);
        }

        p.modify(PADCTL_UPHY_PLL_P0_CTL1, 0, PLL_CTL1_ENABLE);
        if !p.wait(
            PADCTL_UPHY_PLL_P0_CTL1,
            PLL_CTL1_LOCKDET,
            PLL_CTL1_LOCKDET,
            100_000,
        ) {
            return Err(-ETIMEDOUT);
        }

        // Termination resistor calibration.
        p.modify(
            PADCTL_UPHY_PLL_P0_CTL8,
            0,
            PLL_CTL8_RCAL_EN | PLL_CTL8_RCAL_CLK_EN,
        );
        if !p.wait(
            PADCTL_UPHY_PLL_P0_CTL8,
            PLL_CTL8_RCAL_DONE,
            PLL_CTL8_RCAL_DONE,
            100_000,
        ) {
            return Err(-ETIMEDOUT);
        }
        p.modify(PADCTL_UPHY_PLL_P0_CTL8, PLL_CTL8_RCAL_EN, 0);
        if !p.wait(PADCTL_UPHY_PLL_P0_CTL8, PLL_CTL8_RCAL_DONE, 0, 100_000) {
            return Err(-ETIMEDOUT);
        }
        p.modify(PADCTL_UPHY_PLL_P0_CTL8, PLL_CTL8_RCAL_CLK_EN, 0);

        // Back to hardware control of power and calibration.
        p.modify(PADCTL_UPHY_PLL_P0_CTL1, PLL_CTL1_PWR_OVRD, 0);
        p.modify(PADCTL_UPHY_PLL_P0_CTL2, PLL_CTL2_CAL_OVRD, 0);
        p.modify(PADCTL_UPHY_PLL_P0_CTL8, PLL_CTL8_RCAL_OVRD, 0);
        time::sleep_us(20);
        Ok(())
    }

    /// Lanes out of IDDQ. The mux says who owns each lane; it is left as
    /// the loader set it and only reported.
    fn uphy_lanes(&self) {
        let mux = self.padctl.r32(PADCTL_USB3_PAD_MUX);
        let mut v = mux;
        for i in 0..UPHY_LANES {
            v |= 1 << (1 + i);
        }
        self.padctl.w32(PADCTL_USB3_PAD_MUX, v);
        println!("[wlan] UPHY lane mux {:#010x} -> {:#010x}", mux, v);
    }

    /// AFI: address windows, error reporting, legacy interrupts.
    fn controller(&self) {
        let afi = self.afi();
        // Slot clock bias pad off (T210 has no slot).
        afi.w32(AFI_PEXBIAS_CTRL_0, 0);
        let mut cfg = afi.r32(AFI_PCIE_CONFIG);
        cfg &= !AFI_PCIE_CONFIG_XBAR_MASK;
        cfg |= AFI_PCIE_CONFIG_PCIE_DISABLE_ALL | AFI_PCIE_CONFIG_XBAR_X4_X1;
        cfg |= AFI_PCIE_CONFIG_CLKREQ_GPIO_ALL;
        // Both ports on: which one the module sits on is read from the link.
        for i in 0..PORT_REGS.len() {
            cfg &= !(1 << (i + 1));
            cfg &= !(1 << (i + 29));
        }
        afi.w32(AFI_PCIE_CONFIG, cfg);
        afi.modify(AFI_FUSE, AFI_FUSE_PCIE_T0_GEN2_DIS, 0);
        afi.modify(
            AFI_CONFIGURATION,
            0,
            AFI_CONFIGURATION_EN_FPCI | AFI_CONFIGURATION_CLKEN_OVERRIDE,
        );
        afi.w32(
            AFI_AFI_INTR_ENABLE,
            AFI_INTR_ERRORS | AFI_INTR_EN_PRSNT_SENSE,
        );
        afi.w32(AFI_SM_INTR_ENABLE, 0xFFFF_FFFF);
        // Legacy INTx only: no MSI capability in the kernel.
        afi.w32(AFI_INTR_MASK, AFI_INTR_MASK_INT_MASK);
        afi.w32(AFI_FPCI_ERROR_MASKS, 0);

        // Window 0 is the configuration page, 2 and 3 the memory windows
        // (FPCI address = (addr >> 12) << 4 | 1).
        afi.w32(AFI_AXI_BAR_START[0], CFG_WINDOW as u32);
        afi.w32(AFI_AXI_BAR_SZ[0], (CFG_WINDOW_LEN >> 12) as u32);
        afi.w32(AFI_AXI_BAR_START[2], PREFETCH_WINDOW as u32);
        afi.w32(AFI_AXI_BAR_SZ[2], (PREFETCH_WINDOW_LEN >> 12) as u32);
        afi.w32(AFI_FPCI_BAR[2], fpci_bar(PREFETCH_WINDOW));
        afi.w32(AFI_AXI_BAR_START[3], MEM_WINDOW as u32);
        afi.w32(AFI_AXI_BAR_SZ[3], (MEM_WINDOW_LEN >> 12) as u32);
        afi.w32(AFI_FPCI_BAR[3], fpci_bar(MEM_WINDOW));
        for i in [1, 4, 5] {
            afi.w32(AFI_AXI_BAR_START[i], 0);
            afi.w32(AFI_AXI_BAR_SZ[i], 0);
            afi.w32(AFI_FPCI_BAR[i], 0);
        }
        afi.w32(AFI_MSI_FPCI_BAR_ST, 0);
        afi.w32(AFI_MSI_AXI_BAR_ST, 0);
        afi.w32(AFI_MSI_BAR_SZ, 0);
    }

    /// Reference clock, PERST pulse and the tuning a T210 port wants.
    fn port_enable(&self, i: usize) {
        let afi = self.afi();
        let ctrl = AFI_PEX_CTRL[i];
        afi.modify(
            ctrl,
            0,
            AFI_PEX_CTRL_REFCLK_EN | AFI_PEX_CTRL_CLKREQ_EN | AFI_PEX_CTRL_OVERRIDE_EN,
        );
        self.port_reset(i);

        let rp = self.port(i);
        rp.modify(RP_VEND_CTL2, 0, RP_VEND_CTL2_PCA_ENABLE);
        rp.modify(RP_VEND_CTL1, 0, RP_VEND_CTL1_ERPT);
        rp.modify(
            RP_VEND_XP,
            0,
            RP_VEND_XP_OPPORTUNISTIC_ACK | RP_VEND_XP_OPPORTUNISTIC_UPDATEFC,
        );
        // Wait for the DLLP before L1/L2: truncated PM messages look like
        // receiver errors.
        rp.modify(RP_VEND_XP_BIST, 0, RP_VEND_XP_BIST_GOTO_L1_L2);
        rp.modify(
            RP_PRIV_MISC,
            RP_PRIV_MISC_CTLR_CLAMP_MASK | RP_PRIV_MISC_TMS_CLAMP_MASK,
            RP_PRIV_MISC_CTLR_CLK_CLAMP_ENABLE
                | RP_PRIV_MISC_TMS_CLK_CLAMP_ENABLE
                | RP_PRIV_MISC_CLAMP_THRESHOLD,
        );
        for (off, mask, val) in RP_ECTL {
            rp.modify(off, mask, val);
        }
        rp.modify(
            RP_VEND_CTL0,
            RP_VEND_CTL0_DSK_PULSE_MASK,
            RP_VEND_CTL0_DSK_PULSE,
        );
        rp.modify(RP_VEND_XP, RP_VEND_XP_UPDATE_FC_MASK, RP_VEND_XP_UPDATE_FC);
        // Gen1 first: some endpoints never train when both speeds are
        // advertised. Gen2 is reached by retraining afterwards.
        rp.modify(RP_LINK_CONTROL_STATUS_2, 0xF, 1);
    }

    fn port_reset(&self, i: usize) {
        let afi = self.afi();
        afi.modify(AFI_PEX_CTRL[i], AFI_PEX_CTRL_RST, 0);
        time::sleep_ms(2);
        afi.modify(AFI_PEX_CTRL[i], 0, AFI_PEX_CTRL_RST);
    }

    /// Data link up on a port, with the retries the reference driver uses.
    fn port_link(&self, i: usize) -> bool {
        let rp = self.port(i);
        rp.modify(
            RP_PRIV_MISC,
            RP_PRIV_MISC_PRSNT_MAP_EP_ABSNT,
            RP_PRIV_MISC_PRSNT_MAP_EP_PRSNT,
        );
        for _ in 0..3 {
            if rp.wait(RP_VEND_XP, RP_VEND_XP_DL_UP, RP_VEND_XP_DL_UP, 200_000)
                && rp.wait(
                    RP_LINK_CONTROL_STATUS,
                    RP_LINK_STATUS_DL_ACTIVE,
                    RP_LINK_STATUS_DL_ACTIVE,
                    200_000,
                )
            {
                return true;
            }
            self.port_reset(i);
        }
        false
    }

    /// Root port answering for a device number on bus 0 (port 0 is
    /// device 1), if there is one.
    fn root_port(&self, dev: u8) -> Option<Mmio> {
        let i = (dev as usize).checked_sub(1)?;
        (i < PORT_REGS.len()).then(|| self.port(i))
    }

    /// Configuration word of a device. Bus 0 is the root ports
    /// themselves; deeper buses go through the 4 KiB AFI window.
    fn cfg_read(&self, bus: u8, dev: u8, off: usize) -> u32 {
        if bus == 0 {
            return match self.root_port(dev) {
                Some(rp) => rp.r32(off & !3),
                None => !0,
            };
        }
        let o = cfg_offset(bus, dev, off);
        self.afi()
            .w32(AFI_FPCI_BAR[0], 0xFE10_0000 + ((o as u32 & !0xFFF) >> 8));
        self.cfg.r32(o & 0xFFC)
    }

    fn cfg_write(&self, bus: u8, dev: u8, off: usize, v: u32) {
        if bus == 0 {
            if let Some(rp) = self.root_port(dev) {
                rp.w32(off & !3, v);
            }
            return;
        }
        let o = cfg_offset(bus, dev, off);
        self.afi()
            .w32(AFI_FPCI_BAR[0], 0xFE10_0000 + ((o as u32 & !0xFFF) >> 8));
        self.cfg.w32(o & 0xFFC, v);
    }

    /// Configuration write to the endpoint (bus 1, device 0): the
    /// driver moves its BAR0 backplane window this way.
    pub fn ep_cfg_write(&self, off: usize, v: u32) {
        self.cfg_write(1, 0, off, v);
    }

    /// Pending AFI interrupt: legacy INTx from the endpoint, or an error
    /// worth reporting. Clears the code register either way.
    pub fn interrupt(&self) -> u32 {
        let afi = self.afi();
        let code = afi.r32(AFI_INTR_CODE) & AFI_INTR_CODE_MASK;
        afi.w32(AFI_INTR_CODE, 0);
        code
    }

    /// Whole bring-up: rail, module, clocks, UPHY, AFI, link, then the
    /// endpoint behind whichever port trained.
    pub fn bring_up(&self) -> Result<Endpoint, Fail> {
        rail(true).map_err(|e| Fail::new("PCIe rail (PMIC LDO1)", e))?;
        clock_on().map_err(|e| Fail::new("PCIe clocks and PLLE", e))?;
        // The root ports identify themselves once their clock runs. No
        // NVIDIA bridge there means no root complex (emulator): stop
        // before seconds of calibration and link-training timeouts.
        let id = self.port(0).r32(CFG_VENDOR);
        if id as u16 != VENDOR_NVIDIA {
            return Err(Fail::read("no NVIDIA root port", -ENODEV, id));
        }
        self.module_on();
        self.uphy_pll()
            .map_err(|e| Fail::new("UPHY PLL of the PCIe lanes", e))?;
        self.uphy_lanes();
        self.controller();

        let mut found = None;
        for i in 0..PORT_REGS.len() {
            self.port_enable(i);
            if self.port_link(i) {
                found = Some(i);
                break;
            }
            println!(
                "[wlan] PCIe port {}: no link (RP_VEND_XP {:#010x})",
                i,
                self.port(i).r32(RP_VEND_XP)
            );
        }
        let port = found.ok_or_else(|| {
            Fail::read(
                "no link on either port",
                -ENODEV,
                self.port(0).r32(RP_VEND_XP),
            )
        })?;
        println!("[wlan] PCIe port {}: link up", port);
        self.enumerate(port)
            .map_err(|e| Fail::new("endpoint behind the port", e))
    }

    /// One endpoint behind the port: bus numbers, BARs placed in our
    /// windows, memory and bus mastering on.
    fn enumerate(&self, port: usize) -> Result<Endpoint, i64> {
        let dev = port as u8 + 1;
        // Root port as a bridge: bus 0 upstream, bus 1 below it.
        let secondary = 1u32;
        self.cfg_write(
            0,
            dev,
            CFG_PRIMARY_BUS,
            (secondary << 16) | (secondary << 8),
        );
        let id = self.cfg_read(1, 0, CFG_VENDOR);
        if id == 0xFFFF_FFFF || id == 0 {
            return Err(-ENXIO);
        }
        let mut ep = Endpoint {
            port,
            vendor: id as u16,
            device: (id >> 16) as u16,
            bars: [0; 6],
        };

        let mut mem = MEM_WINDOW;
        let mut pref = PREFETCH_WINDOW;
        let mut i = 0;
        while i < 6 {
            let off = CFG_BAR0 + i * 4;
            self.cfg_write(1, 0, off, 0xFFFF_FFFF);
            let probe = self.cfg_read(1, 0, off);
            self.cfg_write(1, 0, off, 0);
            if probe == 0 {
                i += 1;
                continue;
            }
            let is_mem = probe & 1 == 0;
            let is_64 = is_mem && (probe & 0x6) == 0x4;
            let prefetchable = is_mem && probe & 0x8 != 0;
            let size = (!(probe & !0xF)).wrapping_add(1) as u64;
            if !is_mem || size == 0 {
                i += 1;
                continue;
            }
            let base = if prefetchable { &mut pref } else { &mut mem };
            let addr = (*base).next_multiple_of(size.max(0x1000));
            let end = if prefetchable {
                PREFETCH_WINDOW + PREFETCH_WINDOW_LEN
            } else {
                MEM_WINDOW + MEM_WINDOW_LEN
            };
            if addr + size > end {
                println!("[wlan] BAR{}: {} bytes do not fit", i, size);
                return Err(-EIO);
            }
            self.cfg_write(1, 0, off, addr as u32);
            if is_64 {
                self.cfg_write(1, 0, off + 4, (addr >> 32) as u32);
            }
            ep.bars[i] = addr;
            *base = addr + size;
            i += if is_64 { 2 } else { 1 };
        }

        let cmd = self.cfg_read(1, 0, CFG_COMMAND);
        self.cfg_write(
            1,
            0,
            CFG_COMMAND,
            (cmd & !(CFG_COMMAND_INTX_OFF | CFG_COMMAND_IO))
                | CFG_COMMAND_MEMORY
                | CFG_COMMAND_MASTER,
        );
        Ok(ep)
    }
}

/// FPCI address of an AXI window (pci-tegra `tegra_pcie_setup_translations`).
fn fpci_bar(addr: u64) -> u32 {
    ((((addr >> 12) & 0x0FFF_FFFF) << 4) | 1) as u32
}

/// Extended configuration offset (`tegra_pcie_conf_offset`, function 0).
fn cfg_offset(bus: u8, dev: u8, where_: usize) -> usize {
    ((where_ & 0xF00) << 16) | ((bus as usize) << 16) | ((dev as usize) << 11) | (where_ & 0xFF)
}

/// PCIe/XUSB rail through the `power` service.
fn rail(on: bool) -> Result<(), i64> {
    let ep = ns::wait_for_timeout("power", 2_000_000_000)?;
    let mut m = Msg::new(power::PCIE_POWER, 1, 0);
    m.mr[0] = on as u64;
    ipc::checked(ipc::call(ep, &m)?).map(|_| ())
}

/// Partition, PLLE and the AFI/PCIe clocks through the `clk` service.
fn clock_on() -> Result<(), i64> {
    let ep = ns::wait_for_timeout("clk", 2_000_000_000)?;
    let mut m = Msg::new(clk::ENABLE, 1, 0);
    m.mr[0] = clk::PCIE;
    ipc::checked(ipc::call(ep, &m)?)?;
    let mut m = Msg::new(clk::STATUS, 1, 0);
    m.mr[0] = clk::PCIE;
    let st = ipc::checked(ipc::call(ep, &m)?)?;
    if st.mr[0] & clk::ST_READY != clk::ST_READY {
        println!("[wlan] PCIe clocks not ready ({:#x})", st.mr[0]);
        return Err(-EIO);
    }
    Ok(())
}

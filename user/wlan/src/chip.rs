//! BCM4356 behind the PCIe link: backplane window and chip identity.
//! Window register and ChipCommon layout as Linux `brcmfmac/pcie.c`
//! (`BRCMF_PCIE_BAR0_WINDOW`) and `brcmfmac/chip.c`.

use libnx::drv::Mmio;
use libnx::{fs, println};

/// BAR0 maps a 4 KiB view of the backplane; this config register says
/// which backplane address it starts at.
pub const CFG_BAR0_WINDOW: usize = 0x80;
pub const BAR0_WINDOW_LEN: u64 = 0x1000;
/// ChipCommon core: `chipid` is its first register.
pub const CHIPCOMMON: u32 = 0x1800_0000;
const CC_CHIPID: usize = 0x00;

/// Wi-Fi chip of the Switch (Erista and Mariko).
pub const VENDOR_BROADCOM: u16 = 0x14E4;
pub const DEVICE_BCM4356: u16 = 0x43EC;
pub const CHIP_BCM4356: u16 = 0x4356;

/// Firmware and calibration data, from the SD card: proprietary blobs,
/// never part of the image.
pub const FW_PATH: &str = "/firmware/brcm/brcmfmac4356-pcie.bin";
pub const NVRAM_PATH: &str = "/firmware/brcm/brcmfmac4356-pcie.txt";

pub struct Chip {
    pub id: u16,
    pub rev: u8,
    /// Firmware and NVRAM found on the card.
    pub firmware: bool,
}

/// Identify the chip through the BAR0 backplane window. `window` moves
/// that window (a configuration write on the endpoint).
pub fn identify(bar0: &Mmio, window: &dyn Fn(u32)) -> Chip {
    window(CHIPCOMMON);
    let chipid = bar0.r32(CC_CHIPID);
    let id = chipid as u16;
    let rev = ((chipid >> 16) & 0xF) as u8;
    // Name the file that is actually missing: the calibration data is
    // public, the firmware itself comes off the console.
    let mut firmware = true;
    for path in [FW_PATH, NVRAM_PATH] {
        if fs::stat(path).is_err() {
            println!("[wlan] missing {}", path);
            firmware = false;
        }
    }
    Chip { id, rev, firmware }
}

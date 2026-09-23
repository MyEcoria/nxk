//! BCM4356 behind the PCIe link: backplane window, cores, their resets.
//! Layout and sequences as Linux `brcmfmac` (`pcie.c`, `chip.c`), cores
//! and wrapper registers as `bcma` (AXI interconnect, "AI" chips).

use alloc::vec::Vec;
use core::cell::Cell;

use libnx::drv::Mmio;
use libnx::{fs, println, time};

/// BAR0 maps a 4 KiB view of the backplane; this config register says
/// which backplane address it starts at.
pub const CFG_BAR0_WINDOW: usize = 0x80;
pub const BAR0_WINDOW_LEN: u64 = 0x1000;
/// BAR2: the ARM's tightly coupled memory (TCM), from address 0.
pub const BAR2_LEN: u64 = 0x40_0000;
/// ChipCommon core: `chipid` first, `eromptr` to the enumeration ROM.
pub const CHIPCOMMON: u32 = 0x1800_0000;
const CC_CHIPID: u32 = 0x00;
const CC_EROMPTR: u32 = 0xFC;

/// Wi-Fi chip of the Switch (Erista and Mariko).
pub const VENDOR_BROADCOM: u16 = 0x14E4;
pub const DEVICE_BCM4356: u16 = 0x43EC;
pub const CHIP_BCM4356: u16 = 0x4356;
/// TCM base of the 4350 family (`brcmf_chip_tcm_rambase`).
pub const RAMBASE_4356: u32 = 0x18_0000;

// Core ids.
pub const CORE_80211: u16 = 0x812;
const CORE_PMU: u16 = 0x827;
pub const CORE_PCIE2: u16 = 0x83C;
pub const CORE_ARM_CR4: u16 = 0x83E;
const CORE_GCI: u16 = 0x840;

// Enumeration ROM descriptors (`brcmf_chip_dmp_*`).
const DESC_TYPE: u32 = 0xF;
const DESC_VALID: u32 = 0x1;
const DESC_COMPONENT: u32 = 0x1;
const DESC_MASTER_PORT: u32 = 0x3;
const DESC_ADDRESS: u32 = 0x5;
const DESC_ADDRSIZE_GT32: u32 = 0x8;
const DESC_EOT: u32 = 0xF;
const SLAVE_SLAVE: u32 = 0;
const SLAVE_SWRAP: u32 = 2;
const SLAVE_MWRAP: u32 = 3;
const SIZE_4K: u32 = 0;
const SIZE_8K: u32 = 1;
const SIZE_DESC: u32 = 3;
/// The ROM is 4 KiB: a bound on the scan whatever it contains.
const EROM_WORDS: usize = 1024;

// Wrapper registers, from a core's wrapper base.
const AI_IOCTL: u32 = 0x408;
const AI_IOCTL_CLK: u32 = 1 << 0;
const AI_IOCTL_FGC: u32 = 1 << 1;
const AI_RESET_CTL: u32 = 0x800;
const AI_RESET: u32 = 1 << 0;
const CR4_IOCTL_CPUHALT: u32 = 1 << 5;
const D11_IOCTL_PHYCLOCKEN: u32 = 1 << 2;
const D11_IOCTL_PHYRESET: u32 = 1 << 3;

// ARM CR4 core registers: TCM banks.
const CR4_CAP: u32 = 0x04;
const CR4_BANKIDX: u32 = 0x40;
const CR4_BANKINFO: u32 = 0x44;
const CR4_BANK_1K: u32 = 1 << 9;

// PCIe2 core registers.
const PCIE2_MAILBOXMASK: u32 = 0x4C;
const PCIE2_CONFIGADDR: u32 = 0x120;
const PCIE2_CONFIGDATA: u32 = 0x124;

/// Firmware and calibration data, from the SD card: proprietary blobs,
/// never part of the image.
pub const FW_PATH: &str = "/firmware/brcm/brcmfmac4356-pcie.bin";
pub const NVRAM_PATH: &str = "/firmware/brcm/brcmfmac4356-pcie.txt";

/// Any backplane address through the BAR0 window, moved (a config
/// write on the endpoint) only when the 4 KiB page changes.
pub struct Backplane<'a> {
    bar0: &'a Mmio,
    window: &'a dyn Fn(u32),
    page: Cell<u32>,
}

impl<'a> Backplane<'a> {
    pub fn new(bar0: &'a Mmio, window: &'a dyn Fn(u32)) -> Self {
        Self {
            bar0,
            window,
            page: Cell::new(u32::MAX),
        }
    }

    fn at(&self, addr: u32) -> usize {
        let page = addr & !0xFFF;
        if self.page.get() != page {
            (self.window)(page);
            self.page.set(page);
        }
        (addr & 0xFFF) as usize
    }

    pub fn r32(&self, addr: u32) -> u32 {
        let off = self.at(addr);
        self.bar0.r32(off)
    }

    pub fn w32(&self, addr: u32, v: u32) {
        let off = self.at(addr);
        self.bar0.w32(off, v)
    }
}

pub struct Chip {
    pub id: u16,
    pub rev: u8,
    /// Firmware and NVRAM found on the card.
    pub firmware: bool,
}

/// Chip identity from ChipCommon, and whether its firmware is on the card.
pub fn identify(bp: &Backplane) -> Chip {
    let chipid = bp.r32(CHIPCOMMON + CC_CHIPID);
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

/// One core: registers at `base`, its AI wrapper at `wrap`.
#[derive(Clone, Copy)]
pub struct Core {
    pub id: u16,
    pub rev: u8,
    pub base: u32,
    pub wrap: u32,
}

pub fn find(cores: &[Core], id: u16) -> Option<Core> {
    cores.iter().copied().find(|c| c.id == id)
}

/// Next descriptor and its type (address descriptors of any width as one).
fn desc(bp: &Backplane, at: &mut u32) -> (u32, u32) {
    let v = bp.r32(*at);
    *at += 4;
    let mut t = v & DESC_TYPE;
    if t & !DESC_ADDRSIZE_GT32 == DESC_ADDRESS {
        t = DESC_ADDRESS;
    }
    (v, t)
}

/// First 4/8 KiB register slave and wrapper of the component the scan is
/// in (`brcmf_chip_dmp_get_regaddr`); `None` if it has no port.
fn regaddr(bp: &Backplane, at: &mut u32) -> Option<(u32, u32)> {
    let (_, t) = desc(bp, at);
    let wraptype = match t {
        DESC_MASTER_PORT => SLAVE_MWRAP,
        DESC_ADDRESS => {
            *at -= 4;
            SLAVE_SWRAP
        }
        _ => {
            *at -= 4;
            return None;
        }
    };
    let (mut base, mut wrap) = (0, 0);
    while base == 0 || wrap == 0 {
        let (v, t) = loop {
            let (v, t) = desc(bp, at);
            if t == DESC_EOT {
                *at -= 4;
                return None;
            }
            if t == DESC_ADDRESS || t == DESC_COMPONENT {
                break (v, t);
            }
        };
        // The next component starts: keep what this one had.
        if t == DESC_COMPONENT {
            *at -= 4;
            break;
        }
        if v & DESC_ADDRSIZE_GT32 != 0 {
            desc(bp, at);
        }
        let size = (v >> 4) & 0x3;
        if size == SIZE_DESC {
            let (s, _) = desc(bp, at);
            if s & DESC_ADDRSIZE_GT32 != 0 {
                desc(bp, at);
            }
        }
        if size != SIZE_4K && size != SIZE_8K {
            continue;
        }
        let stype = (v >> 6) & 0x3;
        if base == 0 && stype == SLAVE_SLAVE {
            base = v & !0xFFF;
        }
        if wrap == 0 && stype == wraptype {
            wrap = v & !0xFFF;
        }
    }
    Some((base, wrap))
}

/// Cores listed by the enumeration ROM (`brcmf_chip_dmp_erom_scan`).
pub fn cores(bp: &Backplane) -> Vec<Core> {
    let mut at = bp.r32(CHIPCOMMON + CC_EROMPTR);
    let end = at + 4 * EROM_WORDS as u32;
    let mut out = Vec::new();
    while at < end {
        let (v, t) = desc(bp, &mut at);
        if t == DESC_EOT {
            break;
        }
        if v & DESC_VALID == 0 || t != DESC_COMPONENT {
            continue;
        }
        let id = ((v >> 8) & 0xFFF) as u16;
        let (v, t) = desc(bp, &mut at);
        if t != DESC_COMPONENT {
            break;
        }
        let (nmw, nsw, rev) = ((v >> 14) & 0x1F, (v >> 19) & 0x1F, (v >> 24) as u8);
        if nmw + nsw == 0 && id != CORE_PMU && id != CORE_GCI {
            continue;
        }
        if let Some((base, wrap)) = regaddr(bp, &mut at) {
            out.push(Core {
                id,
                rev,
                base,
                wrap,
            });
        }
    }
    out
}

/// Core held in reset with `reset` in its control bits
/// (`brcmf_chip_ai_coredisable`).
fn core_disable(bp: &Backplane, c: &Core, prereset: u32, reset: u32) {
    if bp.r32(c.wrap + AI_RESET_CTL) & AI_RESET == 0 {
        bp.w32(c.wrap + AI_IOCTL, prereset | AI_IOCTL_FGC | AI_IOCTL_CLK);
        bp.r32(c.wrap + AI_IOCTL);
        bp.w32(c.wrap + AI_RESET_CTL, AI_RESET);
        time::sleep_us(20);
        time::poll(300, || bp.r32(c.wrap + AI_RESET_CTL) == AI_RESET);
    }
    bp.w32(c.wrap + AI_IOCTL, reset | AI_IOCTL_FGC | AI_IOCTL_CLK);
    bp.r32(c.wrap + AI_IOCTL);
}

/// Reset pulse, then running with `postreset` (`brcmf_chip_ai_resetcore`).
fn core_reset(bp: &Backplane, c: &Core, prereset: u32, reset: u32, postreset: u32) {
    core_disable(bp, c, prereset, reset);
    for _ in 0..50 {
        if bp.r32(c.wrap + AI_RESET_CTL) & AI_RESET == 0 {
            break;
        }
        bp.w32(c.wrap + AI_RESET_CTL, 0);
        time::sleep_us(50);
    }
    bp.w32(c.wrap + AI_IOCTL, postreset | AI_IOCTL_CLK);
    bp.r32(c.wrap + AI_IOCTL);
}

/// ARM halted and the radio core in reset (`brcmf_chip_cr4_set_passive`):
/// the state its RAM can be written in.
pub fn set_passive(bp: &Backplane, cores: &[Core]) {
    if let Some(arm) = find(cores, CORE_ARM_CR4) {
        let keep = bp.r32(arm.wrap + AI_IOCTL) & CR4_IOCTL_CPUHALT;
        core_reset(bp, &arm, keep, CR4_IOCTL_CPUHALT, CR4_IOCTL_CPUHALT);
    }
    if let Some(d11) = find(cores, CORE_80211) {
        core_reset(
            bp,
            &d11,
            D11_IOCTL_PHYRESET | D11_IOCTL_PHYCLOCKEN,
            D11_IOCTL_PHYCLOCKEN,
            D11_IOCTL_PHYCLOCKEN,
        );
    }
}

/// ARM out of halt: it starts at the vector written at TCM address 0.
pub fn set_active(bp: &Backplane, arm: &Core) {
    core_reset(bp, arm, CR4_IOCTL_CPUHALT, 0, 0);
}

/// TCM size from the CR4's bank registers (`brcmf_chip_tcm_ramsize`).
pub fn ram_size(bp: &Backplane, arm: &Core) -> u32 {
    let cap = bp.r32(arm.base + CR4_CAP);
    let banks = (cap & 0xF) + ((cap >> 4) & 0xF);
    let mut size = 0;
    for i in 0..banks {
        bp.w32(arm.base + CR4_BANKIDX, i);
        let info = bp.r32(arm.base + CR4_BANKINFO);
        let unit = if info & CR4_BANK_1K != 0 { 1024 } else { 8192 };
        size += ((info & 0x7F) + 1) * unit;
    }
    size
}

/// PCIe2 core: config word 0x4E0 re-latched, which sizes the BAR1
/// window (`brcmf_pcie_attach`), and mailbox interrupts masked until the
/// host serves them: INTx is level, an unmasked one would never stop.
pub fn pcie2_quiet(bp: &Backplane, pcie2: &Core) {
    bp.w32(pcie2.base + PCIE2_CONFIGADDR, 0x4E0);
    let v = bp.r32(pcie2.base + PCIE2_CONFIGDATA);
    bp.w32(pcie2.base + PCIE2_CONFIGDATA, v);
    bp.w32(pcie2.base + PCIE2_MAILBOXMASK, 0);
}

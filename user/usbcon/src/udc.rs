//! Tegra X1 USB2 OTG controller in device mode (ChipIdea-style EDCI, used
//! by the boot ROM for RCM and by hekate: bdk/usb/usbd.c, usb_t210.h).
//! Queue heads live in the controller's RAM, descriptors and buffers in
//! one uncached DMA block.

use core::arch::asm;

use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::drv::Mmio;
use libnx::mem::SharedMemory;
use libnx::time;

// Controller (t210_usb2d_t).
const DCCPARAMS: usize = 0x124;
const USBCMD: usize = 0x130;
const USBSTS: usize = 0x134;
const USBINTR: usize = 0x138;
/// PERIODICLISTBASE in host mode.
const DEVICEADDR: usize = 0x144;
/// ASYNCLISTADDR in host mode.
const EPLISTADDR: usize = 0x148;
const DEVLC: usize = 0x1B4;
const OTGSC: usize = 0x1F4;
const USBMODE: usize = 0x1F8;
const SETUPSTAT: usize = 0x208;
const PRIME: usize = 0x20C;
const FLUSH: usize = 0x210;
const STATUS: usize = 0x214;
const COMPLETE: usize = 0x218;
const EPCTRL0: usize = 0x21C;

// USB1 interface and UTMIP PHY.
const SUSP_CTRL: usize = 0x400;
const VBUS_SENSORS: usize = 0x404;
const XCVR_CFG0: usize = 0x808;
const BIAS_CFG0: usize = 0x80C;
const HSRX_CFG0: usize = 0x810;
const HSRX_CFG1: usize = 0x814;
const TX_CFG0: usize = 0x820;
const MISC_CFG1: usize = 0x828;
const DEBOUNCE_CFG0: usize = 0x82C;
const SPARE_CFG0: usize = 0x834;
const XCVR_CFG1: usize = 0x838;
const BIAS_CFG1: usize = 0x83C;
const BIAS_CFG2: usize = 0x850;
const XCVR_CFG2: usize = 0x854;
const XCVR_CFG3: usize = 0x858;

const SUSP_PHY_CLK_VALID: u32 = 1 << 7;
const SUSP_UTMIP_RESET: u32 = 1 << 11;
const SUSP_UTMIP_PHY_ENB: u32 = 1 << 12;
const XTAL_CLOCKEN: u32 = 1 << 30;

const CMD_RUN: u32 = 1 << 0;
const CMD_RST: u32 = 1 << 1;
const CMD_SUTW: u32 = 1 << 13;
const CMD_ITC: u32 = 0xFF << 16;
pub const STS_UI: u32 = 1 << 0;
pub const STS_UEI: u32 = 1 << 1;
pub const STS_PCI: u32 = 1 << 2;
pub const STS_URI: u32 = 1 << 6;
/// UI, UEI, PCI, FRI, SEI, AAI, URI, SRI, SLI.
const STS_ALL: u32 = 0x1FF;
const MODE_CM: u32 = 3;
const MODE_DEVICE: u32 = 2;
const OTGSC_ID_PULLUP: u32 = 1 << 5;
const OTGSC_IRQ_STS: u32 = 0x7F << 16;
const DEVLC_ASUS: u32 = 1 << 17;

// ENDPTCTRLn, receive half; the transmit half is the same at bit 16.
const EP_STALL: u32 = 1 << 0;
const EP_TYPE_SHIFT: u32 = 2;
const EP_TOGGLE_RESET: u32 = 1 << 6;
const EP_ENABLE: u32 = 1 << 7;

/// Queue-head RAM of the controller (hekate USB2_QH_USB2D_QH_EP_BASE).
const QH_RAM: usize = 0x1000;
const QH_RAM_PHYS: u32 = 0x7D00_1000;
const QH_SIZE: usize = 64;
const QH_CAPS: usize = 0x00;
const QH_NEXT: usize = 0x08;
const QH_TOKEN: usize = 0x0C;
const QH_SETUP: usize = 0x28;
const QH_IOS: u32 = 1 << 15;
const QH_ZLT_DIS: u32 = 1 << 29;

const TD_SIZE: usize = 32;
const TD_TERMINATE: u32 = 1;
const TOK_ACTIVE: u32 = 1 << 7;
/// Halted, data buffer error, transaction error.
const TOK_ERRORS: u32 = (1 << 6) | (1 << 5) | (1 << 3);
const TOK_IOC: u32 = 1 << 15;

/// Endpoints used: 0 (control), 1 (bulk), 2 (interrupt IN).
pub const NUM_EPS: usize = 3;
/// dTD of the zero-length packet ending a control IN data stage.
const TD_ZLP: usize = NUM_EPS * 2;
pub const DMA_SIZE: usize = 0x2000;

#[derive(Clone, Copy, PartialEq)]
pub enum Dir {
    Out = 0,
    In = 1,
}

#[derive(Clone, Copy)]
pub enum EpType {
    Control = 0,
    Bulk = 2,
    Interrupt = 3,
}

/// Endpoint bit of PRIME, FLUSH, STATUS and COMPLETE.
pub const fn bit(ep: usize, dir: Dir) -> u32 {
    1 << (ep + if matches!(dir, Dir::In) { 16 } else { 0 })
}

/// Descriptor and buffer writes reach memory before the controller looks.
#[inline(always)]
fn dma_wmb() {
    unsafe { asm!("dsb st", options(nostack, preserves_flags)) };
}

/// Controller status read before the memory it describes.
#[inline(always)]
fn dma_rmb() {
    unsafe { asm!("dsb ld", options(nostack, preserves_flags)) };
}

pub struct Udc {
    regs: Mmio,
    qh: Mmio,
    dma: SharedMemory,
    dma_pa: u32,
}

impl Udc {
    /// `regs`: controller page plus its queue-head RAM (0x2000 bytes).
    pub fn new(regs: Mmio) -> Result<Self, i64> {
        let dma = SharedMemory::new(DMA_SIZE, CONTIGUOUS | UNCACHED | DMA32)?;
        let dma_pa = dma.paddr(0)? as u32;
        Ok(Self {
            regs,
            qh: regs.offset(QH_RAM),
            dma,
            dma_pa,
        })
    }

    /// Endpoints the controller has (DCCPARAMS.DEN).
    pub fn endpoints(&self) -> u32 {
        self.regs.r32(DCCPARAMS) & 0x1F
    }

    /// UTMIP pad setup, PHY in reset, as hekate `_usb_init_phy` (the CAR
    /// and PMC steps are the clk service's). `trk` gates the tracking clock.
    pub fn phy_init(&self, trims: Option<(u32, u32)>, mut trk: impl FnMut(bool) -> bool) -> bool {
        let r = &self.regs;
        let susp = r.r32(SUSP_CTRL);
        r.w32(SUSP_CTRL, susp | SUSP_UTMIP_RESET);
        r.w32(SUSP_CTRL, susp | SUSP_UTMIP_PHY_ENB | SUSP_UTMIP_RESET);
        r.modify(MISC_CFG1, XTAL_CLOCKEN, 0);
        // B session valid forced: VBUS itself is not sensed.
        r.modify(VBUS_SENSORS, 0, 1 << 12);
        r.modify(VBUS_SENSORS, 0, 1 << 11);
        // Transceiver trims from FUSE_USB_CALIB(_EXT): setup, term range, RPD.
        if let Some((c, ext)) = trims {
            let x = (r.r32(XCVR_CFG0) & 0xFFFF_FFF0) | (c & 0xF);
            r.w32(
                XCVR_CFG0,
                (x & 0xFE3F_FFFF) | ((c & 0x3F) << 25 >> 29 << 22),
            );
            r.w32(
                XCVR_CFG1,
                (r.r32(XCVR_CFG1) & 0xFFC3_FFFF) | ((c << 21) >> 28 << 18),
            );
            r.w32(
                XCVR_CFG3,
                (r.r32(XCVR_CFG3) & 0xFFFF_C1FF) | ((ext & 0x1F) << 9),
            );
        }
        r.modify(XCVR_CFG0, 1 << 21, 0);
        r.w32(XCVR_CFG2, (r.r32(XCVR_CFG2) & 0xFFFF_F1FF) | 0x400);
        time::sleep_us(1);
        r.w32(DEBOUNCE_CFG0, (r.r32(DEBOUNCE_CFG0) & 0xFFFF_0000) | 0xBB80);
        r.w32(BIAS_CFG1, (r.r32(BIAS_CFG1) & 0xFFFF_C0FF) | 0x100);
        // hekate "patched" values: squelch, HS IREF cap, TX, HS RX.
        r.modify(BIAS_CFG2, 0, 2);
        r.w32(SPARE_CFG0, r.r32(SPARE_CFG0) & 0xFFFF_FE67);
        r.modify(TX_CFG0, 0, 0x8_0000);
        r.w32(
            HSRX_CFG0,
            (r.r32(HSRX_CFG0) & 0xF0F0_03FF) | 0x8_8000 | 0x4000,
        );
        r.modify(BIAS_CFG2, 7, 0);
        r.w32(HSRX_CFG1, (r.r32(HSRX_CFG1) & 0xFFFF_FFC1) | 0x12);
        r.modify(MISC_CFG1, 0, XTAL_CLOCKEN);

        // Bias pad tracking: two power-up/down cycles of PDTRK.
        if !trk(true) {
            return false;
        }
        r.w32(
            BIAS_CFG1,
            (r.r32(BIAS_CFG1) & 0xFFC0_3F07) | 0x7_8000 | 0x50,
        );
        r.modify(BIAS_CFG0, 1 << 10, 0);
        time::sleep_us(1);
        r.w32(BIAS_CFG1, (r.r32(BIAS_CFG1) & !1) | 2);
        time::sleep_us(100);
        r.w32(BIAS_CFG1, (r.r32(BIAS_CFG1) & 0xFF7F_FFFF) | 1);
        time::sleep_us(3);
        r.modify(BIAS_CFG1, 1, 0);
        time::sleep_us(100);
        r.w32(BIAS_CFG1, (r.r32(BIAS_CFG1) & 0xFF7F_FFFF) | 1);
        trk(false);
        time::sleep_us(1);

        // OTG, ID detectors and transceiver out of power-down.
        r.w32(BIAS_CFG0, r.r32(BIAS_CFG0) & 0xFF3F_F7FF);
        for (reg, b) in [
            (XCVR_CFG0, 14),
            (XCVR_CFG0, 16),
            (XCVR_CFG0, 18),
            (XCVR_CFG1, 2),
            (XCVR_CFG1, 4),
        ] {
            time::sleep_us(1);
            r.modify(reg, 1 << b, 0);
        }
        time::sleep_us(1);
        true
    }

    /// PHY out of reset, controller reset into device mode, interrupts
    /// masked (hekate `_usbd_reset_usb_otg_phy_device_mode`).
    pub fn reset_device_mode(&self) -> bool {
        let r = &self.regs;
        r.modify(SUSP_CTRL, SUSP_UTMIP_RESET, 0);
        if !r.wait(SUSP_CTRL, SUSP_PHY_CLK_VALID, SUSP_PHY_CLK_VALID, 200_000) {
            return false;
        }
        r.w32(DEVICEADDR, 0);
        r.w32(SETUPSTAT, r.r32(SETUPSTAT));
        r.w32(COMPLETE, r.r32(COMPLETE));
        r.modify(USBCMD, CMD_RUN, 0);
        r.modify(USBMODE, MODE_CM, 0);
        r.modify(USBCMD, 0, CMD_RST);
        if !r.wait(USBCMD, CMD_RST, 0, 200_000)
            || !r.wait(SUSP_CTRL, SUSP_PHY_CLK_VALID, SUSP_PHY_CLK_VALID, 200_000)
        {
            return false;
        }
        r.w32(USBMODE, (r.r32(USBMODE) & !MODE_CM) | MODE_DEVICE);
        if !r.wait(USBMODE, MODE_CM, MODE_DEVICE, 200_000) {
            return false;
        }
        r.w32(USBINTR, 0);
        r.w32(OTGSC, OTGSC_ID_PULLUP);
        r.w32(USBSTS, STS_ALL);
        r.w32(OTGSC, OTGSC_IRQ_STS);
        r.w32(SETUPSTAT, 0x7);
        r.modify(USBCMD, CMD_ITC, 0);
        true
    }

    /// Queue heads cleared, control endpoint on, interrupts on, then
    /// attach (pull-up on D+).
    pub fn attach(&self, irqs: u32) -> bool {
        for i in 0..NUM_EPS * 2 * QH_SIZE / 4 {
            self.qh.w32(i * 4, 0);
        }
        self.regs.w32(EPLISTADDR, QH_RAM_PHYS);
        self.enable(0, Dir::Out, EpType::Control, 64);
        self.enable(0, Dir::In, EpType::Control, 64);
        self.regs.modify(DEVLC, DEVLC_ASUS, 0);
        self.regs.w32(USBINTR, irqs);
        self.regs.modify(USBCMD, 0, CMD_RUN);
        self.regs.wait(USBCMD, CMD_RUN, CMD_RUN, 200_000)
    }

    /// Pending interrupt causes, acknowledged.
    pub fn take_status(&self) -> u32 {
        let s = self.regs.r32(USBSTS) & STS_ALL;
        self.regs.w32(USBSTS, s);
        s
    }

    /// Port speed after a reset: high speed (PSPD = 2)?
    pub fn high_speed(&self) -> bool {
        (self.regs.r32(DEVLC) >> 25) & 3 == 2
    }

    /// Bus reset: address 0, no setup or completion pending, nothing primed,
    /// data endpoints off (hekate, plus the ChipIdea flush loop).
    pub fn bus_reset(&self) {
        let r = &self.regs;
        r.w32(DEVICEADDR, 0);
        r.w32(SETUPSTAT, r.r32(SETUPSTAT));
        r.w32(COMPLETE, r.r32(COMPLETE));
        self.flush(!0);
        self.disable_data_eps();
    }

    /// Endpoints other than 0 off.
    pub fn disable_data_eps(&self) {
        for ep in 1..NUM_EPS {
            self.regs.w32(EPCTRL0 + 4 * ep, 0);
        }
    }

    pub fn set_address(&self, addr: u8) {
        self.regs.w32(DEVICEADDR, (addr as u32) << 25);
    }

    /// Completed endpoints (COMPLETE bits), acknowledged.
    pub fn take_complete(&self) -> u32 {
        let c = self.regs.r32(COMPLETE);
        self.regs.w32(COMPLETE, c);
        c
    }

    /// Next SETUP packet of EP0, read under the setup tripwire.
    pub fn take_setup(&self) -> Option<[u8; 8]> {
        let r = &self.regs;
        if r.r32(SETUPSTAT) & 1 == 0 {
            return None;
        }
        r.w32(SETUPSTAT, 1);
        let mut pkt = [0u8; 8];
        for _ in 0..1000 {
            r.modify(USBCMD, 0, CMD_SUTW);
            pkt[..4].copy_from_slice(&self.qh.r32(QH_SETUP).to_le_bytes());
            pkt[4..].copy_from_slice(&self.qh.r32(QH_SETUP + 4).to_le_bytes());
            // A new SETUP clears the tripwire: read again.
            if r.r32(USBCMD) & CMD_SUTW != 0 {
                break;
            }
        }
        r.modify(USBCMD, CMD_SUTW, 0);
        Some(pkt)
    }

    /// Endpoint on: queue head (max packet, no automatic zero-length
    /// packet), type, data toggle reset.
    pub fn enable(&self, ep: usize, dir: Dir, kind: EpType, max_packet: u32) {
        let q = qh_off(ep, dir);
        let ios = if ep == 0 && dir == Dir::Out {
            QH_IOS
        } else {
            0
        };
        self.qh
            .w32(q + QH_CAPS, ios | QH_ZLT_DIS | (max_packet & 0x7FF) << 16);
        self.qh.w32(q + QH_NEXT, TD_TERMINATE);
        self.qh.w32(q + QH_TOKEN, 0);
        let (half, other) = if dir == Dir::In { (16, 0) } else { (0, 16) };
        let mut v = self.regs.r32(EPCTRL0 + 4 * ep) & !(0xFF << half);
        v |= ((kind as u32) << EP_TYPE_SHIFT | EP_TOGGLE_RESET | EP_ENABLE) << half;
        // An unused half must not stay control type (ChipIdea data PID).
        if v & (EP_ENABLE << other) == 0 {
            v = (v & !(3 << (EP_TYPE_SHIFT + other)))
                | (EpType::Bulk as u32) << (EP_TYPE_SHIFT + other);
        }
        self.regs.w32(EPCTRL0 + 4 * ep, v);
    }

    /// Stall an endpoint (EP0: both halves, until the next SETUP) or clear
    /// its stall with a data toggle reset.
    pub fn stall(&self, ep: usize, dir: Dir, on: bool) {
        let reg = EPCTRL0 + 4 * ep;
        if ep == 0 {
            self.regs.modify(reg, 0, EP_STALL | EP_STALL << 16);
            return;
        }
        let half = if dir == Dir::In { 16 } else { 0 };
        if on {
            self.regs.modify(reg, 0, EP_STALL << half);
        } else {
            self.regs
                .modify(reg, EP_STALL << half, EP_TOGGLE_RESET << half);
        }
    }

    pub fn stalled(&self, ep: usize, dir: Dir) -> bool {
        let half = if dir == Dir::In { 16 } else { 0 };
        self.regs.r32(EPCTRL0 + 4 * ep) & EP_STALL << half != 0
    }

    /// Cancel primed transfers (ChipIdea: repeat while still active).
    pub fn flush(&self, bits: u32) {
        let end = time::now_ns() + 200_000_000;
        loop {
            self.regs.w32(FLUSH, bits);
            let _ = self.regs.wait(FLUSH, bits, 0, 100_000);
            if self.regs.r32(STATUS) & bits == 0 || time::now_ns() > end {
                break;
            }
        }
    }

    /// DMA buffer at `off` (CPU view).
    pub fn buf(&self, off: usize, len: usize) -> &mut [u8] {
        &mut self.dma.as_mut_slice()[off..off + len]
    }

    /// Start one transfer of `len` bytes at DMA offset `off`, interrupt on
    /// completion; `zlp`: a zero-length packet follows (control IN).
    pub fn start(&self, ep: usize, dir: Dir, off: usize, len: usize, zlp: bool) {
        let td = td_off(ep, dir);
        let next = if zlp {
            self.td_write(TD_ZLP, TD_TERMINATE, 0, 0);
            self.dma_pa + (TD_ZLP * TD_SIZE) as u32
        } else {
            TD_TERMINATE
        };
        self.td_write(td / TD_SIZE, next, off, len);
        let q = qh_off(ep, dir);
        self.qh.w32(q + QH_NEXT, self.dma_pa + td as u32);
        self.qh.w32(q + QH_TOKEN, 0);
        dma_wmb();
        self.regs.w32(PRIME, bit(ep, dir));
    }

    /// Outcome of the last transfer: `None` while it runs, else the bytes
    /// it did not move, or its token on error.
    pub fn finished(&self, ep: usize, dir: Dir) -> Option<Result<usize, u32>> {
        dma_rmb();
        let tok = self.td_token(td_off(ep, dir));
        if tok & TOK_ACTIVE != 0 {
            None
        } else if tok & TOK_ERRORS != 0 {
            Some(Err(tok))
        } else {
            Some(Ok((tok >> 16 & 0x7FFF) as usize))
        }
    }

    fn td_write(&self, slot: usize, next: u32, off: usize, len: usize) {
        let pa = self.dma_pa + off as u32;
        let base = slot * TD_SIZE;
        let words = [
            next,
            (len as u32) << 16 | TOK_IOC | TOK_ACTIVE,
            pa,
            (pa & !0xFFF) + 0x1000,
            (pa & !0xFFF) + 0x2000,
            (pa & !0xFFF) + 0x3000,
            (pa & !0xFFF) + 0x4000,
            0,
        ];
        let p = unsafe { self.dma.as_ptr().add(base) } as *mut u32;
        for (i, w) in words.into_iter().enumerate() {
            unsafe { p.add(i).write_volatile(w) };
        }
    }

    fn td_token(&self, off: usize) -> u32 {
        unsafe { (self.dma.as_ptr().add(off + 4) as *const u32).read_volatile() }
    }
}

const fn qh_off(ep: usize, dir: Dir) -> usize {
    (ep * 2 + dir as usize) * QH_SIZE
}

const fn td_off(ep: usize, dir: Dir) -> usize {
    (ep * 2 + dir as usize) * TD_SIZE
}

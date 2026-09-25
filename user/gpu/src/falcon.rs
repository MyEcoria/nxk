//! Falcon microcontrollers (nouveau nvkm/falcon/v1.c, gm200.c): the PMU
//! runs NVIDIA's secure loader, FECS and GPCCS switch graphics contexts.

use crate::hw::{Regs, Result};

pub const PMU: u32 = 0x10A000;
pub const FECS: u32 = 0x409000;
pub const GPCCS: u32 = 0x41A000;

const MAILBOX0: u32 = 0x040;
const MAILBOX1: u32 = 0x044;
const CPUCTL: u32 = 0x100;
const BOOTVEC: u32 = 0x104;
const HWCFG: u32 = 0x108;
const DMACTL: u32 = 0x10C;
/// Writable in place of CPUCTL once a secure loader made the falcon
/// light-secure (CPUCTL_ALIAS_EN): the only way to start it then.
const CPUCTL_ALIAS: u32 = 0x130;
const IMEMC: u32 = 0x180;
const IMEMD: u32 = 0x184;
const IMEMT: u32 = 0x188;
const DMEMC: u32 = 0x1C0;
const DMEMD: u32 = 0x1C4;
const CPUCTL_STARTCPU: u32 = 1 << 1;
const CPUCTL_HALTED: u32 = 1 << 4;
const CPUCTL_ALIAS_EN: u32 = 1 << 6;
/// Auto-increment on write; secure IMEM pages.
const MEMC_AINCW: u32 = 1 << 24;
const IMEMC_SECURE: u32 = 1 << 28;

#[derive(Clone, Copy)]
pub struct Falcon {
    r: Regs,
    base: u32,
}

impl Falcon {
    pub fn new(r: Regs, base: u32) -> Self {
        Self { r, base }
    }

    pub fn rd(&self, reg: u32) -> u32 {
        self.r.rd(self.base + reg)
    }

    pub fn wr(&self, reg: u32, v: u32) {
        self.r.wr(self.base + reg, v)
    }

    pub fn mask(&self, reg: u32, clear: u32, set: u32) {
        self.r.mask(self.base + reg, clear, set);
    }

    /// IMEM and DMEM sizes in bytes.
    pub fn sizes(&self) -> (u32, u32) {
        let v = self.rd(HWCFG);
        ((v & 0x1FF) << 8, (v & 0x3_FE00) >> 1)
    }

    /// IMEM and DMEM scrubbed after a reset.
    pub fn wait_scrub(&self, what: &'static str) -> Result<()> {
        self.r.wait(self.base + DMACTL, 6, 0, 10_000, what)
    }

    /// Code at IMEM `addr`, tagged from `tag` per 256-byte block, padded
    /// to a whole block (`nvkm_falcon_v1_load_imem`, port 0).
    pub fn imem(&self, addr: u32, code: &[u8], tag: u32, secure: bool) {
        self.wr(
            IMEMC,
            addr | MEMC_AINCW | if secure { IMEMC_SECURE } else { 0 },
        );
        let words = code.len().div_ceil(4).next_multiple_of(64);
        for i in 0..words {
            if i % 64 == 0 {
                self.wr(IMEMT, tag + (i / 64) as u32);
            }
            self.wr(IMEMD, word(code, i));
        }
    }

    /// Data at DMEM `addr` (`nvkm_falcon_v1_load_dmem`).
    pub fn dmem(&self, addr: u32, data: &[u8]) {
        self.wr(DMEMC, addr | MEMC_AINCW);
        for i in 0..data.len().div_ceil(4) {
            self.wr(DMEMD, word(data, i));
        }
    }

    pub fn set_bootvec(&self, addr: u32) {
        self.wr(BOOTVEC, addr);
    }

    /// Start at BOOTVEC, through the alias if a secure loader set it up
    /// (`nvkm_falcon_v1_start`).
    pub fn start(&self) {
        if self.rd(CPUCTL) & CPUCTL_ALIAS_EN != 0 {
            self.wr(CPUCTL_ALIAS, CPUCTL_STARTCPU);
        } else {
            self.wr(CPUCTL, CPUCTL_STARTCPU);
        }
    }

    pub fn wait_halt(&self, us: u64, what: &'static str) -> Result<()> {
        self.r
            .wait(self.base + CPUCTL, CPUCTL_HALTED, CPUCTL_HALTED, us, what)
    }

    pub fn mailbox(&self) -> (u32, u32) {
        (self.rd(MAILBOX0), self.rd(MAILBOX1))
    }

    pub fn set_mailbox0(&self, v: u32) {
        self.wr(MAILBOX0, v);
    }

    pub fn cpuctl(&self) -> u32 {
        self.rd(CPUCTL)
    }
}

/// Word `i` of `b`, zero-padded past its end.
fn word(b: &[u8], i: usize) -> u32 {
    let mut w = [0u8; 4];
    for (k, x) in w.iter_mut().enumerate() {
        *x = b.get(4 * i + k).copied().unwrap_or(0);
    }
    u32::from_le_bytes(w)
}

//! DRAM training and the boot rate, as hekate does them before its L4T
//! launch (bdk/mem/minerva.c `minerva_init` and `minerva_prep_boot_l4t`),
//! with nxk-emc's port of Minerva (checked against hekate's C, table words
//! included). The base table for the fused DRAM id comes from the card.
//! Erista only: on a Mariko hekate's table-less boost path is not ported.

use nxk_emc::{layout as L, Bank, Hw, Mtc, Op, TABLE_WORDS};

use crate::hdr::*;
use crate::reg::{rd, wr};
use crate::timer::usleep;

/// The table being trained (hekate keeps it in Nyx's storage): room for
/// ten entries, as `_minerva_get_table` clears.
pub const BUF: u32 = 0x8F10_0000;
pub const BUF_BYTES: u32 = 10 * 4928;
/// Where nxk's `emc` service reads the trained table (hekate's
/// MTCTABLE_BASE, in the secure firmware carveout).
pub const MTCTABLE_BASE: u32 = 0xFFE8_0000;
const MTCTABLE_BYTES: u32 = 256 << 10;

const FREQ_204: u32 = 204_000;
const FREQ_408: u32 = 408_000;
const FREQ_800: u32 = 800_000;
const FREQ_1333: u32 = 1_331_200;
const FREQ_1600: u32 = 1_600_000;
/// `minerva_sdmmc_la_program`: SDMMC1's entry among the LA scale registers.
const LA_SDMMC1_INDEX: usize = 6;

/// The base table file for a fused DRAM id (`_minerva_get_table`).
pub fn table_file(dram_id: u32) -> &'static str {
    match dram_id {
        1 => "nxk/mtc_2.bin", // Hynix 4 GB
        7 => "nxk/mtc_7.bin", // Samsung 8 GB
        _ => "nxk/mtc_0.bin",
    }
}

/// The EMC sequence's bus: CAR, MC and EMC (its channel windows at
/// +0x3000 and +0x4000), and hekate Minerva's polled delay.
struct Bus;

impl Hw for Bus {
    fn rd(&mut self, bank: Bank, off: u32) -> u32 {
        rd(base(bank) + off)
    }
    fn wr(&mut self, bank: Bank, off: u32, val: u32) {
        wr(base(bank) + off, val)
    }
    fn usleep(&mut self, us: u32) {
        usleep(us);
    }
}

fn base(bank: Bank) -> u32 {
    match bank {
        Bank::Car => CLOCK_BASE,
        Bank::Mc => MC_BASE,
        Bank::Emc => EMC_BASE,
    }
}

fn words(entries: usize) -> &'static mut [u32] {
    unsafe { core::slice::from_raw_parts_mut(BUF as *mut u32, entries * TABLE_WORDS) }
}

/// Why training stopped.
pub type Error = nxk_emc::Error;

/// `minerva_init` (T210): 204, 800 and 1600 MHz trained, the FSP
/// workaround through 800, then 1600 MHz. `entries`: the table loaded at
/// `BUF` (seven ODINX02 entries).
pub fn init(entries: usize) -> Result<Mtc<'static>, Error> {
    let mut mtc = Mtc::new(words(entries), entries, FREQ_204);
    let h = &mut Bus;
    mtc.run(h, Op::Train, FREQ_204)?;
    mtc.run(h, Op::Train, FREQ_800)?;
    mtc.run(h, Op::Train, FREQ_1600)?;
    mtc.run(h, Op::Switch, FREQ_800)?;
    mtc.run(h, Op::Switch, FREQ_1600)?;
    Ok(mtc)
}

/// `minerva_change_freq`: a switch, unless already there.
fn change_freq(mtc: &mut Mtc, khz: u32) -> Result<(), Error> {
    if mtc.rate_from != khz {
        mtc.run(&mut Bus, Op::Switch, khz)?;
    }
    Ok(())
}

/// `minerva_prep_boot_l4t(0, 0, true)`: SDMMC1's latency allowance set in
/// every entry, the table trimmed to 204, 408, 800 and 1333+ MHz, the
/// rest trained, then 800 MHz (through 1333 MHz when FSP OP/WR is off).
/// Returns the entries left.
pub fn prep_boot(mtc: &mut Mtc) -> Result<usize, Error> {
    let w = TABLE_WORDS;
    for i in 0..mtc.entries {
        let e = &mut mtc.words[i * w..(i + 1) * w];
        let la = L::LA_SCALE_REGS + LA_SDMMC1_INDEX;
        let keep = e[la] & 0xFF_0000;
        e[la] = keep
            | match e[L::RATE_KHZ] {
                FREQ_204 => 50,
                FREQ_408 => 25,
                _ => 20,
            };
    }
    let mut n = 0;
    for i in 0..mtc.entries {
        let rate = mtc.words[i * w + L::RATE_KHZ];
        if rate == FREQ_204 || rate == FREQ_408 || rate == FREQ_800 || rate >= FREQ_1333 {
            mtc.words.copy_within(i * w..(i + 1) * w, n * w);
            n += 1;
        }
    }
    mtc.entries = n;
    change_freq(mtc, FREQ_204)?;
    for i in 0..n {
        if mtc.words[i * w + L::TRAINED] == 0 {
            let rate = mtc.words[i * w + L::RATE_KHZ];
            mtc.run(&mut Bus, Op::Train, rate)?;
        }
    }
    let fsp_opwr_disabled = rd(EMC_BASE + EMC_MRW3) & 0xC0 == 0;
    if fsp_opwr_disabled {
        change_freq(mtc, FREQ_1333)?;
    }
    change_freq(mtc, FREQ_800)?;
    Ok(n)
}

/// The trained entries to `MTCTABLE_BASE`, the rest of the place cleared
/// (hekate's L4T launch copies the entries alone).
pub fn publish(entries: usize) {
    let bytes = (entries * TABLE_WORDS * 4) as u32;
    unsafe {
        core::ptr::copy_nonoverlapping(BUF as *const u8, MTCTABLE_BASE as *mut u8, bytes as usize);
        core::ptr::write_bytes((MTCTABLE_BASE + bytes) as *mut u8, 0, (MTCTABLE_BYTES - bytes) as usize);
    }
}

/// No table: the place cleared, so `emc` finds none.
pub fn clear() {
    unsafe { core::ptr::write_bytes(MTCTABLE_BASE as *mut u8, 0, MTCTABLE_BYTES as usize) };
}

/// The EMC rate now (MHz), for a log line.
pub fn rate_mhz(mtc: &Mtc) -> u32 {
    mtc.rate_from / 1000
}

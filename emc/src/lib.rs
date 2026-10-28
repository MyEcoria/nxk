//! EMC (LPDDR4) frequency switch for the Tegra X1, from the tables hekate's
//! Minerva trained.
//!
//! A port of the switch path (`OP_SWITCH`, no training, no periodic
//! compensation) of Minerva Training Cell (hekate `modules/
//! hekate_libsys_minerva/sys_sdrammtc.c`, (c) 2018-2025 CTCaer, GPL-2,
//! itself derived from NVIDIA's DVFS sequence, Linux `tegra210-emc-cc-
//! r21021.c`). The sequence is generic over `Hw`, so the PC tests run it
//! against a recording bus and compare the register trace with the C code's.
//!
//! The tables are `emc_table_t` revision 7 (4928 bytes, 1232 words): hekate
//! copies them to DRAM at 0xFFE80000 before the L4T launch.

#![no_std]

pub mod layout;
pub mod regs;
mod seq;
mod train;
pub mod warm;

pub use seq::{check_lpddr4, current_rate_khz, mc_mismatches, switch, switch_entries};

/// Size of one table entry in words.
pub const TABLE_WORDS: usize = layout::TABLE_WORDS;
/// Table revision the sequence was written for.
pub const TABLE_REV: u32 = 7;

/// Register blocks the sequence touches. EMC offsets include the
/// per-channel windows (EMC0 at +0x3000, EMC1 at +0x4000 from the EMC base).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bank {
    /// Clock and reset controller, 0x6000_6000.
    Car,
    /// Memory controller, 0x7001_9000.
    Mc,
    /// External memory controller, 0x7001_B000 (0x5000 bytes).
    Emc,
}

/// Register access and microsecond delays.
pub trait Hw {
    fn rd(&mut self, bank: Bank, off: u32) -> u32;
    fn wr(&mut self, bank: Bank, off: u32, val: u32);
    /// Busy-wait: no sleeping, no scheduling.
    fn usleep(&mut self, us: u32);
}

/// Why a switch was not made (or not finished).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A table entry is not a revision-7 table of the sizes this code knows.
    BadTable,
    /// No entry for this rate, or the current rate is not in the tables.
    NoRate,
    /// Already running at this rate (Minerva: "will hang").
    SameRate,
    /// The destination needs periodic training: not ported.
    Periodic,
    /// The DRAM is not LPDDR4.
    NotLpddr4,
    /// PLLM post divider out of range (Minerva hangs here).
    PostDiv,
    /// A hardware wait ran out (the step's name). Before the clock change
    /// the DRAM still works; after it, the state is unknown.
    Timeout(&'static str),
}

/// One validated table entry.
#[derive(Clone, Copy)]
pub struct Table<'a>(pub(crate) &'a [u32]);

impl<'a> Table<'a> {
    /// Check size, revision and the counts the sequence indexes with.
    pub fn new(words: &'a [u32]) -> Result<Self, Error> {
        use layout as L;
        if words.len() != TABLE_WORDS || words[L::REV] != TABLE_REV {
            return Err(Error::BadTable);
        }
        let counts = [
            (L::NUM_BURST, regs::BURST_REGS_EMC_ADDR_TABLE.len()),
            (
                L::NUM_BURST_PER_CH,
                regs::BURST_REG_PER_CH_EMC01_ADDR_TABLE.len(),
            ),
            (L::NUM_TRIM, regs::TRIM_REGS_EMC_ADDR_TABLE.len()),
            (
                L::NUM_TRIM_PER_CH,
                regs::TRIM_PERCH_REGS_EMC01_ADDR_TABLE.len(),
            ),
            (L::NUM_MC_REGS, regs::BURST_MC_REGS_ADDR_TABLE.len()),
            (L::NUM_UP_DOWN, regs::LA_SCALE_REGS_MC_ADDR_TABLE.len()),
            (L::VREF_NUM, regs::VREF_PERCH_REGS_EMC01_ADDR_TABLE.len()),
        ];
        if counts.iter().any(|&(i, n)| words[i] as usize != n) {
            return Err(Error::BadTable);
        }
        let rate = words[L::RATE_KHZ];
        if rate == 0 || rate > 4_000_000 {
            return Err(Error::BadTable);
        }
        Ok(Self(words))
    }

    pub fn rate_khz(&self) -> u32 {
        self.0[layout::RATE_KHZ]
    }

    pub fn clk_src_emc(&self) -> u32 {
        self.0[layout::CLK_SRC_EMC]
    }

    pub fn periodic(&self) -> bool {
        self.0[layout::PERIODIC_TRAINING] != 0
    }
}

/// Word access to a table entry, read-only or not.
pub trait Words {
    fn w(&self, i: usize) -> u32;
}

impl Words for Table<'_> {
    fn w(&self, i: usize) -> u32 {
        self.0[i]
    }
}

/// A table entry the sequence may write: training results, clock tree
/// delays (Minerva writes them into the destination entry).
pub struct TableMut<'a>(&'a mut [u32]);

impl<'a> TableMut<'a> {
    pub fn new(words: &'a mut [u32]) -> Result<Self, Error> {
        Table::new(words)?;
        Ok(Self(words))
    }

    pub fn view(&self) -> Table<'_> {
        Table(self.0)
    }

    pub fn rate_khz(&self) -> u32 {
        self.0[layout::RATE_KHZ]
    }

    pub fn set(&mut self, i: usize, v: u32) {
        self.0[i] = v;
    }
}

impl Words for TableMut<'_> {
    fn w(&self, i: usize) -> u32 {
        self.0[i]
    }
}

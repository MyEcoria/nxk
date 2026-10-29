//! What nxk-ipl did, left in IRAM for nxk-bl31 to print: UART-A's pads are
//! internal on a console, so nxk-ipl's own log is not readable there.
//! Words: "NIPL", DRAM id, trained entries, training ms, `F_*` flags, EMC
//! MHz at the launch (0: boot rate), battery mV. nxk-bl31 reads the same.

pub const BASE: u32 = 0x4003_FF40;
pub const MAGIC: u32 = 0x4C50_494E; // "NIPL"
pub const DRAM_ID: usize = 1;
pub const ENTRIES: usize = 2;
pub const TRAIN_MS: usize = 3;
pub const FLAGS: usize = 4;
pub const EMC_MHZ: usize = 5;
pub const BATTERY_MV: usize = 6;
const WORDS: usize = 7;

pub const F_WATCHDOG: u32 = 1;
pub const F_NO_TABLE: u32 = 2;
pub const F_TRAIN_FAILED: u32 = 4;
pub const F_PREP_FAILED: u32 = 8;

fn word(i: usize) -> *mut u32 {
    (BASE as usize + i * 4) as *mut u32
}

/// A fresh record.
pub fn start() {
    for i in 0..WORDS {
        unsafe { word(i).write_volatile(0) };
    }
    unsafe { word(0).write_volatile(MAGIC) };
}

pub fn set(i: usize, v: u32) {
    unsafe { word(i).write_volatile(v) };
}

pub fn flag(f: u32) {
    unsafe { word(FLAGS).write_volatile(word(FLAGS).read_volatile() | f) };
}

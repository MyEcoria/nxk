//! emc: the DRAM clock. Switches the EMC between the rates hekate's
//! Minerva trained, using its tables (copied to DRAM at 0xFFE80000 before
//! the L4T launch) and a port of Minerva's switch sequence (`nxk-emc`,
//! register-trace-tested against hekate's C code on the PC).
//!
//! Nothing is written to the hardware until a SET, or a SLEEP once the
//! sleep mode is on. The DRAM voltage is never touched. During a switch the
//! service runs at the highest user priority, its code and data resident
//! (a dry run on a register file that touches nothing).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err::{EEXIST, EINVAL, EIO, ENODEV, ENOENT, EOPNOTSUPP};
use libnx::abi::proto::emc as proto;
use libnx::abi::thread;
use libnx::drv::{self, Mmio};
use libnx::ipc::Msg;
use libnx::{println, sys, time};
use nxk_emc::{warm::Warm, Bank, Error, Hw, Table, TABLE_WORDS};

const CAR: u64 = 0x6000_6000;
const MC: u64 = 0x7001_9000;
const EMC: u64 = 0x7001_B000;
const EMC_LEN: u64 = 0x5000;
/// Bytes of one table entry (revision 7).
const ENTRY_BYTES: usize = TABLE_WORDS * 4;
/// Priority of the service (init's table) and during a switch.
const SERVICE_PRIO: u64 = 6;
const SWITCH_PRIO: u64 = 1;
/// Differing MC registers tolerated (a few bits move on their own).
const MC_SLACK: u32 = 3;

/// The registers, mapped; delays spin on the arch counter.
struct Bus {
    car: Mmio,
    mc: Mmio,
    emc: Mmio,
}

impl Hw for Bus {
    fn rd(&mut self, bank: Bank, off: u32) -> u32 {
        match bank {
            Bank::Car => self.car.r32(off as usize),
            Bank::Mc => self.mc.r32(off as usize),
            Bank::Emc => self.emc.r32(off as usize),
        }
    }

    fn wr(&mut self, bank: Bank, off: u32, val: u32) {
        match bank {
            Bank::Car => self.car.w32(off as usize, val),
            Bank::Mc => self.mc.w32(off as usize, val),
            Bank::Emc => self.emc.w32(off as usize, val),
        }
    }

    fn usleep(&mut self, us: u32) {
        let end = time::ticks() + (us as u64 + 1) * time::freq() / 1_000_000 + 1;
        while time::ticks() < end {
            core::hint::spin_loop();
        }
    }
}

/// The validated entries.
fn tables_of(words: &[[u32; TABLE_WORDS]]) -> Vec<Table<'_>> {
    words.iter().filter_map(|w| Table::new(w).ok()).collect()
}

struct Emc {
    bus: Bus,
    /// Entries found (`Table::new` validated at load), words copied out of
    /// the carveout so the switch never reads it.
    words: Vec<[u32; TABLE_WORDS]>,
    switches: u64,
    last_err: i64,
    last_us: u64,
    sleep_mode: bool,
    /// MC registers that differ from the running entry's table (`u32::MAX`:
    /// no entry for the current rate): a SET needs at most `MC_SLACK`.
    mc_diff: u32,
    /// Rate to go back to on wake (0: awake).
    restore: u64,
}

fn errno(e: Error) -> i64 {
    match e {
        Error::SameRate => EEXIST,
        Error::NoRate => ENOENT,
        Error::Periodic => EOPNOTSUPP,
        Error::BadTable => EINVAL,
        Error::Timeout(_) => EIO,
        Error::NotLpddr4 | Error::PostDiv => ENODEV,
    }
}

impl Emc {
    fn rate(&mut self) -> u32 {
        nxk_emc::current_rate_khz(&mut self.bus).unwrap_or(0)
    }

    fn status(&mut self) -> Msg {
        let rate = self.rate();
        let ready = rate != 0 && tables_of(&self.words).iter().any(|t| t.rate_khz() == rate);
        let mut st = 0;
        if ready {
            st |= proto::ST_READY;
        }
        if self.sleep_mode {
            st |= proto::ST_SLEEP_MODE;
        }
        if self.mc_diff <= MC_SLACK {
            st |= proto::ST_MC_OK;
        }
        Msg::from_words(
            0,
            &[
                rate as u64,
                self.words.len() as u64 | st << 8,
                self.last_err as u64,
                self.switches,
                self.last_us,
                self.restore,
            ],
        )
    }

    /// Compare the MC registers with the entry for the current rate and say
    /// what differs: the table must describe the DRAM that is running.
    fn check_mc(&mut self, what: &str) {
        let rate = self.rate();
        let tables = tables_of(&self.words);
        let Some(t) = tables.iter().find(|t| t.rate_khz() == rate) else {
            self.mc_diff = u32::MAX;
            println!("[emc] {}: no table entry for {} kHz", what, rate);
            return;
        };
        let mut out = [(0, 0, 0); 33];
        let n = nxk_emc::mc_mismatches(&mut self.bus, t, &mut out);
        self.mc_diff = n as u32;
        println!(
            "[emc] {}: MC matches the {} MHz entry in {}/33 registers",
            what,
            rate / 1000,
            33 - n
        );
        for &(o, have, want) in &out[..n.min(8)] {
            println!("[emc]   MC {:#x}: {:#x}, table {:#x}", o, have, want);
        }
    }

    /// The switch: prefault on the dry register file, then the real one at
    /// top priority, nothing else of ours running.
    fn switch(&mut self, to_khz: u32) -> Result<(), i64> {
        let tables = tables_of(&self.words);
        if tables.is_empty() || self.mc_diff > MC_SLACK {
            return Err(ENODEV);
        }
        let from = nxk_emc::current_rate_khz(&mut self.bus).ok_or(ENOENT)?;
        let (Some(s), Some(d)) = (
            tables.iter().find(|t| t.rate_khz() == from),
            tables.iter().find(|t| t.rate_khz() == to_khz),
        ) else {
            return Err(ENOENT);
        };
        nxk_emc::check_lpddr4(&mut self.bus).map_err(errno)?;
        nxk_emc::switch_entries(&mut Warm::default(), s, d).map_err(errno)?;
        let _ = sys::thread_set(0, thread::SET_PRIO, SWITCH_PRIO);
        let t0 = time::ticks();
        let r = nxk_emc::switch(&mut self.bus, &tables, to_khz);
        let us = time::ticks_to_ns(time::ticks() - t0) / 1000;
        let _ = sys::thread_set(0, thread::SET_PRIO, SERVICE_PRIO);
        self.last_us = us;
        match r {
            Ok(_) => {
                self.switches += 1;
                println!(
                    "[emc] {} -> {} MHz in {} us",
                    from / 1000,
                    to_khz / 1000,
                    us
                );
                drop(tables);
                self.check_mc("after the switch");
                Ok(())
            }
            Err(e) => {
                println!(
                    "[emc] {} -> {} MHz failed: {:?}",
                    from / 1000,
                    to_khz / 1000,
                    e
                );
                Err(errno(e))
            }
        }
    }

    fn set(&mut self, to_khz: u64) -> Msg {
        match self.switch(to_khz as u32) {
            Ok(()) => {
                self.last_err = 0;
            }
            Err(e) => {
                self.last_err = e;
                return Msg::error(-e);
            }
        }
        self.status()
    }

    /// Asleep: drop to the sleep rate, if the mode is on. Awake: back.
    fn sleep(&mut self, asleep: bool) -> Msg {
        if asleep && self.sleep_mode && self.restore == 0 {
            let cur = self.rate() as u64;
            if cur > proto::SLEEP_KHZ && self.switch(proto::SLEEP_KHZ as u32).is_ok() {
                self.restore = cur;
            }
        } else if !asleep && self.restore != 0 {
            let back = core::mem::take(&mut self.restore);
            match self.switch(back as u32) {
                Ok(()) | Err(EEXIST) => {}
                Err(e) => self.last_err = e,
            }
        }
        self.status()
    }

    fn entry(&self, i: u64) -> Msg {
        match self.words.get(i as usize) {
            Some(w) => {
                use nxk_emc::layout as L;
                Msg::from_words(
                    0,
                    &[
                        w[L::RATE_KHZ] as u64,
                        w[L::CLK_SRC_EMC] as u64,
                        w[L::PERIODIC_TRAINING] as u64,
                        w[L::TRAINED] as u64,
                    ],
                )
            }
            None => Msg::error(-ENOENT),
        }
    }
}

/// Entries of hekate's table: revision 7, the counts the sequence indexes
/// by, back to back from the start of the window.
fn load_tables(win: &Mmio) -> Vec<[u32; TABLE_WORDS]> {
    let mut out = Vec::new();
    for i in 0..(proto::TABLE_LEN as usize / ENTRY_BYTES) {
        let mut w = [0u32; TABLE_WORDS];
        for (k, v) in w.iter_mut().enumerate() {
            *v = win.r32(i * ENTRY_BYTES + k * 4);
        }
        if Table::new(&w).is_err() {
            break;
        }
        out.push(w);
    }
    out
}

libnx::entry!(main);

fn main() -> i32 {
    let car = Mmio::map(drv::dev_slot(0), CAR, 0x1000).expect("emc: CAR");
    let mc = Mmio::map(drv::dev_slot(1), MC, 0x1000).expect("emc: MC");
    let emc = Mmio::map(drv::dev_slot(2), EMC, EMC_LEN).expect("emc: EMC");
    let words = match Mmio::map(drv::dev_slot(3), proto::TABLE_BASE, proto::TABLE_LEN) {
        Ok(win) => load_tables(&win),
        Err(e) => {
            println!("[emc] no table window: {}", e);
            Vec::new()
        }
    };
    let mut svc = Emc {
        bus: Bus { car, mc, emc },
        words,
        switches: 0,
        last_err: 0,
        last_us: 0,
        sleep_mode: false,
        mc_diff: u32::MAX,
        restore: 0,
    };
    svc.check_mc("at start");
    let rate = svc.rate();
    let rates: Vec<u32> = tables_of(&svc.words)
        .iter()
        .map(|t| t.rate_khz() / 1000)
        .collect();
    println!("[emc] {} MHz now, tables for {:?} MHz", rate / 1000, rates);

    let ep = drv::publish("emc").expect("emc: publish");
    drv::serve(ep, |rx| {
        Some(match rx.msg.label() {
            proto::STATUS => svc.status(),
            proto::SET => svc.set(rx.msg.mr[0]),
            proto::ENTRY => svc.entry(rx.msg.mr[0]),
            proto::SLEEP => svc.sleep(rx.msg.mr[0] != 0),
            proto::SLEEP_MODE => {
                svc.sleep_mode = rx.msg.mr[0] != 0;
                svc.status()
            }
            _ => Msg::error(-EINVAL),
        })
    })
}

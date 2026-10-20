//! Register-trace tests of the switch sequence.
//!
//! The sequence runs against a modelled register file that records every
//! access. The traces are compared with golden files and, when
//! `NXK_EMC_ORACLE` names the oracle binary (`emc/oracle/build.sh`: hekate's
//! C code on the same model), with the C code's trace for many random
//! tables. `NXK_EMC_BLESS=1` rewrites the golden files from the oracle.

use nxk_emc::{layout as L, regs, switch_entries, Bank, Error, Hw, Table};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

fn mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846ca68b);
    x ^ (x >> 16)
}

fn bank_id(b: Bank) -> u32 {
    match b {
        Bank::Car => 0,
        Bank::Mc => 1,
        Bank::Emc => 2,
    }
}

fn bank_ch(b: Bank) -> char {
    ['C', 'M', 'E'][bank_id(b) as usize]
}

/// The oracle's bus: written values stick, other registers read a hash of
/// the seed; the status bits the sequence waits for are forced.
struct Model {
    mem: HashMap<(u32, u32), u32>,
    seed: u32,
    trace: String,
}

impl Model {
    fn new(seed: u32, init: &[(Bank, u32, u32)]) -> Self {
        let mut mem = HashMap::new();
        for &(b, o, v) in init {
            mem.insert((bank_id(b), o), v);
        }
        Self {
            mem,
            seed,
            trace: String::new(),
        }
    }

    fn alias(bank: Bank, off: u32) -> u32 {
        if bank == Bank::Emc && (0x4000..0x5000).contains(&off) {
            let lo = off & 0xFFF;
            if lo == 0x2BC || lo == 0x2B4 || lo == 0x2C4 {
                return lo;
            }
        }
        off
    }
}

impl Hw for Model {
    fn rd(&mut self, bank: Bank, off: u32) -> u32 {
        let a = Self::alias(bank, off);
        let b = bank_id(bank);
        let mut v = match self.mem.get(&(b, a)) {
            Some(&v) => v,
            None => mix(self.seed
                ^ mix(b.wrapping_mul(0x9E3779B1))
                ^ mix(a.wrapping_mul(0x85EBCA6B).wrapping_add(1))),
        };
        match (bank, a) {
            (Bank::Emc, 0x0) => v |= 0x10,
            (Bank::Emc, 0x2B4) => v = 0,
            (Bank::Emc, 0x2C4) => v |= 0x28000,
            (Bank::Car, 0x90) | (Bank::Car, 0x5E8) => v |= 1 << 27,
            _ => {}
        }
        writeln!(self.trace, "R {} {:x} {:x}", bank_ch(bank), off, v).unwrap();
        v
    }

    fn wr(&mut self, bank: Bank, off: u32, val: u32) {
        self.mem.insert((bank_id(bank), off), val);
        writeln!(self.trace, "W {} {:x} {:x}", bank_ch(bank), off, val).unwrap();
    }

    fn usleep(&mut self, us: u32) {
        writeln!(self.trace, "U {}", us).unwrap();
    }
}

struct Rng(u32);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = mix(self.0.wrapping_add(0x9E3779B9));
        self.0
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

const CLK_204: u32 = 0x4018_8002;
const CLK_408: u32 = 0xE018_8000;
const CLK_800: u32 = 0x8018_8000;

/// A plausible entry: random words, the fields the sequence indexes or
/// divides by set to valid values.
fn gen_table(rng: &mut Rng, rate: u32, clk: u32, periodic: u32) -> Vec<u32> {
    let mut t: Vec<u32> = (0..L::TABLE_WORDS).map(|_| rng.next()).collect();
    t[L::REV] = 7;
    t[L::RATE_KHZ] = rate;
    t[L::CLK_SRC_EMC] = clk;
    t[L::PERIODIC_TRAINING] = periodic;
    t[L::NUM_BURST] = 221;
    t[L::NUM_BURST_PER_CH] = 8;
    t[L::NUM_TRIM] = 138;
    t[L::NUM_TRIM_PER_CH] = 10;
    t[L::NUM_MC_REGS] = 33;
    t[L::NUM_UP_DOWN] = 24;
    t[L::VREF_NUM] = 4;
    t[L::DRAM_TIMINGS_T_RP] = 18 + rng.below(8);
    t[L::DRAM_TIMINGS_T_FC_LPDDR4] = 100 + rng.below(100);
    t[L::DRAM_TIMINGS_T_PDEX] = 10 + rng.below(20);
    t[L::DRAM_TIMINGS_RL] = 6 + rng.below(30);
    t[L::BURST_REGS_EMC_RP] = rng.below(64);
    t[L::BURST_REGS_EMC_R2P] = rng.below(64);
    t[L::BURST_REGS_EMC_W2P] = rng.below(64);
    t[L::BURST_REGS_EMC_TRPAB] = rng.below(64);
    // Half of the entries have the DLL on, the bandgap regulator flip and
    // the shared ZQ resistor vary too.
    t[L::BURST_REGS_EMC_CFG_DIG_DLL] = rng.below(2);
    t[L::BURST_REGS_EMC_FBIO_CFG7] =
        (t[L::BURST_REGS_EMC_FBIO_CFG7] & !0b110) | (rng.below(2) << 2);
    t
}

fn words_to_bytes(t: &[u32]) -> Vec<u8> {
    t.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Initial registers: the live clock source and PLLs of `src`, LPDDR4 (or
/// not), the rank count and the FSP state.
fn gen_init(rng: &mut Rng, src_clk: u32, lpddr4: bool) -> Vec<(Bank, u32, u32)> {
    let pllm = 3 | (125 << 8) | (1 << 20) | (1 << 30);
    let fbio5 = (rng.next() & !3) | if lpddr4 { 1 } else { rng.below(4) & !1 };
    vec![
        (Bank::Car, regs::CLK_RST_CONTROLLER_CLK_SOURCE_EMC, src_clk),
        (Bank::Car, regs::CLK_RST_CONTROLLER_PLLM_BASE, pllm),
        (Bank::Car, regs::CLK_RST_CONTROLLER_PLLMB_BASE, pllm),
        (Bank::Emc, regs::EMC_FBIO_CFG5, fbio5),
        (Bank::Emc, regs::EMC_FBIO_CFG7, 0x6 | (rng.next() & !0x6)),
        (Bank::Mc, regs::MC_EMEM_ADR_CFG, rng.next()),
        (
            Bank::Emc,
            regs::EMC_MRW3,
            if rng.below(2) == 0 { 0xC0 } else { 0 } | (rng.next() & !0xC0),
        ),
    ]
}

fn init_text(seed: u32, init: &[(Bank, u32, u32)]) -> String {
    let mut s = format!("seed {}\n", seed);
    for &(b, o, v) in init {
        writeln!(s, "{} {:x} {:x}", bank_ch(b), o, v).unwrap();
    }
    s
}

struct Case {
    name: String,
    src: Vec<u32>,
    dst: Vec<u32>,
    seed: u32,
    init: Vec<(Bank, u32, u32)>,
}

fn make_case(name: &str, n: u32, from: (u32, u32), to: (u32, u32), lpddr4: bool) -> Case {
    let mut rng = Rng(n);
    let src = gen_table(&mut rng, from.0, from.1, 0);
    let dst = gen_table(&mut rng, to.0, to.1, 0);
    let seed = rng.next();
    let init = gen_init(&mut rng, from.1, lpddr4);
    Case {
        name: name.into(),
        src,
        dst,
        seed,
        init,
    }
}

fn rust_trace(c: &Case) -> (String, Result<u32, Error>) {
    let (s, d) = (Table::new(&c.src).unwrap(), Table::new(&c.dst).unwrap());
    let mut m = Model::new(c.seed, &c.init);
    let r = switch_entries(&mut m, &s, &d);
    (m.trace, r)
}

fn oracle_trace(bin: &str, c: &Case) -> String {
    let dir = std::env::temp_dir().join(format!("nxk-emc-{}-{}", std::process::id(), c.name));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("src.bin"), words_to_bytes(&c.src)).unwrap();
    std::fs::write(dir.join("dst.bin"), words_to_bytes(&c.dst)).unwrap();
    std::fs::write(dir.join("regs.txt"), init_text(c.seed, &c.init)).unwrap();
    let out = Command::new(bin)
        .args([
            dir.join("src.bin"),
            dir.join("dst.bin"),
            dir.join("regs.txt"),
        ])
        .output()
        .expect("run oracle");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "oracle failed on {}", c.name);
    String::from_utf8(out.stdout).unwrap()
}

fn first_diff(a: &str, b: &str) -> String {
    for (i, (x, y)) in a.lines().zip(b.lines()).enumerate() {
        if x != y {
            return format!("line {}: rust `{}` vs oracle `{}`", i + 1, x, y);
        }
    }
    format!(
        "lengths differ: {} vs {} lines",
        a.lines().count(),
        b.lines().count()
    )
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.trace"))
}

const RATES: [(u32, u32); 3] = [(204_000, CLK_204), (408_000, CLK_408), (800_000, CLK_800)];

fn switch_cases() -> Vec<Case> {
    let mut v = Vec::new();
    for (i, &from) in RATES.iter().enumerate() {
        for (j, &to) in RATES.iter().enumerate() {
            if i != j {
                v.push(make_case(
                    &format!("{}_{}", from.0 / 1000, to.0 / 1000),
                    100 + i as u32 * 7 + j as u32,
                    from,
                    to,
                    true,
                ));
            }
        }
    }
    v
}

#[test]
fn golden_traces() {
    let oracle = std::env::var("NXK_EMC_ORACLE").ok();
    let bless = std::env::var("NXK_EMC_BLESS").is_ok();
    for c in switch_cases() {
        let (rust, r) = rust_trace(&c);
        assert!(r.is_ok(), "{}: {:?}", c.name, r);
        let path = golden_path(&c.name);
        if bless {
            let o = oracle
                .as_deref()
                .expect("NXK_EMC_BLESS needs NXK_EMC_ORACLE");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, oracle_trace(o, &c)).unwrap();
        }
        let golden =
            std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {}", path.display()));
        assert!(rust == golden, "{}: {}", c.name, first_diff(&rust, &golden));
    }
}

#[test]
fn matches_c_on_random_tables() {
    let Ok(bin) = std::env::var("NXK_EMC_ORACLE") else {
        eprintln!("NXK_EMC_ORACLE not set: C comparison skipped");
        return;
    };
    let mut n = 1000;
    for &from in &RATES {
        for &to in &RATES {
            if from.0 == to.0 {
                continue;
            }
            for k in 0..40 {
                n += 1;
                let c = make_case(&format!("rnd{n}_{k}"), n, from, to, true);
                let (rust, r) = rust_trace(&c);
                assert!(r.is_ok());
                let theirs = oracle_trace(&bin, &c);
                assert!(
                    rust == theirs,
                    "{} {}->{}: {}",
                    c.name,
                    from.0,
                    to.0,
                    first_diff(&rust, &theirs)
                );
            }
        }
    }
}

#[test]
fn pllmb_sources_match_c() {
    let Ok(bin) = std::env::var("NXK_EMC_ORACLE") else {
        return;
    };
    // 800 MHz running from PLLMB (what a second switch leaves), then away and back.
    for (k, (from, to)) in [
        ((800_000, 0xA018_8000), (204_000, CLK_204)),
        ((800_000, 0xA018_8000), (408_000, CLK_408)),
    ]
    .into_iter()
    .enumerate()
    {
        let c = make_case(&format!("pllmb{k}"), 5000 + k as u32, from, to, true);
        let (rust, r) = rust_trace(&c);
        assert!(r.is_ok());
        assert!(
            rust == oracle_trace(&bin, &c),
            "{}",
            first_diff(&rust, &oracle_trace(&bin, &c))
        );
    }
}

#[test]
fn refuses_what_it_cannot_do() {
    let mut rng = Rng(9);
    let a = gen_table(&mut rng, 800_000, CLK_800, 0);
    let b = gen_table(&mut rng, 204_000, CLK_204, 0);
    let periodic = gen_table(&mut rng, 1_600_000, CLK_800, 1);
    let mut m = Model::new(1, &gen_init(&mut Rng(2), CLK_800, true));
    let (ta, tb, tp) = (
        Table::new(&a).unwrap(),
        Table::new(&b).unwrap(),
        Table::new(&periodic).unwrap(),
    );
    assert_eq!(switch_entries(&mut m, &ta, &ta), Err(Error::SameRate));
    assert_eq!(switch_entries(&mut m, &ta, &tp), Err(Error::Periodic));
    assert!(m.trace.is_empty(), "refusals touch no register");
    // Not LPDDR4: nothing is written.
    let mut m = Model::new(1, &gen_init(&mut Rng(3), CLK_800, false));
    assert_eq!(
        nxk_emc::switch(&mut m, &[ta, tb], 204_000),
        Err(Error::NotLpddr4)
    );
    assert!(
        !m.trace.contains('W'),
        "no write before the DRAM type is known"
    );
    // Table checks.
    let mut bad = a.clone();
    bad[L::NUM_BURST] = 220;
    assert_eq!(Table::new(&bad).err(), Some(Error::BadTable));
    assert_eq!(Table::new(&a[..100]).err(), Some(Error::BadTable));
    bad = a.clone();
    bad[L::REV] = 3;
    assert_eq!(Table::new(&bad).err(), Some(Error::BadTable));
}

#[test]
fn current_rate_decodes_the_boot_sources() {
    let init = |clk: u32, pllm: u32| {
        Model::new(
            0,
            &[
                (Bank::Car, regs::CLK_RST_CONTROLLER_CLK_SOURCE_EMC, clk),
                (Bank::Car, regs::CLK_RST_CONTROLLER_PLLM_BASE, pllm),
                (Bank::Car, regs::CLK_RST_CONTROLLER_PLLMB_BASE, pllm),
            ],
        )
    };
    let p800 = 3 | (125 << 8) | (1 << 20);
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(CLK_800, p800)),
        Some(800_000)
    );
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(0xA018_8000, p800)),
        Some(800_000)
    );
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(CLK_204, p800)),
        Some(204_000)
    );
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(CLK_408, p800)),
        Some(408_000)
    );
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(CLK_800, 3 | (125 << 8))),
        Some(1_600_000)
    );
    assert_eq!(
        nxk_emc::current_rate_khz(&mut init(0x2000_0000, p800)),
        None
    );
}

/// hekate's own odinx02 tables (`NXK_EMC_TABLES`: 7 entries of 4928 bytes
/// extracted from mtc_switch_tables.h), every switch among the entries that
/// do not need periodic training, against the C code.
#[test]
fn odinx02_tables_match_c() {
    let (Ok(bin), Ok(path)) = (
        std::env::var("NXK_EMC_ORACLE"),
        std::env::var("NXK_EMC_TABLES"),
    ) else {
        return;
    };
    let raw = std::fs::read(path).unwrap();
    let entries: Vec<Vec<u32>> = raw
        .chunks(4928)
        .map(|c| {
            c.chunks(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect()
        })
        .collect();
    assert_eq!(entries.len(), 7);
    let usable: Vec<&Vec<u32>> = entries
        .iter()
        .filter(|e| e[L::PERIODIC_TRAINING] == 0)
        .collect();
    assert!(usable.len() >= 3);
    let mut done = 0;
    for src in &usable {
        for dst in &usable {
            if src[L::RATE_KHZ] == dst[L::RATE_KHZ] {
                continue;
            }
            for k in 0..3u32 {
                let mut rng = Rng(77 + done);
                let init = gen_init(&mut rng, src[L::CLK_SRC_EMC], true);
                let c = Case {
                    name: format!("odin{done}"),
                    src: (*src).clone(),
                    dst: (*dst).clone(),
                    seed: rng.next() ^ k,
                    init,
                };
                let (rust, r) = rust_trace(&c);
                assert!(r.is_ok());
                let theirs = oracle_trace(&bin, &c);
                assert!(
                    rust == theirs,
                    "{}->{}: {}",
                    src[L::RATE_KHZ],
                    dst[L::RATE_KHZ],
                    first_diff(&rust, &theirs)
                );
                done += 1;
            }
        }
    }
    eprintln!("{done} real-table switches match");
}

#[test]
fn warm_run_completes_and_addresses_fit_the_windows() {
    let mut rng = Rng(11);
    let a = gen_table(&mut rng, 800_000, CLK_800, 0);
    let b = gen_table(&mut rng, 204_000, CLK_204, 0);
    let (ta, tb) = (Table::new(&a).unwrap(), Table::new(&b).unwrap());
    let mut w = nxk_emc::warm::Warm::default();
    assert_eq!(switch_entries(&mut w, &ta, &tb), Ok(204_000));
    // Every address the service will touch is inside what init maps for it:
    // EMC 0x5000 bytes, MC 0x1000.
    let emc = regs::BURST_REGS_EMC_ADDR_TABLE
        .iter()
        .chain(&regs::BURST_REG_PER_CH_EMC01_ADDR_TABLE)
        .chain(&regs::VREF_PERCH_REGS_EMC01_ADDR_TABLE)
        .chain(&regs::TRIM_REGS_EMC_ADDR_TABLE)
        .chain(&regs::TRIM_PERCH_REGS_EMC01_ADDR_TABLE);
    assert!(emc.clone().all(|&a| a < 0x5000 && a % 4 == 0), "EMC window");
    let mc = regs::BURST_MC_REGS_ADDR_TABLE
        .iter()
        .chain(&regs::LA_SCALE_REGS_MC_ADDR_TABLE);
    assert!(mc.clone().all(|&a| a < 0x1000 && a % 4 == 0), "MC window");
}

#[test]
fn mc_check_compares_the_burst_registers() {
    let mut rng = Rng(21);
    let a = gen_table(&mut rng, 800_000, CLK_800, 0);
    let t = Table::new(&a).unwrap();
    let init: Vec<(Bank, u32, u32)> = regs::BURST_MC_REGS_ADDR_TABLE
        .iter()
        .enumerate()
        .map(|(i, &o)| (Bank::Mc, o, a[L::BURST_MC_REGS + i]))
        .collect();
    let mut m = Model::new(1, &init);
    let mut out = [(0, 0, 0); 33];
    assert_eq!(nxk_emc::mc_mismatches(&mut m, &t, &mut out), 0);
    let mut init = init;
    init[5].2 ^= 0x10;
    init[20].2 ^= 1;
    // The hysteresis group (indexes 24..33) is not compared.
    init[26].2 ^= 0x80_0000;
    let mut m = Model::new(1, &init);
    assert_eq!(nxk_emc::mc_mismatches(&mut m, &t, &mut out), 2);
    assert_eq!(out[0].0, regs::BURST_MC_REGS_ADDR_TABLE[5]);
    assert_eq!(out[1].0, regs::BURST_MC_REGS_ADDR_TABLE[20]);
}

//! Battery power measurement. The charger's input is cut (BQ24193 HIZ)
//! so the gauge sees the whole system load; its I2C watchdog brings the
//! input back if this service dies meanwhile.

use libnx::abi::err::{EBUSY, EINVAL};
use libnx::abi::proto::power as proto;
use libnx::i2c::I2c;
use libnx::ipc::Msg;
use libnx::sync::Mutex;
use libnx::thread::{self, JoinHandle};
use libnx::{println, time};

use crate::{cell_mv, gauge, gauge_current, BQ24193, MAX_CURRENT, MAX_REPSOC};

/// BQ24193 registers (hekate bdk/power/bq24193.h): input source control
/// (bit 7 = HIZ), power-on configuration (bit 6 = watchdog reset),
/// charge termination/timer (bits 5:4 = watchdog, 01 = 40 s).
const REG_INPUT: u8 = 0x00;
const HIZ: u8 = 1 << 7;
const REG_CONFIG: u8 = 0x01;
const WD_KICK: u8 = 1 << 6;
const REG_TIMER: u8 = 0x05;
const WD_MASK: u8 = 3 << 4;
const WD_40S: u8 = 1 << 4;
/// Below these the run stops: a deep discharge hurts the cell.
const MIN_MV: u64 = 3500;
const MIN_SOC_PCT: u64 = 10;
const MAX_SECONDS: u64 = 600;
const MAX_SETTLE: u64 = 120;
const RESTORE_TRIES: u32 = 3;

/// Samples so far and the state of the run.
struct Stats {
    state: u64,
    count: u64,
    sum: i64,
    min: i64,
    max: i64,
    mv_sum: u64,
}

impl Stats {
    const EMPTY: Stats = Stats {
        state: proto::MEAS_NONE,
        count: 0,
        sum: 0,
        min: i64::MAX,
        max: i64::MIN,
        mv_sum: 0,
    };

    fn add(&mut self, ua: i64, mv: u64) {
        self.count += 1;
        self.sum += ua;
        self.min = self.min.min(ua);
        self.max = self.max.max(ua);
        self.mv_sum += mv;
    }

    /// Mean current (uA), mean cell voltage (mV), mean power drawn (uW).
    fn means(&self) -> (i64, u64, i64) {
        if self.count == 0 {
            return (0, 0, 0);
        }
        let ua = self.sum / self.count as i64;
        let mv = self.mv_sum / self.count;
        (ua, mv, -ua * mv as i64 / 1000)
    }

    fn reply(&self) -> Msg {
        let (ua, mv, uw) = self.means();
        let (lo, hi) = if self.count == 0 {
            (0, 0)
        } else {
            (self.min as i32, self.max as i32)
        };
        let range = lo as u32 as u64 | (hi as u32 as u64) << 32;
        let w = [self.state, self.count, ua as u64, range, mv, uw as u64];
        Msg::from_words(0, &w)
    }
}

static STATS: Mutex<Stats> = Mutex::new(Stats::EMPTY);
/// The last run's thread, joined at the next start to free its stack.
static WORKER: Mutex<Option<JoinHandle>> = Mutex::new(None);

/// Clear HIZ. Returns whether it was set.
pub fn clear_hiz(i2c: &I2c) -> Result<bool, i64> {
    let v = i2c.read_reg(1, BQ24193, REG_INPUT)?;
    if v & HIZ == 0 {
        return Ok(false);
    }
    i2c.write_reg(1, BQ24193, REG_INPUT, v & !HIZ)?;
    Ok(true)
}

/// Watchdog at 40 s, then the input cut.
fn cut_input(i2c: &I2c, timer: u8) -> Result<(), i64> {
    i2c.write_reg(1, BQ24193, REG_TIMER, timer & !WD_MASK | WD_40S)?;
    let v = i2c.read_reg(1, BQ24193, REG_INPUT)?;
    i2c.write_reg(1, BQ24193, REG_INPUT, v | HIZ)
}

/// Input back and the watchdog as found; a few tries, then the watchdog
/// does it.
fn restore(i2c: &I2c, timer: u8) {
    for _ in 0..RESTORE_TRIES {
        let r = clear_hiz(i2c).and_then(|_| i2c.write_reg(1, BQ24193, REG_TIMER, timer));
        if r.is_ok() {
            return;
        }
    }
    println!("[power] measure: charger input not restored, the watchdog will");
}

fn kick(i2c: &I2c) -> Result<(), i64> {
    let v = i2c.read_reg(1, BQ24193, REG_CONFIG)?;
    i2c.write_reg(1, BQ24193, REG_CONFIG, v | WD_KICK)
}

/// One reading: current (uA), cell voltage (mV), or why to stop.
fn read_sample(i2c: &I2c) -> Result<(i64, u64), &'static str> {
    let soc = gauge(i2c, MAX_REPSOC).map_err(|_| "gauge unreadable")? >> 8;
    let mv = cell_mv(i2c).map_err(|_| "gauge unreadable")?;
    let ua = gauge_current(i2c, MAX_CURRENT).map_err(|_| "gauge unreadable")?;
    if mv < MIN_MV || (soc as u64) < MIN_SOC_PCT {
        return Err("battery low");
    }
    Ok((ua as i64, mv))
}

/// Once a second: kick the watchdog, then sample once settled.
fn sample_loop(i2c: &I2c, settle: u64, secs: u64) -> Result<(), &'static str> {
    for t in 0..settle + secs {
        kick(i2c).map_err(|_| "charger unreadable")?;
        time::sleep_ms(1000);
        let (ua, mv) = read_sample(i2c)?;
        if t >= settle {
            STATS.lock().add(ua, mv);
        }
    }
    Ok(())
}

/// The whole run, on its own thread; the input is always restored.
fn run(i2c: I2c, settle: u64, secs: u64) {
    let result = match i2c.read_reg(1, BQ24193, REG_TIMER) {
        Ok(timer) => {
            let r = cut_input(&i2c, timer)
                .map_err(|_| "charger unreadable")
                .and_then(|()| sample_loop(&i2c, settle, secs));
            restore(&i2c, timer);
            r
        }
        Err(_) => Err("charger unreadable"),
    };
    let mut st = STATS.lock();
    let (ua, mv, uw) = st.means();
    match result {
        Ok(()) => println!(
            "[power] measure {} s: mean {} uA (min {} max {}), {}.{:02} V, {} mW drawn, {} samples",
            secs,
            ua,
            st.min,
            st.max,
            mv / 1000,
            mv % 1000 / 10,
            uw / 1000,
            st.count
        ),
        Err(why) => println!(
            "[power] measure aborted ({}) after {} samples, input restored",
            why, st.count
        ),
    }
    st.state = if result.is_ok() {
        proto::MEAS_DONE
    } else {
        proto::MEAS_ABORTED
    };
}

/// `MEASURE`: start the run in the background.
pub fn start(secs: u64, settle: u64) -> Msg {
    if !(1..=MAX_SECONDS).contains(&secs) || settle > MAX_SETTLE {
        return Msg::error(-EINVAL);
    }
    let i2c = match I2c::connect() {
        Ok(i2c) => i2c,
        Err(e) => return Msg::error(e),
    };
    let mut worker = WORKER.lock();
    {
        let mut st = STATS.lock();
        if st.state == proto::MEAS_RUNNING {
            return Msg::error(-EBUSY);
        }
        *st = Stats::EMPTY;
        st.state = proto::MEAS_RUNNING;
    }
    if let Some(old) = worker.take() {
        old.join();
    }
    match thread::spawn(move || run(i2c, settle, secs)) {
        Ok(h) => {
            *worker = Some(h);
            Msg::new(0, 0, 0)
        }
        Err(e) => {
            STATS.lock().state = proto::MEAS_NONE;
            Msg::error(e)
        }
    }
}

/// `MEASURE_RESULT`.
pub fn result() -> Msg {
    STATS.lock().reply()
}

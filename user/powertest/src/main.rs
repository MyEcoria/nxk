//! powertest: battery power draw through the `power` service.
//! `powertest start SECONDS [SETTLE]`, `powertest result`, `powertest wait`.

#![no_std]
#![no_main]

use libnx::abi::err;
use libnx::abi::proto::power;
use libnx::ipc::{self, Msg};
use libnx::{env, ns, println, sys, time};

libnx::entry!(main);

const USAGE: &str = "usage: powertest start SECONDS [SETTLE] | result | wait";

/// One call to `power`; the reply or an errno.
fn call(label: u32, words: &[u64]) -> Result<Msg, i64> {
    let h = ns::wait_for_timeout("power", 3_000_000_000)?;
    let r = ipc::call_checked(h, &Msg::from_words(label, words));
    let _ = sys::handle_close(h);
    r
}

/// Print a `MEASURE_RESULT` reply on one line.
fn show(m: &Msg) {
    let state = match m.mr[0] {
        power::MEAS_RUNNING => "running",
        power::MEAS_DONE => "done",
        power::MEAS_ABORTED => "aborted",
        _ => "none",
    };
    let (mean, uw) = (m.mr[2] as i64, m.mr[5] as i64);
    let (lo, hi) = (m.mr[3] as u32 as i32, (m.mr[3] >> 32) as u32 as i32);
    println!(
        "{}: {} samples, current mean {} min {} max {} mA, {} mV, {} mW drawn",
        state,
        m.mr[1],
        mean / 1000,
        lo as i64 / 1000,
        hi as i64 / 1000,
        m.mr[4],
        uw / 1000
    );
}

/// `result` and `wait`: `wait` polls once a second while running.
fn result(wait: bool) -> Result<(), i64> {
    loop {
        let m = call(power::MEASURE_RESULT, &[])?;
        if !wait || m.mr[0] != power::MEAS_RUNNING {
            show(&m);
            return Ok(());
        }
        time::sleep_ms(1000);
    }
}

fn number(arg: Option<&str>, default: u64) -> Option<u64> {
    match arg {
        Some(s) => s.parse().ok(),
        None => Some(default),
    }
}

fn run() -> Result<(), i64> {
    let mut args = env::args().skip(1);
    match args.next() {
        Some("start") => {
            let (Some(secs), Some(settle)) = (
                number(args.next(), 0).filter(|&s| s > 0),
                number(args.next(), 0),
            ) else {
                println!("{}", USAGE);
                return Err(-err::EINVAL);
            };
            call(power::MEASURE, &[secs, settle]).map(|_| ())
        }
        Some("result") => result(false),
        Some("wait") => result(true),
        _ => {
            println!("{}", USAGE);
            Err(-err::EINVAL)
        }
    }
}

fn main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            println!("powertest: {}", err::name(e));
            1
        }
    }
}

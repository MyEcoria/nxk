//! Network time: SNTP (RFC 4330) once a network is up, kept as an offset
//! to the PMIC RTC in `/config/rtc_offset`, so the next boot has it before
//! Wi-Fi. The RTC itself is never written: Horizon and hekate keep their
//! own offset to it (on a Switch the RTC is not the real time).

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use libnx::abi::err::{EINVAL, ENOENT, ERANGE};
use libnx::i2c::I2c;
use libnx::{fs, net, println, sys, time};

/// Seconds added to the RTC to get the real time.
pub static OFFSET: AtomicI64 = AtomicI64::new(0);

const FILE: &str = "/config/rtc_offset";
const SERVERS: [&str; 2] = ["pool.ntp.org", "time.google.com"];
/// 1900 (NTP's era 0) to 1970.
const NTP_UNIX: u64 = 2_208_988_800;
/// An answer before 2026-10-01 is refused (a wrong or forged server).
const FLOOR_S: u64 = 1_790_812_800;
const ANSWER_NS: u64 = 3_000_000_000;
/// Network checks while waiting for one, retries after a failure, and the
/// resync period once in time.
const WAIT_MS: u64 = 5_000;
const RETRY_MS: u64 = 60_000;
const RESYNC_MS: u64 = 6 * 3_600_000;

/// The real time from the RTC: its reading plus the offset.
pub fn real_seconds(rtc: i64) -> i64 {
    rtc + OFFSET.load(Ordering::Relaxed)
}

/// The thread: the saved offset once the card is readable, then SNTP
/// whenever a network has an address.
pub fn run(sysctl: u32) {
    let Ok(i2c) = I2c::connect() else {
        println!("[power] network time: no i2c");
        return;
    };
    if let Some(off) = saved() {
        OFFSET.store(off, Ordering::Relaxed);
        if crate::sync_time(&i2c, sysctl).is_ok() {
            println!(
                "[power] clock from the RTC and the saved offset ({} s)",
                off
            );
        }
    }
    loop {
        if !net::info().is_ok_and(|i| i.0 != 0) {
            time::sleep_ms(WAIT_MS);
            continue;
        }
        let ms = match SERVERS.iter().find_map(|s| query(s).ok()) {
            Some(ns) => {
                adopt(&i2c, sysctl, ns);
                RESYNC_MS
            }
            None => RETRY_MS,
        };
        time::sleep_ms(ms);
    }
}

/// The offset saved by an earlier sync (the card may take a while).
fn saved() -> Option<i64> {
    for _ in 0..30 {
        match fs::read(FILE) {
            Ok(t) => return String::from_utf8_lossy(&t).trim().parse().ok(),
            Err(e) if e == -ENOENT && fs::statfs().is_ok() => return None,
            Err(_) => time::sleep_ms(2_000),
        }
    }
    None
}

/// Set the clock to `ns` and keep its offset to the RTC.
fn adopt(i2c: &I2c, sysctl: u32, ns: u64) {
    if sys::clock_set_realtime(sysctl, ns).is_err() {
        return;
    }
    let Ok(rtc) = crate::rtc_seconds(i2c) else {
        return;
    };
    let off = (ns / 1_000_000_000) as i64 - rtc;
    let old = OFFSET.swap(off, Ordering::Relaxed);
    let first = !SYNCED.swap(true, Ordering::Relaxed);
    if first || (off - old).abs() > 2 {
        let _ = fs::mkdir("/config");
        let _ = fs::write(FILE, format!("{}\n", off).as_bytes());
        println!("[power] network time set, RTC offset {} s", off);
    }
}

/// A network time was set since the start.
static SYNCED: AtomicBool = AtomicBool::new(false);

/// One SNTP exchange with `server`: Unix time in ns, half the round trip
/// added. The answer must echo our random transmit time.
fn query(server: &str) -> Result<u64, i64> {
    let addr = net::resolve(server)?;
    let sock = net::UdpSocket::bind(0)?;
    let mut req = [0u8; 48];
    req[0] = 0x23; // no leap warning, version 4, client
    let mut nonce = [0u8; 8];
    sys::getrandom(&mut nonce);
    req[40..48].copy_from_slice(&nonce);
    let t0 = time::now_ns();
    sock.send_to(&req, addr, 123)?;
    let mut buf = [0u8; 68];
    let n = loop {
        let (n, from, port) = sock.recv_from(&mut buf, ANSWER_NS)?;
        if from == addr && port == 123 && n >= 48 {
            break n;
        }
    };
    let rtt = time::now_ns() - t0;
    let ok = n >= 48
        && buf[0] & 0x7 == 4
        && buf[0] >> 6 != 3
        && (1..=15).contains(&buf[1])
        && buf[24..32] == nonce;
    if !ok {
        return Err(-EINVAL);
    }
    let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]) as u64;
    let frac = u32::from_be_bytes([buf[44], buf[45], buf[46], buf[47]]) as u64;
    let unix = secs.checked_sub(NTP_UNIX).ok_or(-ERANGE)?;
    if unix < FLOOR_S {
        return Err(-ERANGE);
    }
    Ok(unix * 1_000_000_000 + ((frac * 1_000_000_000) >> 32) + rtt / 2)
}

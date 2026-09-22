//! wifi: Wi-Fi from the shell (scan, connect, status, disconnect).
//! Client of the `wlan` service, published as "wifi".

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::proto::wifi as proto;
use libnx::ipc::{self, Msg};
use libnx::{env, ns, println};

libnx::entry!(main);

/// Connecting and scanning outlast the default reply wait.
const CALL_TIMEOUT: u64 = 20_000_000_000;

/// The service publishes itself once the adapter is looked at, which
/// takes a moment at boot.
fn service() -> Result<u32, i64> {
    ns::wait_for_timeout("wifi", 5_000_000_000)
}

fn call(m: &Msg) -> Result<Msg, i64> {
    let ep = service()?;
    ipc::checked(ipc::call_timeout(ep, m, CALL_TIMEOUT)?)
}

fn state_name(s: u64) -> &'static str {
    match s {
        proto::ST_IDLE => "idle",
        proto::ST_SCANNING => "scanning",
        proto::ST_CONNECTING => "connecting",
        proto::ST_CONNECTED => "connected",
        _ => "down",
    }
}

fn security_name(s: u8) -> &'static str {
    match s {
        proto::SEC_WEP => "WEP",
        proto::SEC_WPA => "WPA",
        proto::SEC_WPA2 => "WPA2",
        proto::SEC_WPA3 => "WPA3",
        _ => "open",
    }
}

fn scan() -> Result<(), i64> {
    let mut m = Msg::new(proto::SCAN, 1, 0);
    m.mr[0] = 0;
    let r = call(&m)?;
    let count = ipc::header(&r, 0) as usize;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack_at(&r, 1, &mut buf);
    println!(
        "{:<20} {:>4} {:>7} {:>6}  {}",
        "SSID", "chan", "signal", "sec", "BSSID"
    );
    let mut at = 0;
    for _ in 0..count {
        if at + proto::RECORD > n {
            break;
        }
        let rec = &buf[at..at + proto::RECORD];
        let len = rec[0] as usize;
        if at + proto::RECORD + len > n {
            break;
        }
        let ssid = &buf[at + proto::RECORD..at + proto::RECORD + len];
        println!(
            "{:<20} {:>4} {:>4} dBm {:>6}  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            core::str::from_utf8(ssid).unwrap_or("?"),
            rec[1],
            -(rec[3] as i32),
            security_name(rec[2]),
            rec[4],
            rec[5],
            rec[6],
            rec[7],
            rec[8],
            rec[9]
        );
        at += proto::RECORD + len;
    }
    Ok(())
}

fn connect(ssid: &str, psk: &str) -> Result<(), i64> {
    let mut data = Vec::from(ssid.as_bytes());
    data.push(0);
    data.extend_from_slice(psk.as_bytes());
    // Word 0 is the timeout the service may use; 0 means its default.
    call(&ipc::pack_at(proto::CONNECT, &[0], &data))?;
    println!("connected to {}", ssid);
    Ok(())
}

fn status() -> Result<(), i64> {
    let r = call(&Msg::new(proto::STATUS, 0, 0))?;
    let state = ipc::header(&r, 0);
    let (channel, signal, mac) = (
        ipc::header(&r, 1),
        ipc::header(&r, 2),
        ipc::header(&r, 3).to_le_bytes(),
    );
    let mut buf = [0u8; proto::MAX_SSID];
    let n = ipc::unpack_at(&r, 4, &mut buf);
    println!(
        "state {}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        state_name(state),
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    if state == proto::ST_CONNECTED {
        println!(
            "network {}, channel {}, signal -{} dBm",
            core::str::from_utf8(&buf[..n]).unwrap_or("?"),
            channel,
            signal
        );
    }
    Ok(())
}

fn main() -> i32 {
    let args: Vec<&str> = env::args().collect();
    let r = match args.get(1).copied() {
        Some("scan") => scan(),
        Some("connect") if args.len() >= 3 => connect(args[2], args.get(3).copied().unwrap_or("")),
        Some("disconnect") => call(&Msg::new(proto::DISCONNECT, 0, 0)).map(|_| ()),
        Some("status") | None => status(),
        _ => {
            println!("usage: wifi scan | connect SSID [PASSPHRASE] | disconnect | status");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            println!("wifi: {}", err::name(e));
            1
        }
    }
}

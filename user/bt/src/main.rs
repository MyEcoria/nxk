//! bt: Bluetooth controllers from the shell (status, pair, list,
//! forget, retry). Client of the `bluetooth` service, published as "bt".

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::proto::bt as proto;
use libnx::ipc::{self, Msg};
use libnx::{env, ns, println};

libnx::entry!(main);

/// Bring-up (power cycle, firmware) and a pairing window outlast the
/// default reply wait.
const CALL_TIMEOUT: u64 = 20_000_000_000;
/// The service waits for the card before publishing its state.
const SERVICE_WAIT_NS: u64 = 40_000_000_000;

fn call_for(m: &Msg, timeout: u64) -> Result<Msg, i64> {
    let ep = ns::wait_for_timeout("bt", SERVICE_WAIT_NS)?;
    ipc::checked(ipc::call_timeout(ep, m, timeout)?)
}

fn call(m: &Msg) -> Result<Msg, i64> {
    call_for(m, CALL_TIMEOUT)
}

fn addr_str(w: u64) -> String {
    let b = w.to_be_bytes();
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        b[2], b[3], b[4], b[5], b[6], b[7]
    )
}

fn parse_addr(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    parts.iter().try_fold(0u64, |w, p| {
        Some(w << 8 | u8::from_str_radix(p, 16).ok()? as u64)
    })
}

fn state_name(s: u64) -> &'static str {
    match s {
        proto::ST_STARTING => "starting",
        proto::ST_READY => "ready",
        proto::ST_PAIRING => "pairing",
        _ => "down",
    }
}

fn kind_name(k: u8) -> &'static str {
    match k {
        proto::KIND_JOYCON_L => "Joy-Con (L)",
        proto::KIND_JOYCON_R => "Joy-Con (R)",
        proto::KIND_PRO => "Pro Controller",
        _ => "controller",
    }
}

fn status_reply(r: &Msg) {
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack_at(r, 4, &mut buf);
    println!(
        "state {}, {} connected, {} paired",
        state_name(ipc::header(r, 0)),
        ipc::header(r, 1),
        ipc::header(r, 2)
    );
    if n > 0 {
        println!("adapter {}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
    }
}

fn status() -> Result<(), i64> {
    status_reply(&call(&Msg::new(proto::STATUS, 0, 0))?);
    Ok(())
}

fn list() -> Result<(), i64> {
    let r = call(&Msg::new(proto::LIST, 0, 0))?;
    let count = ipc::header(&r, 0) as usize;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack_at(&r, 1, &mut buf);
    if count == 0 {
        println!("no controller paired (bt pair, then hold its sync button)");
        return Ok(());
    }
    println!(
        "{:<17}  {:<14}  {:<9}  {:<7}  {}",
        "address", "kind", "state", "battery", "name"
    );
    let mut at = 0;
    for _ in 0..count {
        if at + proto::RECORD > n {
            break;
        }
        let rec = &buf[at..at + proto::RECORD];
        let len = rec[9] as usize;
        if at + proto::RECORD + len > n {
            break;
        }
        let name =
            core::str::from_utf8(&buf[at + proto::RECORD..at + proto::RECORD + len]).unwrap_or("?");
        let addr = rec[..6].iter().rev().fold(0u64, |w, &b| w << 8 | b as u64);
        let battery = if rec[8] == proto::BATTERY_UNKNOWN {
            String::from("-")
        } else {
            format!("{}/4", rec[8])
        };
        println!(
            "{:<17}  {:<14}  {:<9}  {:<7}  {}",
            addr_str(addr),
            kind_name(rec[6]),
            if rec[7] != 0 { "connected" } else { "paired" },
            battery,
            name
        );
        at += proto::RECORD + len;
    }
    Ok(())
}

fn pair(secs: u64) -> Result<(), i64> {
    println!("pairing for {} s: hold the controller's sync button", secs);
    let window = secs * 1_000_000_000;
    let r = call_for(
        &Msg::from_words(proto::PAIR, &[window, 1]),
        window + CALL_TIMEOUT,
    )?;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack_at(&r, 1, &mut buf);
    println!(
        "paired {} ({})",
        core::str::from_utf8(&buf[..n]).unwrap_or("?"),
        addr_str(ipc::header(&r, 0))
    );
    // The window stays open for a second controller; close it.
    let _ = call(&Msg::from_words(proto::PAIR, &[0, 0]));
    Ok(())
}

fn main() -> i32 {
    let args: Vec<&str> = env::args().collect();
    let r = match args.get(1).copied() {
        Some("pair") => {
            let secs = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(proto::PAIR_DEFAULT_NS / 1_000_000_000);
            pair(secs)
        }
        Some("list") => list(),
        Some("forget") => match args.get(2).copied() {
            Some("all") => call(&Msg::from_words(proto::FORGET, &[0])).map(|r| {
                println!("{} forgotten", ipc::header(&r, 0));
            }),
            Some(a) => match parse_addr(a) {
                Some(w) => call(&Msg::from_words(proto::FORGET, &[w])).map(|r| {
                    println!("{} forgotten", ipc::header(&r, 0));
                }),
                None => Err(-err::EINVAL),
            },
            None => Err(-err::EINVAL),
        },
        Some("retry") => call(&Msg::new(proto::RETRY, 0, 0)).map(|r| status_reply(&r)),
        Some("status") | None => status(),
        _ => {
            println!("usage: bt status | pair [SECONDS] | list | forget ADDRESS|all | retry");
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            println!("bt: {}", err::name(e));
            1
        }
    }
}

//! wifi: Wi-Fi from the shell (scan, connect, status, disconnect).
//! Client of the `wlan` service, published as "wifi".

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::proto::{net, wifi as proto};
use libnx::ipc::{self, Msg};
use libnx::{env, ns, println, time};

libnx::entry!(main);

/// Connecting and scanning outlast the default reply wait.
const CALL_TIMEOUT: u64 = 20_000_000_000;
/// After a join, how long DHCP gets to give `net` an address.
const ADDRESS_WAIT_NS: u64 = 10_000_000_000;

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
        proto::ST_RADIO_OFF => "radio off",
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
    address();
    Ok(())
}

/// Once joined, the address DHCP gives `net` over Wi-Fi: the network is
/// usable when this returns. Nothing to wait for if `net` runs over a
/// wired link.
fn address() {
    let end = time::now_ns() + ADDRESS_WAIT_NS;
    let ask = |ep: u32, label: u32| {
        ipc::call_timeout(ep, &Msg::new(label, 0, 0), 1_000_000_000).and_then(ipc::checked)
    };
    let Ok(ep) = ns::wait_for_timeout("net", ADDRESS_WAIT_NS) else {
        return;
    };
    match ask(ep, net::STATS) {
        Ok(r) if ipc::header(&r, 5) == net::LINK_WLAN0 => {}
        _ => return,
    }
    let ip = |v: u64| {
        let b = (v as u32).to_be_bytes();
        alloc::format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    };
    while time::now_ns() < end {
        if let Ok(r) = ask(ep, net::INFO) {
            if ipc::header(&r, 0) != 0 {
                println!(
                    "address {}/{}, gateway {}, DNS {}",
                    ip(ipc::header(&r, 0)),
                    ipc::header(&r, 1),
                    ip(ipc::header(&r, 2)),
                    ip(ipc::header(&r, 3))
                );
                return;
            }
        }
        time::sleep_ms(100);
    }
    println!(
        "no address from DHCP after {} s",
        ADDRESS_WAIT_NS / 1_000_000_000
    );
}

/// One line on what the adapter is, or why there is none.
fn diag() -> Result<(), i64> {
    text(proto::DIAG)
}

/// Reply made of bytes, printed as a line.
fn text(label: u32) -> Result<(), i64> {
    let r = call(&Msg::new(label, 0, 0))?;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack(&r, &mut buf);
    if n > 0 {
        println!("adapter {}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
    }
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
    let _ = diag();
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
        Some("retry") => text(proto::RETRY),
        Some("poke") if args.get(2).copied() == Some("die") => {
            call(&Msg::from_words(proto::POKE, &[1])).map(|_| ())
        }
        Some("poke") => text(proto::POKE),
        Some("radio") if matches!(args.get(2).copied(), Some("on" | "off")) => {
            let on = args[2] == "on";
            call(&Msg::from_words(proto::RADIO, &[on as u64]))
                .map(|_| println!("radio {}", args[2]))
        }
        Some("status") | None => status(),
        _ => {
            println!("usage: wifi scan | connect SSID [PASSPHRASE] | disconnect | status | retry | radio on|off");
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

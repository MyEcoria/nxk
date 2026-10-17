//! What the pages say about the hardware and the links: short texts and
//! the color they show in.

use alloc::format;
use alloc::string::String;

use libnx::abi::proto::{cpufreq, fan, gpu, usbcon as usb, wifi};

use crate::stats::{Cpu, Data, Eth, Fan, Gpu, Now, Seen, Wifi};
use crate::theme::pal;

pub fn mhz(khz: u64) -> String {
    match khz {
        0 => "?".into(),
        k => format!("{}", (k + 500) / 1000),
    }
}

/// GPU clock in MHz, one decimal (sys-clk's steps are 76.8 MHz apart).
pub fn gpu_mhz(khz: u64) -> String {
    format!("{}.{}", khz / 1000, khz % 1000 / 100)
}

pub fn usb_status(d: &Data) -> (String, u32) {
    let p = pal();
    match d.usb {
        Now::Missing => ("console not running".into(), p.dim),
        Now::Busy => ("busy".into(), p.dim),
        Now::Value(u) => {
            let speed = if u.state & usb::ST_HIGH_SPEED != 0 {
                "high speed"
            } else {
                "full speed"
            };
            if u.state & usb::ST_TERMINAL != 0 {
                (format!("terminal open, {}", speed), p.good)
            } else if u.state & usb::ST_CONFIGURED != 0 {
                (format!("connected, {}, terminal closed", speed), p.warn)
            } else if u.state & usb::ST_HOST != 0 {
                ("host seen, not configured".into(), p.warn)
            } else {
                ("waiting for a PC".into(), p.dim)
            }
        }
    }
}

pub fn usb_short(d: &Data) -> (&'static str, u32) {
    let p = pal();
    match d.usb {
        Now::Missing => ("not running", p.dim),
        Now::Busy => ("busy", p.dim),
        Now::Value(u) if u.state & usb::ST_TERMINAL != 0 => ("open", p.good),
        Now::Value(u) if u.state & usb::ST_CONFIGURED != 0 => ("closed", p.warn),
        Now::Value(u) if u.state & usb::ST_HOST != 0 => ("host seen", p.warn),
        Now::Value(_) => ("no PC", p.dim),
    }
}

pub fn wifi_short(d: &Data) -> (&'static str, u32) {
    let p = pal();
    match d.wifi {
        Seen::Unknown => ("starting", p.dim),
        Seen::Missing => ("no Wi-Fi driver", p.dim),
        Seen::Value(w) => match w.state {
            wifi::ST_CONNECTED => ("connected", p.good),
            wifi::ST_CONNECTING => ("joining", p.warn),
            wifi::ST_SCANNING => ("scanning", p.warn),
            wifi::ST_IDLE => ("not connected", p.dim),
            wifi::ST_RADIO_OFF => ("radio off", p.warn),
            _ => ("no adapter", p.bad),
        },
    }
}

pub fn eth_text(d: &Data, short: bool) -> (&'static str, u32) {
    let p = pal();
    match d.eth {
        Eth::Up if d.eth_name == "usb0" => ("shared by the PC", p.good),
        Eth::Up => ("connected", p.good),
        Eth::Dhcp if short => ("waiting for DHCP", p.warn),
        Eth::Dhcp => ("link up, waiting for DHCP", p.warn),
        Eth::NoSharing if short => ("no DHCP answer", p.warn),
        Eth::NoSharing => ("no DHCP: sharing off on the PC?", p.warn),
        Eth::Off if short => ("off on the PC", p.dim),
        Eth::Off => ("link not turned on by the PC", p.dim),
        Eth::NoPc if short => ("no PC", p.dim),
        Eth::NoPc => ("no PC on USB-C", p.dim),
        Eth::Missing => ("not running", p.dim),
    }
}

/// Fan state: its mode and value, and the color it shows in.
pub fn fan_text(f: &Fan) -> (String, u32) {
    let p = pal();
    let (mode, c) = if f.state & fan::ST_HOT != 0 {
        ("hot", p.warn)
    } else if f.state & fan::ST_NO_SENSOR != 0 {
        ("no sensor", p.warn)
    } else if f.state & fan::ST_MANUAL != 0 {
        ("manual", p.text)
    } else {
        ("auto", p.text)
    };
    let text = match (f.percent, f.rpm) {
        (0, _) => format!("{}: off", mode),
        (p, Some(rpm)) => format!("{}: {}%, {} rpm", mode, p, rpm),
        (p, None) => format!("{}: {}%", mode, p),
    };
    (text, c)
}

/// CPU clock and rail, and the color they show in.
pub fn cpu_text(c: &Cpu) -> (String, u32) {
    let p = pal();
    let rail = match c.uv {
        0 => String::new(),
        uv => format!(", {} mV", uv / 1000),
    };
    if c.state & cpufreq::ST_UNSUPPORTED != 0 {
        (format!("{} MHz, boot only", mhz(c.khz)), p.dim)
    } else if c.state & cpufreq::ST_HOT != 0 {
        (format!("hot: {} MHz{}", mhz(c.khz), rail), p.warn)
    } else if c.step == cpufreq::BOOT {
        (format!("boot: {} MHz{}", mhz(c.khz), rail), p.text)
    } else {
        (format!("{} MHz{}", mhz(c.khz), rail), p.text)
    }
}

/// The GPU row: off, powering up, its clock and rail (warning color when
/// hot or forced past the cap), or where power-up failed.
pub fn gpu_text(g: &Seen<Gpu>) -> (String, u32) {
    const STAGES: [&str; 11] = [
        "",
        "rail",
        "clocks",
        "PLL",
        "MMU",
        "firmware",
        "secure boot",
        "FECS",
        "context",
        "channel",
        "ready",
    ];
    let p = pal();
    let stage = |g: &Gpu| *STAGES.get(g.stage as usize).unwrap_or(&"?");
    match g {
        Seen::Value(g) if g.state == gpu::ON => {
            let clock = match g.khz {
                0 => "bypass clock".into(),
                k => format!("{} MHz", gpu_mhz(k)),
            };
            let text = format!("{}, {} mV", clock, g.uv / 1000);
            if g.ck & gpu::CK_HOT != 0 {
                (format!("hot: {}", text), p.warn)
            } else if g.ck & gpu::CK_FORCED != 0 {
                (text, p.warn)
            } else {
                (text, p.good)
            }
        }
        Seen::Value(g) if g.state == gpu::FAILED => (format!("failed: {}", stage(g)), p.bad),
        Seen::Value(g) if g.stage != 0 => (format!("powering up: {}", stage(g)), p.dim),
        Seen::Value(_) => ("off".into(), p.dim),
        Seen::Missing => ("no GPU service".into(), p.dim),
        Seen::Unknown => ("starting".into(), p.dim),
    }
}

pub fn wifi_connected(d: &Data) -> Option<Wifi> {
    match d.wifi {
        Seen::Value(w) if w.state == wifi::ST_CONNECTED => Some(w),
        _ => None,
    }
}

/// `10:25 PM` from the wall clock, `--:--` until the clock service sets it.
pub fn clock(wall_ns: u64) -> String {
    let s = wall_ns / 1_000_000_000;
    if s < 978_307_200 {
        return "--:--".into();
    }
    let (h, min) = (s / 3600 % 24, s / 60 % 60);
    let h12 = match h % 12 {
        0 => 12,
        h => h,
    };
    format!("{}:{:02} {}", h12, min, if h < 12 { "AM" } else { "PM" })
}

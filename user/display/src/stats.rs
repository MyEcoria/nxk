//! Home-screen statistics: kernel counters (`sysinfo`) and the services'
//! own, one line each. A missing service shows as such, a busy one keeps
//! its last values: the screen never waits more than `CALL_NS` on one.

use core::fmt::Write;

use libnx::abi::err::ETIMEDOUT;
use libnx::abi::info::{self, CpuInfo, MemInfo};
use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::{fs, net, power, usbcon as usb, wifi};
use libnx::ipc::{self, Msg};
use libnx::ns::Service;
use libnx::{sys, time};

use crate::screen::{Bytes, Grid, Ipv4, Line, Screen, BAD, DIM, FG, GOOD, WARN};

/// Longest wait on a service's reply.
const CALL_NS: u64 = 20_000_000;
/// The sensor (I2C), Wi-Fi and the card are asked every `SLOW` refreshes
/// once they answered, every refresh before.
const SLOW: u64 = 5;
/// Boot time during which a service not published yet is still starting.
const BOOT_NS: u64 = 30_000_000_000;
const LABEL: usize = 9;
const CPUS: usize = 4;
/// Columns per core on the CPU line, and its bar's width in cells.
const CPU_COLS: usize = 17;
const BAR: usize = 8;
const TRACK: u32 = 0xFF50_3020;

/// Lines drawn by `Stats::refresh`.
pub const ROWS: usize = 8;

static USB: Service = Service::optional("usbcon");
static NET: Service = Service::optional("net");
static WIFI: Service = Service::optional("wifi");
static POWER: Service = Service::optional("power");
static FS: Service = Service::optional("fs");

/// What a service answered.
enum Reply {
    Ok(Msg),
    /// Not published, or failed.
    Missing,
    /// No answer within `CALL_NS`.
    Busy,
}

fn ask(s: &Service, label: u32) -> Reply {
    match s.call_timeout(&Msg::new(label, 0, 0), CALL_NS) {
        Ok(m) => Reply::Ok(m),
        Err(e) if e == -ETIMEDOUT => Reply::Busy,
        Err(_) => Reply::Missing,
    }
}

/// Last answer of a slowly polled service.
#[derive(Clone, Copy)]
enum Seen<T> {
    Unknown,
    Missing,
    Value(T),
}

impl<T> Seen<T> {
    /// Ask `s` again: always until it answers, then every `SLOW` refreshes.
    fn poll(&mut self, s: &Service, label: u32, n: u64, f: impl FnOnce(&Msg) -> T) {
        if matches!(self, Seen::Value(_)) && !n.is_multiple_of(SLOW) {
            return;
        }
        match ask(s, label) {
            Reply::Ok(m) => *self = Seen::Value(f(&m)),
            Reply::Missing if time::now_ns() > BOOT_NS => *self = Seen::Missing,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
struct Wifi {
    state: u64,
    channel: u64,
    signal: u64,
    ssid: [u8; wifi::MAX_SSID],
    len: usize,
}

impl Wifi {
    fn ssid(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.len]).unwrap_or("?")
    }
}

/// Kernel table `kind` into `N` records; returns them and their count.
fn sysinfo<T: Copy + Default, const N: usize>(kind: u64) -> ([T; N], usize) {
    let mut v = [T::default(); N];
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, core::mem::size_of_val(&v))
    };
    let n = sys::sysinfo(kind, bytes).unwrap_or(0) as usize;
    (v, n.min(N))
}

fn label(name: &str) -> Line {
    let mut l = Line::new();
    let _ = write!(l.color(crate::accent()), "{}", name);
    l.pad(LABEL);
    l.color(FG);
    l
}

/// Millidegrees as `42.1`.
struct Celsius(i64);

impl core::fmt::Display for Celsius {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(
            f,
            "{}.{}\u{b0}C",
            self.0 / 1000,
            (self.0 % 1000).abs() / 100
        )
    }
}

pub struct Stats {
    /// Previous sample: time and per-core counters.
    prev: Option<(u64, [CpuInfo; CPUS])>,
    refreshes: u64,
    temps: Seen<(i64, i64)>,
    wifi: Seen<Wifi>,
    disk: Seen<(u64, u64)>,
    /// Filled width (px) of each CPU bar as drawn (`usize::MAX`: never).
    bars: [usize; CPUS],
}

impl Stats {
    pub fn new() -> Self {
        Self {
            prev: None,
            refreshes: 0,
            temps: Seen::Unknown,
            wifi: Seen::Unknown,
            disk: Seen::Unknown,
            bars: [usize::MAX; CPUS],
        }
    }

    /// New sample; rows 0..ROWS of `grid` redrawn where they changed.
    pub fn refresh(&mut self, scr: &mut Screen, grid: &mut Grid) {
        let now = time::now_ns();
        self.poll_slow();
        self.refreshes += 1;
        let (cpus, ncpu) = sysinfo::<CpuInfo, CPUS>(info::CPUS);
        let (t0, before) = self.prev.unwrap_or((0, [CpuInfo::default(); CPUS]));
        self.prev = Some((now, cpus));
        let load: [Option<u64>; CPUS] = core::array::from_fn(|k| {
            let (c, p) = (&cpus[k], &before[k]);
            (k < ncpu && c.online != 0).then(|| {
                let busy = c.busy_ns - p.busy_ns;
                busy * 100 / (busy + c.idle_ns - p.idle_ns).max(1)
            })
        });
        grid.line(scr, 0, &self.system(now, &load));
        grid.line(scr, 1, &cpu_line(&load));
        self.cpu_bars(scr, grid, &load);
        grid.line(scr, 2, &rates(&cpus[..ncpu], &before[..ncpu], now - t0));
        grid.line(scr, 3, &memory());
        grid.line(scr, 4, &usb_line());
        grid.line(scr, 5, &net_line());
        grid.line(scr, 6, &self.wifi_line());
        grid.line(scr, 7, &self.disk_line());
    }

    /// Sources that are slow to answer or rarely change.
    fn poll_slow(&mut self) {
        let n = self.refreshes;
        self.temps.poll(&POWER, power::TEMPERATURE, n, |m| {
            (m.mr[0] as i64, m.mr[1] as i64)
        });
        self.wifi.poll(&WIFI, wifi::STATUS, n, |m| {
            let mut w = Wifi {
                state: m.mr[0],
                channel: m.mr[1],
                signal: m.mr[2],
                ssid: [0; wifi::MAX_SSID],
                len: 0,
            };
            w.len = ipc::unpack_at(m, 4, &mut w.ssid);
            w
        });
        self.disk.poll(&FS, fs::STATFS, n, |m| (m.mr[0], m.mr[1]));
    }

    fn system(&self, now: u64, load: &[Option<u64>]) -> Line {
        let mut l = label("SYSTEM");
        let s = now / 1_000_000_000;
        let procs = sys::sysinfo(info::PROCESSES, &mut []).unwrap_or(0);
        let cores = load.iter().filter(|c| c.is_some()).count();
        let _ = write!(
            l,
            "up {}:{:02}:{:02}   {} processes   {} cores",
            s / 3600,
            s / 60 % 60,
            s % 60,
            procs,
            cores
        );
        if let Seen::Value((chip, package)) = self.temps {
            let _ = write!(l, "   chip {}  package {}", Celsius(chip), Celsius(package));
        }
        l
    }

    /// Load bars in the CPU line's blank cells, redrawn when they change.
    fn cpu_bars(&mut self, scr: &mut Screen, grid: &Grid, load: &[Option<u64>; CPUS]) {
        let s = grid.scale;
        let full = BAR * 8 * s;
        for (k, pct) in load.iter().enumerate() {
            let filled = pct.map_or(0, |p| full * p.min(100) as usize / 100);
            if self.bars[k] == filled {
                continue;
            }
            let (x, y) = grid.pos(LABEL + k * CPU_COLS + 2, 1);
            let (y, h) = (y + 4 * s, 8 * s);
            let color = match pct {
                Some(p) if *p >= 85 => BAD,
                Some(p) if *p >= 50 => WARN,
                _ => GOOD,
            };
            scr.fill(x, y, filled, h, color);
            scr.fill(x + filled, y, full - filled, h, TRACK);
            self.bars[k] = filled;
        }
    }

    fn wifi_line(&self) -> Line {
        let mut l = label("WI-FI");
        let _ = match &self.wifi {
            Seen::Unknown => write!(l.color(DIM), "starting"),
            Seen::Missing => write!(l.color(DIM), "no Wi-Fi driver"),
            Seen::Value(w) => match w.state {
                wifi::ST_CONNECTED => {
                    let _ = write!(l.color(GOOD), "connected to {}", w.ssid());
                    write!(l.color(FG), ", channel {}, -{} dBm", w.channel, w.signal)
                }
                wifi::ST_CONNECTING => write!(l.color(WARN), "joining {}", w.ssid()),
                wifi::ST_SCANNING => write!(l.color(WARN), "scanning"),
                wifi::ST_IDLE => write!(l.color(DIM), "not connected (wifi connect SSID)"),
                _ => write!(l.color(DIM), "no adapter (wifi status)"),
            },
        };
        l
    }

    fn disk_line(&self) -> Line {
        let mut l = label("STORAGE");
        let _ = match self.disk {
            Seen::Unknown => write!(l.color(DIM), "starting"),
            Seen::Missing => write!(l.color(DIM), "no card"),
            Seen::Value((free, total)) => {
                write!(l, "SD card, {} free of {}", Bytes(free), Bytes(total))
            }
        };
        l
    }
}

fn cpu_line(load: &[Option<u64>; CPUS]) -> Line {
    let mut l = label("CPU");
    for (k, pct) in load.iter().enumerate() {
        l.pad(LABEL + k * CPU_COLS);
        let _ = write!(l.color(FG), "{}", k);
        l.pad(LABEL + k * CPU_COLS + 2 + BAR);
        let _ = match pct {
            Some(p) => write!(l, " {:>3}%", p),
            None => write!(l.color(DIM), " off"),
        };
    }
    l
}

/// Kernel activity per second, all cores together.
fn rates(now: &[CpuInfo], before: &[CpuInfo], dt: u64) -> Line {
    let mut l = label("LOAD");
    let per_s = |f: fn(&CpuInfo) -> u64| {
        let d: u64 = now.iter().zip(before).map(|(c, p)| f(c) - f(p)).sum();
        d * 1_000_000_000 / dt.max(1)
    };
    let _ = write!(
        l,
        "{} syscalls/s   {} interrupts/s   {} context switches/s",
        per_s(|c| c.syscalls),
        per_s(|c| c.irqs),
        per_s(|c| c.switches)
    );
    l
}

fn memory() -> Line {
    let mut l = label("MEMORY");
    let (m, n) = sysinfo::<MemInfo, 1>(info::MEMORY);
    if n == 1 {
        let page = |p: u64| Bytes(p * PAGE_SIZE);
        let m = &m[0];
        let _ = write!(
            l,
            "{} used of {}   kernel {}   {} free",
            page(m.total_pages - m.free_pages),
            page(m.total_pages),
            page(m.kernel_pages),
            page(m.free_pages)
        );
    }
    l
}

fn usb_line() -> Line {
    let mut l = label("USB-C");
    let m = match ask(&USB, usb::STATS) {
        Reply::Ok(m) => m,
        Reply::Missing => {
            let _ = write!(l.color(DIM), "console not running");
            return l;
        }
        Reply::Busy => {
            let _ = write!(l.color(DIM), "busy");
            return l;
        }
    };
    let st = m.mr[0];
    let speed = if st & usb::ST_HIGH_SPEED != 0 {
        "high speed"
    } else {
        "full speed"
    };
    let _ = if st & usb::ST_TERMINAL != 0 {
        write!(l.color(GOOD), "terminal open, {}", speed)
    } else if st & usb::ST_CONFIGURED != 0 {
        write!(l.color(WARN), "connected, {}, terminal closed", speed)
    } else if st & usb::ST_HOST != 0 {
        write!(l.color(WARN), "host seen, not configured")
    } else {
        write!(l.color(DIM), "waiting for a PC")
    };
    l.pad(LABEL + 36);
    let _ = write!(
        l.color(FG),
        "in {}   out {}",
        Bytes(m.mr[1]),
        Bytes(m.mr[2])
    );
    let errors = m.mr[5] >> 32;
    if errors != 0 {
        let _ = write!(l.color(BAD), "   {} errors", errors);
    }
    l
}

fn net_line() -> Line {
    let mut l = label("NETWORK");
    let info = match ask(&NET, net::INFO) {
        Reply::Ok(m) => m,
        Reply::Missing => {
            let _ = write!(l.color(DIM), "waiting for a link (eth0 or wlan0)");
            return l;
        }
        Reply::Busy => {
            let _ = write!(l.color(DIM), "busy");
            return l;
        }
    };
    let stats = match ask(&NET, net::STATS) {
        Reply::Ok(m) => Some(m),
        _ => None,
    };
    let link = match stats.map(|m| m.mr[5]) {
        Some(net::LINK_ETH0) => "eth0",
        Some(net::LINK_WLAN0) => "wlan0",
        _ => "link",
    };
    let _ = if info.mr[0] == 0 {
        write!(l.color(WARN), "{}: waiting for DHCP", link)
    } else {
        write!(
            l.color(GOOD),
            "{} {}/{}",
            link,
            Ipv4(info.mr[0] as u32),
            info.mr[1]
        )
    };
    if let Some(s) = stats {
        l.pad(LABEL + 28);
        let _ = write!(
            l.color(FG),
            "rx {}   tx {}   {} sockets",
            Bytes(s.mr[1]),
            Bytes(s.mr[3]),
            s.mr[4]
        );
    }
    l
}

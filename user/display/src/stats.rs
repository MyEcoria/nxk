//! What the screen shows: kernel counters (`sysinfo`) and the services'
//! own. A missing service shows as such, a busy one keeps its last
//! values: a refresh never waits more than `CALL_NS` on one.

use libnx::abi::err::ETIMEDOUT;
use libnx::abi::info::{self, CpuInfo, MemInfo};
use libnx::abi::proto::{eth, fs, net, power, usbcon as usb, wifi};
use libnx::ipc::{self, Msg};
use libnx::ns::Service;
use libnx::{sys, time};

/// Longest wait on a service's reply.
const CALL_NS: u64 = 20_000_000;
/// The sensor (I2C), Wi-Fi, the card and link addresses are asked every
/// `SLOW` refreshes once they answered, every refresh before.
const SLOW: u64 = 5;
/// Boot time during which a service not published yet is still starting.
const BOOT_NS: u64 = 30_000_000_000;
pub const CPUS: usize = 4;

static USB: Service = Service::optional("usbcon");
static NET: Service = Service::optional("net");
static WIFI: Service = Service::optional("wifi");
static POWER: Service = Service::optional("power");
static FS: Service = Service::optional("fs");
static ETH0: Service = Service::optional("eth0");

/// A service asked every refresh.
#[derive(Clone, Copy)]
pub enum Now<T> {
    /// Not published, or failed.
    Missing,
    /// No answer within `CALL_NS`.
    Busy,
    Value(T),
}

fn ask(s: &Service, label: u32) -> Now<Msg> {
    match s.call_timeout(&Msg::new(label, 0, 0), CALL_NS) {
        Ok(m) => Now::Value(m),
        Err(e) if e == -ETIMEDOUT => Now::Busy,
        Err(_) => Now::Missing,
    }
}

/// Last answer of a slowly polled service.
#[derive(Clone, Copy)]
pub enum Seen<T> {
    Unknown,
    Missing,
    Value(T),
}

impl<T> Seen<T> {
    /// Ask `s` again: always until it answers, then every `SLOW` refreshes
    /// (or now if `force`).
    fn poll(&mut self, s: &Service, label: u32, n: u64, force: bool, f: impl FnOnce(&Msg) -> T) {
        if matches!(self, Seen::Value(_)) && !force && !n.is_multiple_of(SLOW) {
            return;
        }
        match ask(s, label) {
            Now::Value(m) => *self = Seen::Value(f(&m)),
            Now::Missing if time::now_ns() > BOOT_NS => *self = Seen::Missing,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub struct Wifi {
    pub state: u64,
    pub channel: u64,
    pub signal: u64,
    ssid: [u8; wifi::MAX_SSID],
    len: usize,
}

impl Wifi {
    pub fn ssid(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.len]).unwrap_or("?")
    }
}

#[derive(Clone, Copy)]
pub struct Usb {
    /// `usbcon::ST_*` bits.
    pub state: u64,
    pub rx: u64,
    pub tx: u64,
    pub errors: u64,
}

#[derive(Clone, Copy, Default)]
pub struct Net {
    /// Address (0: waiting for DHCP), prefix, gateway, DNS server.
    pub addr: u32,
    pub prefix: u64,
    pub gateway: u32,
    pub dns: u32,
    pub rx: u64,
    pub tx: u64,
    pub sockets: u64,
    /// `net::LINK_*` in use.
    pub link: u64,
}

impl Net {
    pub fn link_name(&self) -> &'static str {
        match self.link {
            net::LINK_ETH0 => "eth0",
            net::LINK_WLAN0 => "wlan0",
            _ => "link",
        }
    }

    pub fn link_label(&self) -> &'static str {
        match self.link {
            net::LINK_ETH0 => "USB Ethernet",
            net::LINK_WLAN0 => "Wi-Fi",
            _ => "network",
        }
    }
}

/// The wired link, as the network page shows it.
#[derive(Clone, Copy, PartialEq)]
pub enum Eth {
    Missing,
    /// No PC on the USB-C port.
    NoPc,
    Dhcp,
    Up,
}

pub struct Data {
    pub up_s: u64,
    /// Per-core load (%), `None`: core off.
    pub load: [Option<u64>; CPUS],
    /// Syscalls, interrupts, context switches per second, all cores.
    pub rates: [u64; 3],
    pub procs: u64,
    pub mem: Option<MemInfo>,
    pub temps: Seen<(i64, i64)>,
    pub wifi: Seen<Wifi>,
    /// Card: free and total bytes.
    pub disk: Seen<(u64, u64)>,
    pub usb: Now<Usb>,
    pub net: Now<Net>,
    pub eth: Eth,
    /// Wired link name and MAC (0: unknown).
    pub eth_name: &'static str,
    pub eth_mac: u64,
    /// Previous sample: time and per-core counters.
    prev: Option<(u64, [CpuInfo; CPUS])>,
    refreshes: u64,
    /// Ask the slow sources at the next refresh (an action changed them).
    force: bool,
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

impl Data {
    pub fn new() -> Self {
        Self {
            up_s: 0,
            load: [None; CPUS],
            rates: [0; 3],
            procs: 0,
            mem: None,
            temps: Seen::Unknown,
            wifi: Seen::Unknown,
            disk: Seen::Unknown,
            usb: Now::Missing,
            net: Now::Missing,
            eth: Eth::Missing,
            eth_name: "",
            eth_mac: 0,
            prev: None,
            refreshes: 0,
            force: false,
        }
    }

    /// Ask the slow sources again at the next refresh.
    pub fn soon(&mut self) {
        self.force = true;
    }

    /// New sample of everything.
    pub fn refresh(&mut self) {
        let now = time::now_ns();
        self.up_s = now / 1_000_000_000;
        let (n, force) = (self.refreshes, core::mem::take(&mut self.force));
        self.refreshes += 1;

        let (cpus, ncpu) = sysinfo::<CpuInfo, CPUS>(info::CPUS);
        let (t0, before) = self.prev.unwrap_or((0, [CpuInfo::default(); CPUS]));
        self.prev = Some((now, cpus));
        self.load = core::array::from_fn(|k| {
            let (c, p) = (&cpus[k], &before[k]);
            (k < ncpu && c.online != 0).then(|| {
                let busy = c.busy_ns - p.busy_ns;
                busy * 100 / (busy + c.idle_ns - p.idle_ns).max(1)
            })
        });
        let dt = (now - t0).max(1);
        let per_s = |f: fn(&CpuInfo) -> u64| {
            let d: u64 = cpus[..ncpu]
                .iter()
                .zip(&before[..ncpu])
                .map(|(c, p)| f(c) - f(p))
                .sum();
            d * 1_000_000_000 / dt
        };
        self.rates = [
            per_s(|c| c.syscalls),
            per_s(|c| c.irqs),
            per_s(|c| c.switches),
        ];
        self.procs = sys::sysinfo(info::PROCESSES, &mut []).unwrap_or(0);
        let (m, k) = sysinfo::<MemInfo, 1>(info::MEMORY);
        self.mem = (k == 1).then_some(m[0]);

        self.temps.poll(&POWER, power::TEMPERATURE, n, force, |m| {
            (m.mr[0] as i64, m.mr[1] as i64)
        });
        self.wifi.poll(&WIFI, wifi::STATUS, n, force, |m| {
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
        self.disk
            .poll(&FS, fs::STATFS, n, force, |m| (m.mr[0], m.mr[1]));

        self.usb = match ask(&USB, usb::STATS) {
            Now::Value(m) => Now::Value(Usb {
                state: m.mr[0],
                rx: m.mr[1],
                tx: m.mr[2],
                errors: m.mr[5] >> 32,
            }),
            Now::Busy => Now::Busy,
            Now::Missing => Now::Missing,
        };
        self.net = match ask(&NET, net::INFO) {
            Now::Value(i) => {
                let mut v = Net {
                    addr: i.mr[0] as u32,
                    prefix: i.mr[1],
                    gateway: i.mr[2] as u32,
                    dns: i.mr[3] as u32,
                    ..Net::default()
                };
                if let Now::Value(s) = ask(&NET, net::STATS) {
                    (v.rx, v.tx, v.sockets, v.link) = (s.mr[1], s.mr[3], s.mr[4], s.mr[5]);
                }
                Now::Value(v)
            }
            Now::Busy => Now::Busy,
            Now::Missing => Now::Missing,
        };
        self.wired(n, force);
    }

    /// The USB Ethernet adapter (host mode), when net runs over it.
    fn wired(&mut self, n: u64, force: bool) {
        let Now::Value(v) = self.net else {
            self.eth = Eth::Missing;
            return;
        };
        if v.link != net::LINK_ETH0 {
            self.eth = Eth::Missing;
            return;
        }
        self.eth_name = "eth0";
        self.eth = if v.addr != 0 { Eth::Up } else { Eth::Dhcp };
        if self.eth_mac == 0 || force || n.is_multiple_of(SLOW) {
            if let Now::Value(m) = ask(&ETH0, eth::INFO) {
                self.eth_mac = m.mr[0] & 0xFFFF_FFFF_FFFF;
            }
        }
    }

    /// The network in use: its name, label and whether it has an address.
    pub fn active(&self) -> Option<Net> {
        match self.net {
            Now::Value(v) if v.link != 0 => Some(v),
            _ => None,
        }
    }
}

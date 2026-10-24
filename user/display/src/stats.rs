//! What the screen shows: kernel counters (`sysinfo`) and the services'
//! own. A missing service shows as such, a busy one keeps its last
//! values: a refresh never waits more than `CALL_NS` on one. The GPU's
//! come from its stats page, read without asking the service.

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::ETIMEDOUT;
use libnx::abi::info::{self, CpuInfo, MemInfo};
use libnx::abi::proto::{bt, cpufreq, eth, fan, fs, gpu, joycon, net, power, usbcon as usb, wifi};
use libnx::ipc::{self, Msg};
use libnx::ns::Service;
use libnx::seqpage::SeqPage;
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
static USB0: Service = Service::optional("usb0");
static FAN: Service = Service::optional("fan");
static CPUFREQ: Service = Service::optional("cpufreq");
static GPU: Service = Service::optional("gpu");
static BT: Service = Service::optional("bt");
static JOYCON: Service = Service::optional("joycon");

/// A service asked every refresh.
#[derive(Clone, Copy)]
pub enum Now<T> {
    /// Not published, or failed.
    Missing,
    /// No answer within `CALL_NS`.
    Busy,
    Value(T),
}

/// Ask `net` to take Wi-Fi first (`wired` false) or the wired links.
pub fn prefer_links(wired: bool) {
    let mode = if wired {
        net::PREF_WIRED
    } else {
        net::PREF_WIFI
    };
    let _ = NET.call_timeout(&Msg::from_words(net::PREFER, &[mode]), CALL_NS);
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

/// A paired Bluetooth controller (`bt::LIST`).
pub struct BtPad {
    pub addr: u64,
    /// `bt::KIND_*`.
    pub kind: u8,
    pub connected: bool,
    /// 0..4, or `bt::BATTERY_UNKNOWN`.
    pub battery: u8,
    pub name: String,
}

/// The Bluetooth adapter (`bt::STATUS`) and its controllers.
pub struct Bt {
    /// `bt::ST_*`.
    pub state: u64,
    /// What the adapter is, or why there is none.
    pub what: String,
    pub pads: Vec<BtPad>,
}

impl Bt {
    fn from_replies(st: &Msg, list: &Msg) -> Self {
        let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
        let n = ipc::unpack_at(st, 4, &mut buf);
        let what = String::from_utf8_lossy(&buf[..n]).into_owned();
        let count = ipc::header(list, 0) as usize;
        let n = ipc::unpack_at(list, 1, &mut buf);
        let mut pads = Vec::new();
        let mut at = 0;
        for _ in 0..count {
            let Some(rec) = buf[..n].get(at..at + bt::RECORD) else {
                break;
            };
            let len = rec[9] as usize;
            let Some(name) = buf[..n].get(at + bt::RECORD..at + bt::RECORD + len) else {
                break;
            };
            pads.push(BtPad {
                addr: rec[..6].iter().rev().fold(0, |w, &b| w << 8 | b as u64),
                kind: rec[6],
                connected: rec[7] != 0,
                battery: rec[8],
                name: String::from_utf8_lossy(name).into_owned(),
            });
            at += bt::RECORD + len;
        }
        Bt {
            state: ipc::header(st, 0),
            what,
            pads,
        }
    }
}

/// The battery as the fuel gauge and the charger report it.
#[derive(Clone, Copy)]
pub struct Battery {
    /// State of charge, in 1/256 %.
    pub soc: u64,
    /// Cell voltage (mV).
    pub mv: u64,
    /// Average current (µA): positive charging, negative discharging.
    pub ua: i64,
    /// Battery temperature (millidegrees C).
    pub mc: i64,
    /// Remaining and full charge (mAh).
    pub rep_mah: u64,
    pub full_mah: u64,
    /// `power::CHARGE_*`.
    pub charge: u64,
    /// Wear: age (%), cycles (1/100) and design capacity (mAh).
    pub age: u64,
    pub cycles: u64,
    pub design_mah: u64,
}

impl Battery {
    /// Charge level, 0..=100 %.
    pub fn percent(&self) -> u64 {
        (self.soc >> 8).min(100)
    }
}

#[derive(Clone, Copy)]
pub struct Fan {
    /// Duty, % of full speed.
    pub percent: u64,
    /// `fan::ST_*` bits.
    pub state: u64,
    /// Last speed measured (RPM).
    pub rpm: Option<u64>,
    /// Duty set by hand (%); none in auto.
    pub manual: Option<u64>,
    /// SoC temperature (millidegrees C).
    pub temp: Option<i64>,
}

impl Fan {
    /// A `fan::STATUS` (or `SET`) reply.
    pub fn from_reply(m: &Msg) -> Self {
        Self {
            percent: m.mr[0],
            state: m.mr[1],
            rpm: (m.mr[3] != fan::NO_RPM).then_some(m.mr[3]),
            manual: (m.mr[4] != fan::AUTO).then_some(m.mr[4]),
            temp: (m.mr[2] != fan::NO_TEMP).then_some(m.mr[2] as i64),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cpu {
    pub khz: u64,
    /// CPU rail (µV, 0: unreadable).
    pub uv: u64,
    pub speedo: u64,
    /// `cpufreq::ST_*` bits.
    pub state: u64,
    /// Steps this chip runs (bit i: `cpufreq::STEPS_MHZ[i]`).
    pub allowed: u64,
    /// Step set (MHz), `cpufreq::BOOT`.
    pub step: u64,
}

impl Cpu {
    /// A `cpufreq::STATUS` (or `SET`) reply.
    pub fn from_reply(m: &Msg) -> Self {
        Self {
            khz: m.mr[0],
            uv: m.mr[1],
            speedo: m.mr[2],
            state: m.mr[3],
            allowed: m.mr[4],
            step: m.mr[5],
        }
    }
}

/// The GPU as its stats page shows it.
#[derive(Clone, Copy)]
pub struct Gpu {
    /// `gpu::OFF`, `ON`, `FAILED`.
    pub state: u64,
    /// `gpu::STAGE_*` reached (powering up while `OFF`) or failed in.
    pub stage: u64,
    /// GPC clock (kHz, 0: off or PLL bypassed).
    pub khz: u64,
    /// Rail (µV, 0: off).
    pub uv: u64,
    /// Step set (kHz), applied at power-up while off.
    pub step: u64,
    /// The power source's cap (kHz) and `power::SUPPLY_*`.
    pub cap: u64,
    pub supply: u64,
    /// Clock measured after the last change (kHz, 0: not measured).
    pub measured: u64,
    /// `gpu::CK_*` bits.
    pub ck: u64,
    /// Steps this chip runs (bit i: `gpu::STEPS_KHZ[i]`), 0: unknown.
    pub steps: u64,
    /// Share of the time with a batch in flight since the last
    /// refresh (%); none while off.
    pub load: Option<u64>,
}

/// The gpu service's stats page, mapped read-only.
struct GpuPage {
    page: SeqPage,
    instance: u64,
    /// Previous sample: time and busy time (ns).
    prev: (u64, u64),
}

impl GpuPage {
    /// The GPU now, its load since the previous sample.
    fn sample(&mut self) -> Option<Gpu> {
        // Time first: a batch that starts after it adds nothing.
        let now = time::now_ns();
        let w = self.page.read::<{ gpu::STATS_WORDS }>()?;
        let running = match w[gpu::S_SINCE_NS] {
            0 => 0,
            t => now.saturating_sub(t),
        };
        let busy = w[gpu::S_BUSY_NS] + running;
        let (t0, b0) = core::mem::replace(&mut self.prev, (now, busy));
        let state = w[gpu::S_STATE] & 0xFF;
        let load = (state == gpu::ON)
            .then(|| (busy.saturating_sub(b0) * 100 / now.saturating_sub(t0).max(1)).min(100));
        Some(Gpu {
            state,
            stage: w[gpu::S_STATE] >> 8,
            khz: w[gpu::S_KHZ],
            uv: w[gpu::S_UV],
            step: w[gpu::S_STEP],
            cap: w[gpu::S_CAP],
            supply: w[gpu::S_SUPPLY],
            measured: w[gpu::S_MEASURED],
            ck: w[gpu::S_CK],
            steps: w[gpu::S_STEPS],
            load,
        })
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
    /// The network's name as it is (any bytes).
    pub fn raw(&self) -> &[u8] {
        &self.ssid[..self.len]
    }

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
    /// `net::PREF_*` (0: net does not say).
    pub pref: u64,
}

impl Net {
    pub fn link_name(&self) -> &'static str {
        match self.link {
            net::LINK_USB0 => "usb0",
            net::LINK_ETH0 => "eth0",
            net::LINK_WLAN0 => "wlan0",
            _ => "link",
        }
    }

    pub fn link_label(&self) -> &'static str {
        match self.link {
            net::LINK_USB0 => "USB-C Ethernet",
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
    /// A PC, which did not turn the network function on (no driver).
    Off,
    /// A link, but net runs on another: no DHCP answer, not shared.
    NoSharing,
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
    /// `power::SUPPLY_*`: battery, or input power on the USB-C port.
    pub supply: Seen<u64>,
    pub battery: Seen<Battery>,
    /// Asked every refresh: pairing and connections change quickly.
    pub bt: Seen<Bt>,
    /// The rails' Joy-Cons: (state `joycon::ST_*`, battery) left, right.
    pub rails: Seen<[(u64, u64); 2]>,
    /// Asked every refresh; busy while it measures its speed (1 s).
    pub fan: Seen<Fan>,
    pub cpu: Seen<Cpu>,
    /// From the stats page, current even while the service is busy.
    pub gpu: Seen<Gpu>,
    gpu_page: Option<GpuPage>,
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
            supply: Seen::Unknown,
            battery: Seen::Unknown,
            bt: Seen::Unknown,
            rails: Seen::Unknown,
            fan: Seen::Unknown,
            cpu: Seen::Unknown,
            gpu: Seen::Unknown,
            gpu_page: None,
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
        self.supply
            .poll(&POWER, power::SUPPLY, n, force, |m| m.mr[0]);
        self.battery_refresh(n, force);
        self.bt_refresh();
        self.rails.poll(&JOYCON, joycon::STATUS, n, true, |m| {
            [(m.mr[0], m.mr[2]), (m.mr[1], m.mr[3])]
        });
        self.fan.poll(&FAN, fan::STATUS, n, true, Fan::from_reply);
        self.cpu
            .poll(&CPUFREQ, cpufreq::STATUS, n, force, Cpu::from_reply);
        self.gpu_refresh(n, force);
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
                    pref: if i.words() > 4 { i.mr[4] } else { 0 },
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

    /// The adapter and its controllers. A busy service (bring-up, at
    /// boot) keeps what was seen.
    fn bt_refresh(&mut self) {
        match (ask(&BT, bt::STATUS), ask(&BT, bt::LIST)) {
            (Now::Value(st), Now::Value(l)) => self.bt = Seen::Value(Bt::from_replies(&st, &l)),
            (Now::Missing, _) if time::now_ns() > BOOT_NS => self.bt = Seen::Missing,
            _ => {}
        }
    }

    /// The fuel gauge: a gauge that does not answer (no battery, or
    /// not emulated) shows as missing.
    fn battery_refresh(&mut self, n: u64, force: bool) {
        if matches!(self.battery, Seen::Value(_)) && !force && !n.is_multiple_of(SLOW) {
            return;
        }
        let (b, h) = (
            ask(&POWER, power::BATTERY),
            ask(&POWER, power::BATTERY_HEALTH),
        );
        let (Now::Value(b), Now::Value(h)) = (b, h) else {
            if matches!((b, h), (Now::Busy, _) | (_, Now::Busy)) {
                return;
            }
            if time::now_ns() > BOOT_NS {
                self.battery = Seen::Missing;
            }
            return;
        };
        self.battery = Seen::Value(Battery {
            soc: b.mr[0],
            mv: b.mr[1],
            ua: b.mr[2] as i64,
            mc: b.mr[3] as i64,
            rep_mah: b.mr[4] & 0xFFFF_FFFF,
            full_mah: b.mr[4] >> 32,
            charge: b.mr[5],
            age: h.mr[0],
            cycles: h.mr[1],
            design_mah: h.mr[2],
        });
    }

    /// The GPU from its stats page, asked for with the slow sources: the
    /// service reads the power source again then, and a new instance
    /// (restarted) hands over a new page.
    fn gpu_refresh(&mut self, n: u64, force: bool) {
        if self.gpu_page.is_none() || force || n.is_multiple_of(SLOW) {
            let have = self.gpu_page.as_ref().map_or(0, |p| p.instance);
            match GPU.call_timeout(&Msg::from_words(gpu::STATS, &[have]), CALL_NS) {
                Ok(m) if m.handles() == 1 => {
                    let h = ipc::received_handle(0);
                    match SeqPage::map(h) {
                        Ok(page) => {
                            // Busy since the service started, at first.
                            let instance = m.mr[0];
                            let prev = (instance, 0);
                            self.gpu_page = Some(GpuPage {
                                page,
                                instance,
                                prev,
                            });
                        }
                        Err(_) => {
                            let _ = sys::handle_close(h);
                        }
                    }
                }
                // Same page, or busy: the page tells anyway.
                Ok(_) => {}
                Err(e) if e == -ETIMEDOUT => {}
                Err(_) => {
                    self.gpu_page = None;
                    if time::now_ns() > BOOT_NS {
                        self.gpu = Seen::Missing;
                    }
                }
            }
        }
        if let Some(g) = self.gpu_page.as_mut().and_then(GpuPage::sample) {
            self.gpu = Seen::Value(g);
        }
    }

    /// The wired link: the PC's connection over USB-C (usbcon's network
    /// function, "usb0"), or a USB Ethernet adapter net runs over ("eth0").
    fn wired(&mut self, n: u64, force: bool) {
        let net = match self.net {
            Now::Value(v) => Some(v),
            _ => None,
        };
        let on = |link: u64| net.filter(|v| v.link == link).map(|v| v.addr != 0);
        let (name, service) = match on(net::LINK_ETH0) {
            Some(addr) => {
                self.eth = if addr { Eth::Up } else { Eth::Dhcp };
                ("eth0", &ETH0)
            }
            None => {
                self.eth = match (self.usb, on(net::LINK_USB0)) {
                    (Now::Missing, _) => Eth::Missing,
                    (Now::Busy, _) => self.eth,
                    (Now::Value(u), _) if u.state & usb::ST_CONFIGURED == 0 => Eth::NoPc,
                    (Now::Value(u), _) if u.state & usb::ST_NET == 0 => Eth::Off,
                    (_, Some(true)) => Eth::Up,
                    (_, Some(false)) => Eth::Dhcp,
                    (_, None) => Eth::NoSharing,
                };
                ("usb0", &USB0)
            }
        };
        if self.eth_name != name {
            (self.eth_name, self.eth_mac) = (name, 0);
        }
        if self.eth != Eth::Missing && (self.eth_mac == 0 || force || n.is_multiple_of(SLOW)) {
            if let Now::Value(m) = ask(service, eth::INFO) {
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

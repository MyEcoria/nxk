//! The full-screen pages opened from the dock: Network, System (overview
//! and detail), Terminal and Logs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::{usbcon as usb, wifi};

use crate::font::{self, Face};
use crate::screen::{Bytes, Celsius, Hms, Ipv4, Screen, BAD, FG, TERM_BG};
use crate::stats::{Data, Eth, Now, Seen};
use crate::term::Term;
use crate::theme::pal;
use crate::ui::{
    cpu_text, eth_text, fan_text, gpu_text, usb_short, usb_status, wifi_connected, wifi_short, Id,
    Ui, CH, CY, LOGS, M, NETWORK, TERMINAL, W,
};
use crate::widgets::{
    badge, bar, bars_for, card, fit, kv, load_color, signal_bars, split_bar, titled, vc, Tone,
};
use crate::Messages;

/// A processor's load: name, bar and percentage, or off.
fn load_row(s: &mut Screen, x: usize, w: usize, y: usize, name: &str, load: Option<u64>) {
    let p = pal();
    s.text(
        x + 24,
        y,
        name,
        Face::Body,
        if load.is_some() { p.text } else { p.dim },
    );
    match load {
        Some(v) => {
            bar(s, x + 104, y + 6, w - 208, 12, v, load_color(v));
            s.text_r(x + w - 24, y, &format!("{}%", v), Face::Body, p.text);
        }
        None => {
            bar(s, x + 104, y + 6, w - 208, 12, 0, p.track);
            s.text_r(x + w - 24, y, "off", Face::Body, p.dim);
        }
    }
}

/// ACTIVE on the link net uses, STANDBY on another usable one; returns the width.
fn link_badge(s: &mut Screen, d: &Data, xr: usize, y: usize, name: &str, usable: bool) -> usize {
    let p = pal();
    match d.active() {
        Some(n) if n.link_name() == name => badge(s, xr, y, "ACTIVE", p.good),
        _ if usable => badge(s, xr, y, "STANDBY", p.dim),
        _ => 0,
    }
}

impl Ui {
    pub(crate) fn system(&mut self, s: &mut Screen, d: &Data) {
        self.page_head(s, "System");
        let tx = M + 260;
        self.chip(
            s,
            Id::SysTab(0),
            tx,
            30,
            148,
            40,
            "Overview",
            self.sys_tab == 0,
            true,
        );
        self.chip(
            s,
            Id::SysTab(1),
            tx + 160,
            30,
            148,
            40,
            "Details",
            self.sys_tab == 1,
            true,
        );
        if self.sys_tab == 0 {
            self.sys_overview(s, d);
        } else {
            self.sys_detail(s, d);
        }
        self.hint_bar(s, &[("B", "Back"), ("A", "OK")]);
    }

    /// Four summary cards: CPU, Memory, Storage, Connectivity.
    fn sys_overview(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let gap = 20usize;
        let (w, h) = ((W - 2 * M - gap) / 2, (CH - gap) / 2);
        let (xl, xr) = (M, M + w + gap);
        let (yt, yb) = (CY, CY + h + gap);

        // CPU.
        self.tile_card(s, Id::SysTab(1), xl, yt, w, h, "Processor");
        let on: Vec<u64> = d.load.iter().flatten().copied().collect();
        let avg = on.iter().sum::<u64>() / (on.len() as u64).max(1);
        s.text(
            xl + 24,
            yt + 60,
            &format!("{}%", avg),
            Face::Big,
            load_color(avg),
        );
        s.text(xl + 24, yt + 138, "average", Face::Cap, p.dim);
        for (k, v) in d.load.iter().enumerate() {
            let y = yt + 52 + k * 34;
            s.text(
                xl + 240,
                y,
                &format!("C{}", k),
                Face::Cap,
                if v.is_some() { p.text } else { p.dim },
            );
            match v {
                Some(pct) => {
                    bar(s, xl + 280, y + 6, w - 360, 10, *pct, load_color(*pct));
                    s.text_r(xl + w - 24, y, &format!("{}%", pct), Face::Cap, p.text);
                }
                None => {
                    bar(s, xl + 280, y + 6, w - 360, 10, 0, p.track);
                    s.text_r(xl + w - 24, y, "off", Face::Cap, p.dim);
                }
            }
        }
        let line = format!(
            "{} cores   {} processes   up {}",
            on.len(),
            d.procs,
            Hms(d.up_s)
        );
        s.text(xl + 24, yt + h - 40, &line, Face::Cap, p.dim);

        // Memory.
        titled(s, xr, yt, w, h, "Memory");
        if let Some(m) = d.mem {
            let b = |pages: u64| pages * PAGE_SIZE;
            let used = m.total_pages - m.free_pages;
            s.text_r(
                xr + w - 24,
                yt + 18,
                &format!("{}", Bytes(b(m.total_pages))),
                Face::Body,
                p.dim,
            );
            let mw = s.text(
                xr + 24,
                yt + 60,
                &format!("{}", Bytes(b(used))),
                Face::Big,
                p.text,
            );
            s.text(xr + 36 + mw, yt + 96, "used", Face::Body, p.dim);
            let user = used.saturating_sub(m.kernel_pages);
            split_bar(
                s,
                xr + 24,
                yt + 144,
                w - 48,
                14,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                xr + 24,
                xr + w / 2 - 8,
                yt + 174,
                "kernel",
                &format!("{}", Bytes(b(m.kernel_pages))),
                Some(p.accent),
            );
            kv(
                s,
                xr + w / 2 + 8,
                xr + w - 24,
                yt + 174,
                "programs",
                &format!("{}", Bytes(b(user))),
                Some(p.good),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                yt + 206,
                "free",
                &format!("{}", Bytes(b(m.free_pages))),
                Some(p.track),
            );
        }

        // Storage.
        titled(s, xl, yb, w, h, "Storage");
        match d.disk {
            Seen::Value((free, total)) => {
                let used = total.saturating_sub(free);
                let pct = used * 100 / total.max(1);
                s.text_r(xl + w - 24, yb + 18, "SD card", Face::Body, p.dim);
                let fw = s.text(
                    xl + 24,
                    yb + 60,
                    &format!("{}", Bytes(free)),
                    Face::Big,
                    p.text,
                );
                s.text(xl + 36 + fw, yb + 96, "free", Face::Body, p.dim);
                bar(
                    s,
                    xl + 24,
                    yb + 144,
                    w - 48,
                    14,
                    pct,
                    if pct >= 90 { p.bad } else { p.accent },
                );
                kv(
                    s,
                    xl + 24,
                    xl + w - 24,
                    yb + 174,
                    "used",
                    &format!("{} ({}%)", Bytes(used), pct),
                    None,
                );
                kv(
                    s,
                    xl + 24,
                    xl + w - 24,
                    yb + 206,
                    "total",
                    &format!("{}", Bytes(total)),
                    None,
                );
            }
            Seen::Missing => {
                s.text(xl + 24, yb + 76, "no SD card", Face::Body, p.dim);
            }
            Seen::Unknown => {
                s.text(xl + 24, yb + 76, "starting", Face::Body, p.dim);
            }
        }

        // Connectivity & console.
        self.tile_card(s, Id::OpenPage(NETWORK), xr, yb, w, h, "Links");
        let ip = match d.active() {
            Some(n) if n.addr != 0 => {
                format!("{}/{} via {}", Ipv4(n.addr), n.prefix, n.link_name())
            }
            Some(n) => format!("waiting for DHCP ({})", n.link_name()),
            None => "offline".into(),
        };
        s.text(xr + 24, yb + 64, &ip, Face::Title, p.text);
        let (wt, wc) = wifi_short(d);
        s.text(xr + 24, yb + 116, "Wi-Fi", Face::Body, p.dim);
        s.text_r(xr + w - 24, yb + 116, wt, Face::Body, wc);
        let (et, ec) = eth_text(d, true);
        s.text(xr + 24, yb + 150, "Ethernet", Face::Body, p.dim);
        s.text_r(xr + w - 24, yb + 150, et, Face::Body, ec);
        let (ut, uc) = usb_short(d);
        s.text(xr + 24, yb + 184, "USB console", Face::Body, p.dim);
        s.text_r(xr + w - 24, yb + 184, ut, Face::Body, uc);
    }

    /// Per-core load, clocks, activity, memory, temperatures, fan and power.
    fn sys_detail(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let gap = 20usize;
        let w = (W - 2 * M - gap) / 2;
        let (xl, xr) = (M, M + w + gap);
        titled(s, xl, CY, w, CH, "Processor");
        s.text_r(
            xl + w - 24,
            CY + 18,
            &format!("up {}", Hms(d.up_s)),
            Face::Body,
            p.dim,
        );
        for (k, v) in d.load.iter().enumerate() {
            load_row(s, xl, w, CY + 58 + k * 34, &format!("CPU{}", k), *v);
        }
        let gpu_load = match d.gpu {
            Seen::Value(g) => g.load,
            _ => None,
        };
        load_row(s, xl, w, CY + 194, "GPU", gpu_load);
        // The clock rows open the CPU and GPU sheets.
        let (t, c) = match d.cpu {
            Seen::Value(c) => cpu_text(&c),
            Seen::Missing => ("no clock control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(s, Id::CpuOpen, xl, w, CY + 236, "CPU clock", &t, c);
        let (t, c) = gpu_text(&d.gpu);
        self.open_row(s, Id::GpuOpen, xl, w, CY + 292, "GPU clock", &t, c);
        s.fill(xl + 24, CY + 354, w - 48, 1, p.line);
        s.text(xl + 24, CY + 368, "Activity", Face::BodyB, p.dim);
        s.text_r(
            xl + w - 24,
            CY + 368,
            &format!("{} processes", d.procs),
            Face::Body,
            p.dim,
        );
        let rows = [
            ("syscalls/s", d.rates[0]),
            ("interrupts/s", d.rates[1]),
            ("context switches/s", d.rates[2]),
        ];
        for (i, (n, v)) in rows.iter().enumerate() {
            kv(
                s,
                xl + 24,
                xl + w - 24,
                CY + 404 + i * 32,
                n,
                &format!("{}", v),
                None,
            );
        }

        let mh = 236usize;
        titled(s, xr, CY, w, mh, "Memory");
        if let Some(m) = d.mem {
            let b = |pages: u64| format!("{}", Bytes(pages * PAGE_SIZE));
            let used = m.total_pages - m.free_pages;
            let user = used.saturating_sub(m.kernel_pages);
            s.text_r(xr + w - 24, CY + 18, &b(m.total_pages), Face::Body, p.dim);
            split_bar(
                s,
                xr + 24,
                CY + 62,
                w - 48,
                18,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 102,
                "kernel",
                &b(m.kernel_pages),
                Some(p.accent),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 140,
                "programs",
                &b(user),
                Some(p.good),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 178,
                "free",
                &b(m.free_pages),
                Some(p.track),
            );
        }

        let (py, ph) = (CY + mh + gap, CH - mh - gap);
        titled(s, xr, py, w, ph, "Thermal & power");
        match d.temps {
            Seen::Value((chip, package)) => {
                kv(
                    s,
                    xr + 24,
                    xr + w - 24,
                    py + 56,
                    "chip temperature",
                    &format!("{}", Celsius(chip)),
                    None,
                );
                kv(
                    s,
                    xr + 24,
                    xr + w - 24,
                    py + 90,
                    "package temperature",
                    &format!("{}", Celsius(package)),
                    None,
                );
            }
            Seen::Missing => {
                s.text(xr + 24, py + 56, "no temperature sensor", Face::Body, p.dim);
            }
            Seen::Unknown => {
                s.text(xr + 24, py + 56, "starting", Face::Body, p.dim);
            }
        }
        let (t, c) = match d.fan {
            Seen::Value(f) => fan_text(&f),
            Seen::Missing => ("no fan control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(s, Id::FanOpen, xr, w, py + 128, "Fan", &t, c);
        let (bw, bh) = ((w - 64) / 2, 56);
        let by = py + ph - 20 - bh;
        self.button(
            s,
            Id::Reboot,
            xr + 24,
            by,
            bw,
            bh,
            "Restart",
            Tone::Warn,
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            xr + 40 + bw,
            by,
            bw,
            bh,
            "Power off",
            Tone::Bad,
            true,
        );
    }

    pub(crate) fn network(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        self.page_head(s, "Network");
        let gap = 20usize;
        let w = (W - 2 * M - gap) / 2;
        let (xl, xr) = (M, M + w + gap);
        titled(s, M, CY, W - 2 * M, 176, "Internet");
        match d.active() {
            Some(n) => {
                let t = format!(
                    "rx {}   tx {}   {} sockets",
                    Bytes(n.rx),
                    Bytes(n.tx),
                    n.sockets
                );
                s.text_r(W - M - 24, CY + 18, &t, Face::Body, p.dim);
                let dash = |a: u32| {
                    if a == 0 {
                        "-".into()
                    } else {
                        format!("{}", Ipv4(a))
                    }
                };
                if n.addr == 0 {
                    s.text(M + 24, CY + 68, "waiting for DHCP", Face::Title, p.warn);
                } else {
                    let iw = s.text(
                        M + 24,
                        CY + 58,
                        &format!("{}", Ipv4(n.addr)),
                        Face::Head,
                        p.good,
                    );
                    s.text(
                        M + 28 + iw,
                        CY + 74,
                        &format!("/{}", n.prefix),
                        Face::Body,
                        p.dim,
                    );
                    s.text(M + 24, CY + 122, "online", Face::Body, p.good);
                }
                let via = format!("{}, {}", n.link_name(), n.link_label());
                kv(s, xr + 24, W - M - 24, CY + 58, "via", &via, None);
                kv(
                    s,
                    xr + 24,
                    W - M - 24,
                    CY + 94,
                    "gateway",
                    &dash(n.gateway),
                    None,
                );
                kv(s, xr + 24, W - M - 24, CY + 130, "DNS", &dash(n.dns), None);
            }
            None => {
                let t = match d.net {
                    Now::Busy => "busy",
                    _ => "no sharing on USB-C, no Wi-Fi network",
                };
                s.text(M + 24, CY + 58, "offline", Face::Head, p.dim);
                s.text(M + 24, CY + 122, t, Face::Body, p.dim);
            }
        }

        let (y0, h) = (CY + 176 + gap, CH - 176 - gap);
        titled(s, xl, y0, w, h, "USB-C Ethernet");
        let name = if d.eth_name.is_empty() {
            "usb0"
        } else {
            d.eth_name
        };
        link_badge(s, d, xl + w - 24, y0 + 14, name, d.eth == Eth::Up);
        let (et, ec) = eth_text(d, false);
        s.text(xl + 24, y0 + 62, et, Face::Body, ec);
        if d.eth != Eth::Missing {
            let mac = d.eth_mac.to_le_bytes();
            let mac = if d.eth_mac == 0 {
                "-".into()
            } else {
                format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                )
            };
            let speed = match d.usb {
                Now::Value(u) if d.eth != Eth::NoPc && name == "usb0" => {
                    if u.state & usb::ST_HIGH_SPEED != 0 {
                        "high speed, 480 Mb/s"
                    } else {
                        "full speed, 12 Mb/s"
                    }
                }
                _ => "-",
            };
            kv(s, xl + 24, xl + w - 24, y0 + 108, "interface", name, None);
            kv(s, xl + 24, xl + w - 24, y0 + 144, "MAC", &mac, None);
            kv(s, xl + 24, xl + w - 24, y0 + 180, "USB", speed, None);
        }
        let hint = match d.eth {
            Eth::NoSharing => Some([
                "on the PC: share its connection",
                "(nmcli: ipv4.method shared)",
            ]),
            Eth::Off => Some(["the PC has no CDC-ECM driver", "(Linux and macOS have one)"]),
            Eth::NoPc => Some(["plug the Switch into a PC that", "shares its connection"]),
            _ => None,
        };
        for (i, t) in hint.iter().flatten().enumerate() {
            s.text(xl + 24, y0 + 228 + i * 32, t, Face::Body, p.dim);
        }

        titled(s, xr, y0, w, h, "Wi-Fi");
        let connected = wifi_connected(d);
        let bw = link_badge(s, d, xr + w - 24, y0 + 14, "wlan0", connected.is_some());
        let (by, btw) = (y0 + h - 20 - 56, (w - 80) / 3);
        let state = match d.wifi {
            Seen::Value(wf) => wf.state,
            _ => wifi::ST_DOWN,
        };
        let adapter = matches!(d.wifi, Seen::Value(_))
            && !matches!(state, wifi::ST_DOWN | wifi::ST_RADIO_OFF);
        if let Some(nets) = &self.scan {
            let t = format!("{} networks", nets.len());
            s.text_r(xr + w - 36 - bw, y0 + 18, &t, Face::Body, p.dim);
            for (i, n) in nets.iter().take(5).enumerate() {
                let y = y0 + 58 + i * 36;
                let cur = connected.is_some_and(|c| c.ssid() == n.ssid);
                let ssid = fit(Face::Body, &n.ssid, 220);
                s.text(
                    xr + 24,
                    y,
                    &ssid,
                    Face::Body,
                    if cur { p.good } else { p.text },
                );
                let sec = match n.security {
                    wifi::SEC_OPEN => "open",
                    wifi::SEC_WEP => "WEP",
                    wifi::SEC_WPA => "WPA",
                    wifi::SEC_WPA2 => "WPA2",
                    _ => "WPA3",
                };
                let t = format!("ch {:>2}   -{} dBm   {}", n.channel, n.signal, sec);
                s.text_r(xr + w - 24, y, &t, Face::Body, p.dim);
            }
            let busy = self.busy;
            self.button(
                s,
                Id::ScanBack,
                xr + 24,
                by,
                btw,
                56,
                "Back",
                Tone::Plain,
                true,
            );
            self.button(
                s,
                Id::Scan,
                xr + 40 + btw,
                by,
                btw,
                56,
                "Scan",
                Tone::Accent,
                !busy,
            );
        } else {
            if let Some(wf) = connected {
                signal_bars(
                    s,
                    xr + w - 36 - bw - 36,
                    y0 + 40,
                    bars_for(wf.signal),
                    p.good,
                );
            }
            let (t, c) = wifi_short(d);
            s.text(xr + 24, y0 + 62, t, Face::Body, c);
            if let Seen::Value(wf) = d.wifi {
                if matches!(wf.state, wifi::ST_CONNECTED | wifi::ST_CONNECTING) {
                    s.text(xr + 24, y0 + 96, wf.ssid(), Face::TitleB, p.text);
                    self.ssid = wf.ssid().into();
                }
                if wf.state == wifi::ST_CONNECTED {
                    kv(
                        s,
                        xr + 24,
                        xr + w - 24,
                        y0 + 150,
                        "channel",
                        &format!("{}", wf.channel),
                        None,
                    );
                    kv(
                        s,
                        xr + 24,
                        xr + w - 24,
                        y0 + 186,
                        "signal",
                        &format!("-{} dBm", wf.signal),
                        None,
                    );
                } else if wf.state == wifi::ST_IDLE {
                    s.text(
                        xr + 24,
                        y0 + 108,
                        "join: wifi connect SSID (shell)",
                        Face::Body,
                        p.dim,
                    );
                } else if wf.state == wifi::ST_RADIO_OFF {
                    s.text(xr + 24, y0 + 108, "Flight mode is on", Face::Body, p.dim);
                }
            }
            let (busy, joined) = (
                self.busy,
                matches!(state, wifi::ST_CONNECTED | wifi::ST_CONNECTING),
            );
            let scan_ok = adapter && state != wifi::ST_SCANNING && !busy;
            let retry_ok = !matches!(d.wifi, Seen::Missing | Seen::Unknown)
                && state != wifi::ST_RADIO_OFF
                && !busy;
            self.button(
                s,
                Id::Scan,
                xr + 24,
                by,
                btw,
                56,
                "Scan",
                Tone::Accent,
                scan_ok,
            );
            self.button(
                s,
                Id::Retry,
                xr + 40 + btw,
                by,
                btw,
                56,
                "Retry",
                Tone::Plain,
                retry_ok,
            );
            self.button(
                s,
                Id::Leave,
                xr + 56 + 2 * btw,
                by,
                btw,
                56,
                "Leave",
                Tone::Warn,
                joined && !busy,
            );
        }
        self.hint_bar(s, &[("B", "Back"), ("A", "OK")]);
    }

    pub(crate) fn terminal(&mut self, s: &mut Screen, d: &Data, term: &Term, now: u64) {
        let p = pal();
        self.page_head(s, "Terminal");
        let (x, y, w, h) = (M, CY, W - 2 * M, CH);
        s.card_round(x, y, w, h, 16, 1, p.line, TERM_BG);
        s.fill_round(x + 1, y + 1, w - 2, 56, 15, p.surface);
        s.fill(x + 1, y + 56, w - 2, 1, p.line);
        let (ut, uc) = usb_status(d);
        s.disc(x + 28, y + 28, 6, uc);
        s.text(
            x + 46,
            vc(Face::Body, y, 56),
            &ut,
            Face::Body,
            if uc == p.dim { p.dim } else { p.text },
        );
        let bx = x + w - 16 - 136;
        let label = if self.term_scale == 1 {
            "Text x2"
        } else {
            "Text x1"
        };
        self.button(
            s,
            Id::TermSize,
            bx,
            y + 8,
            136,
            40,
            label,
            Tone::Plain,
            true,
        );
        if let Now::Value(u) = d.usb {
            let t = format!("in {}   out {}", Bytes(u.rx), Bytes(u.tx));
            let tw = s.text_r(bx - 20, vc(Face::Body, y, 56), &t, Face::Body, p.dim);
            if u.errors != 0 {
                s.text_r(
                    bx - 40 - tw,
                    vc(Face::Body, y, 56),
                    &format!("{} errors", u.errors),
                    Face::Body,
                    BAD,
                );
            }
        }
        let sc = self.term_scale;
        let (x0, y0) = (x + 20, y + 70);
        let cols = (w - 40) / (8 * sc);
        let rows = (h - 70 - 16) / (16 * sc);
        let live = matches!(d.usb, Now::Value(u) if u.state & usb::ST_TERMINAL != 0);
        let notice = (!live).then(|| format!("-- {} --", ut));
        for (i, r) in term.rows(cols, rows, notice).iter().enumerate() {
            let ry = y0 + i * 16 * sc;
            let mut cx = x0;
            for (t, c) in &r.segs {
                cx += s.mono(cx, ry, t, sc, *c, TERM_BG);
            }
            let mut rx = x0 + cols * 8 * sc;
            for (t, c) in r.right.iter().rev() {
                rx -= s.mono_r(rx, ry, t, sc, *c, TERM_BG);
            }
            if r.cursor && (now / 500_000_000).is_multiple_of(2) {
                s.fill(cx, ry, 8 * sc, 16 * sc, FG);
            }
        }
        self.hint_bar(s, &[("B", "Back"), ("A", "OK")]);
        let _ = TERMINAL;
    }

    pub(crate) fn logs(&mut self, s: &mut Screen, msgs: &Messages) {
        let p = pal();
        self.unread = 0;
        self.page_head(s, "Logs");
        let (x, y, w, h) = (M, CY, W - 2 * M, CH);
        card(s, x, y, w, h);
        let cx = x + w - 20 - 116;
        let label = if self.log_scale == 1 {
            "Text x2"
        } else {
            "Text x1"
        };
        self.button(
            s,
            Id::LogClear,
            cx,
            y + 12,
            116,
            44,
            "Clear",
            Tone::Plain,
            !msgs.is_empty(),
        );
        self.button(
            s,
            Id::LogSize,
            cx - 12 - 136,
            y + 12,
            136,
            44,
            label,
            Tone::Plain,
            true,
        );
        let sc = self.log_scale;
        let cols = (w - 48) / (8 * sc);
        let rows = (h - 80) / (16 * sc);
        let mut wrapped: Vec<String> = Vec::with_capacity(rows + 8);
        for l in msgs.lines().iter().rev() {
            if wrapped.len() >= rows {
                break;
            }
            let mut parts: Vec<String> = Vec::new();
            let chars: Vec<char> = l.chars().collect();
            if chars.is_empty() {
                parts.push(String::new());
            }
            for chunk in chars.chunks(cols) {
                parts.push(chunk.iter().collect());
            }
            wrapped.extend(parts.into_iter().rev());
        }
        wrapped.reverse();
        if wrapped.is_empty() {
            s.text(x + 24, y + 72, "no messages", Face::Body, p.dim);
        }
        for (r, t) in wrapped[wrapped.len().saturating_sub(rows)..]
            .iter()
            .enumerate()
        {
            s.mono(x + 24, y + 68 + r * 16 * sc, t, sc, p.text, p.surface);
        }
        self.hint_bar(s, &[("B", "Back"), ("A", "OK")]);
        let _ = (LOGS, font::width);
    }
}

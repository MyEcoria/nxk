//! The pages of the tab row after Games: System (overview and details),
//! Network, Terminal and Logs, as cards on the black page.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::{usbcon as usb, wifi};

use crate::font::Face;
use crate::screen::{Bytes, Celsius, Hms, Ipv4, Screen, BAD, FG, TERM_BG};
use crate::stats::{Data, Eth, Now, Seen};
use crate::status::{
    cpu_text, eth_text, fan_text, gpu_text, usb_short, usb_status, wifi_connected, wifi_short,
};
use crate::term::Term;
use crate::theme::pal;
use crate::ui::{Id, Ui, CB, CH, CY, HINT_Y, M, NETWORK, W};
use crate::widgets::{
    badge, bar, bars_for, card, heading, kv, load_color, signal_bars, split_bar, titled, Tone,
};
use crate::Messages;

const GAP: usize = 16;

/// A processor's load centered on `cy`: name, bar and percentage, or off.
fn load_row(s: &mut Screen, x: usize, w: usize, cy: usize, name: &str, load: Option<u64>) {
    let p = pal();
    s.label(
        x + 24,
        cy,
        name,
        Face::Small,
        if load.is_some() { p.text } else { p.dim },
    );
    bar(
        s,
        x + 104,
        cy - 4,
        w - 208,
        8,
        load.unwrap_or(0),
        load.map_or(p.track, load_color),
    );
    match load {
        Some(v) => s.label_r(x + w - 24, cy, &format!("{}%", v), Face::Small, p.text),
        None => s.label_r(x + w - 24, cy, "off", Face::Small, p.dim),
    };
}

/// ACTIVE on the link net uses, STANDBY on another usable one; returns its width.
fn link_badge(s: &mut Screen, d: &Data, xr: usize, cy: usize, name: &str, usable: bool) -> usize {
    let p = pal();
    match d.active() {
        Some(n) if n.link_name() == name => badge(s, xr, cy, "ACTIVE", p.good),
        _ if usable => badge(s, xr, cy, "STANDBY", p.dim),
        _ => 0,
    }
}

/// A big value with its unit after it, centered on `cy`.
fn big(s: &mut Screen, x: usize, cy: usize, value: &str, unit: &str, c: u32) {
    let w = s.label(x, cy, value, Face::Head, c);
    s.label(x + w + 12, cy + 6, unit, Face::Small, pal().dim);
}

impl Ui {
    pub(crate) fn system(&mut self, s: &mut Screen, d: &Data) {
        if self.sys_tab == 0 {
            self.sys_overview(s, d);
        } else {
            self.sys_detail(s, d);
        }
        self.page_hints(s);
        let tx = W / 2 - 144;
        for (i, t) in ["Overview", "Details"].iter().enumerate() {
            let on = self.sys_tab == i;
            self.chip(
                s,
                Id::SysTab(i),
                tx + i * 148,
                HINT_Y - 18,
                136,
                36,
                t,
                on,
                true,
            );
        }
    }

    /// Four summary cards: processor, memory, storage, links.
    fn sys_overview(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let (w, h) = ((W - 2 * M - GAP) / 2, (CH - GAP) / 2);
        let (xl, xr) = (M, M + w + GAP);
        let (yt, yb) = (CY, CY + h + GAP);

        self.tile_card(s, Id::SysTab(1), xl, yt, w, h, "Processor");
        let on: Vec<u64> = d.load.iter().flatten().copied().collect();
        let avg = on.iter().sum::<u64>() / (on.len() as u64).max(1);
        s.label(
            xl + 24,
            yt + 100,
            &format!("{}%", avg),
            Face::Big,
            load_color(avg),
        );
        s.label(xl + 24, yt + 150, "average", Face::Cap, p.dim);
        for (k, v) in d.load.iter().enumerate() {
            let cy = yt + 66 + k * 30;
            s.label(
                xl + 220,
                cy,
                &format!("C{}", k),
                Face::Cap,
                if v.is_some() { p.text } else { p.dim },
            );
            bar(
                s,
                xl + 260,
                cy - 4,
                w - 340,
                8,
                v.unwrap_or(0),
                v.map_or(p.track, load_color),
            );
            match v {
                Some(pct) => s.label_r(xl + w - 24, cy, &format!("{}%", pct), Face::Cap, p.text),
                None => s.label_r(xl + w - 24, cy, "off", Face::Cap, p.dim),
            };
        }
        let line = format!(
            "{} cores \u{b7} {} processes \u{b7} up {}",
            on.len(),
            d.procs,
            Hms(d.up_s)
        );
        s.label(xl + 24, yt + h - 28, &line, Face::Cap, p.dim);

        titled(s, xr, yt, w, h, "Memory");
        if let Some(m) = d.mem {
            let b = |pages: u64| format!("{}", Bytes(pages * PAGE_SIZE));
            let used = m.total_pages - m.free_pages;
            let user = used.saturating_sub(m.kernel_pages);
            s.label_r(xr + w - 24, yt + 30, &b(m.total_pages), Face::Small, p.dim);
            big(s, xr + 24, yt + 88, &b(used), "used", p.text);
            split_bar(
                s,
                xr + 24,
                yt + 128,
                w - 48,
                10,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                xr + 24,
                xr + w / 2 - 12,
                yt + 166,
                "kernel",
                &b(m.kernel_pages),
                Some(p.accent),
            );
            kv(
                s,
                xr + w / 2 + 12,
                xr + w - 24,
                yt + 166,
                "programs",
                &b(user),
                Some(p.good),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                yt + 198,
                "free",
                &b(m.free_pages),
                Some(p.track),
            );
        }

        titled(s, xl, yb, w, h, "Storage");
        match d.disk {
            Seen::Value((free, total)) => {
                let used = total.saturating_sub(free);
                let pct = used * 100 / total.max(1);
                s.label_r(xl + w - 24, yb + 30, "SD card", Face::Small, p.dim);
                big(
                    s,
                    xl + 24,
                    yb + 88,
                    &format!("{}", Bytes(free)),
                    "free",
                    p.text,
                );
                bar(
                    s,
                    xl + 24,
                    yb + 128,
                    w - 48,
                    10,
                    pct,
                    if pct >= 90 { p.bad } else { p.accent },
                );
                let u = format!("{} ({}%)", Bytes(used), pct);
                kv(s, xl + 24, xl + w - 24, yb + 166, "used", &u, None);
                kv(
                    s,
                    xl + 24,
                    xl + w - 24,
                    yb + 198,
                    "total",
                    &format!("{}", Bytes(total)),
                    None,
                );
            }
            Seen::Missing => {
                s.label(xl + 24, yb + 88, "no SD card", Face::Body, p.dim);
            }
            Seen::Unknown => {
                s.label(xl + 24, yb + 88, "starting", Face::Body, p.dim);
            }
        }

        self.tile_card(s, Id::OpenPage(NETWORK), xr, yb, w, h, "Links");
        let ip = match d.active() {
            Some(n) if n.addr != 0 => {
                format!("{}/{} via {}", Ipv4(n.addr), n.prefix, n.link_name())
            }
            Some(n) => format!("waiting for DHCP ({})", n.link_name()),
            None => "offline".into(),
        };
        s.label(xr + 24, yb + 80, &ip, Face::Title, p.text);
        let (wt, wc) = wifi_short(d);
        let (et, ec) = eth_text(d, true);
        let (ut, uc) = usb_short(d);
        for (i, (k, v, c)) in [
            ("Wi-Fi", wt, wc),
            ("Ethernet", et, ec),
            ("USB console", ut, uc),
        ]
        .into_iter()
        .enumerate()
        {
            let cy = yb + 132 + i * 30;
            s.label(xr + 24, cy, k, Face::Small, p.dim);
            s.label_r(xr + w - 24, cy, v, Face::Small, c);
        }
    }

    /// Per-core load, clocks, activity, memory, temperatures, fan and power.
    fn sys_detail(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let w = (W - 2 * M - GAP) / 2;
        let (xl, xr) = (M, M + w + GAP);
        titled(s, xl, CY, w, CH, "Processor");
        s.label_r(
            xl + w - 24,
            CY + 30,
            &format!("up {}", Hms(d.up_s)),
            Face::Small,
            p.dim,
        );
        for (k, v) in d.load.iter().enumerate() {
            load_row(s, xl, w, CY + 66 + k * 30, &format!("CPU{}", k), *v);
        }
        let gpu_load = match d.gpu {
            Seen::Value(g) => g.load,
            _ => None,
        };
        load_row(s, xl, w, CY + 186, "GPU", gpu_load);
        // The clock rows open the CPU and GPU sheets.
        let (t, c) = match d.cpu {
            Seen::Value(c) => cpu_text(&c),
            Seen::Missing => ("no clock control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(
            s,
            Id::CpuOpen,
            xl + 16,
            w - 32,
            CY + 214,
            "CPU clock",
            &t,
            c,
        );
        let (t, c) = gpu_text(&d.gpu);
        self.open_row(
            s,
            Id::GpuOpen,
            xl + 16,
            w - 32,
            CY + 270,
            "GPU clock",
            &t,
            c,
        );
        s.fill(xl + 24, CY + 334, w - 48, 1, p.line);
        heading(s, xl + 24, CY + 358, "Activity");
        s.label_r(
            xl + w - 24,
            CY + 358,
            &format!("{} processes", d.procs),
            Face::Small,
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
                CY + 392 + i * 30,
                n,
                &format!("{}", v),
                None,
            );
        }

        let mh = 210usize;
        titled(s, xr, CY, w, mh, "Memory");
        if let Some(m) = d.mem {
            let b = |pages: u64| format!("{}", Bytes(pages * PAGE_SIZE));
            let used = m.total_pages - m.free_pages;
            let user = used.saturating_sub(m.kernel_pages);
            s.label_r(xr + w - 24, CY + 30, &b(m.total_pages), Face::Small, p.dim);
            split_bar(
                s,
                xr + 24,
                CY + 62,
                w - 48,
                12,
                m.total_pages,
                m.kernel_pages,
                user,
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 110,
                "kernel",
                &b(m.kernel_pages),
                Some(p.accent),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 142,
                "programs",
                &b(user),
                Some(p.good),
            );
            kv(
                s,
                xr + 24,
                xr + w - 24,
                CY + 174,
                "free",
                &b(m.free_pages),
                Some(p.track),
            );
        }

        let (py, ph) = (CY + mh + GAP, CH - mh - GAP);
        titled(s, xr, py, w, ph, "Thermal & power");
        match d.temps {
            Seen::Value((chip, package)) => {
                kv(
                    s,
                    xr + 24,
                    xr + w - 24,
                    py + 66,
                    "chip temperature",
                    &format!("{}", Celsius(chip)),
                    None,
                );
                let t = format!("{}", Celsius(package));
                kv(
                    s,
                    xr + 24,
                    xr + w - 24,
                    py + 96,
                    "package temperature",
                    &t,
                    None,
                );
            }
            Seen::Missing => {
                s.label(
                    xr + 24,
                    py + 66,
                    "no temperature sensor",
                    Face::Small,
                    p.dim,
                );
            }
            Seen::Unknown => {
                s.label(xr + 24, py + 66, "starting", Face::Small, p.dim);
            }
        }
        let (t, c) = match d.fan {
            Seen::Value(f) => fan_text(&f),
            Seen::Missing => ("no fan control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(s, Id::FanOpen, xr + 16, w - 32, py + 118, "Fan", &t, c);
        let (bw, bh) = ((w - 64) / 2, 52);
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
            "Power Off",
            Tone::Bad,
            true,
        );
    }

    pub(crate) fn network(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let w = (W - 2 * M - GAP) / 2;
        let (xl, xr) = (M, M + w + GAP);
        let ih = 150;
        titled(s, M, CY, W - 2 * M, ih, "Internet");
        match d.active() {
            Some(n) => {
                let t = format!(
                    "rx {}   tx {}   {} sockets",
                    Bytes(n.rx),
                    Bytes(n.tx),
                    n.sockets
                );
                s.label_r(W - M - 24, CY + 30, &t, Face::Small, p.dim);
                let dash = |a: u32| {
                    if a == 0 {
                        "-".into()
                    } else {
                        format!("{}", Ipv4(a))
                    }
                };
                if n.addr == 0 {
                    s.label(M + 24, CY + 86, "waiting for DHCP", Face::Title, p.warn);
                } else {
                    big(
                        s,
                        M + 24,
                        CY + 86,
                        &format!("{}", Ipv4(n.addr)),
                        &format!("/{}", n.prefix),
                        p.good,
                    );
                    s.label(M + 24, CY + 126, "online", Face::Small, p.good);
                }
                let via = format!("{}, {}", n.link_name(), n.link_label());
                kv(s, xr + 24, W - M - 24, CY + 66, "via", &via, None);
                kv(
                    s,
                    xr + 24,
                    W - M - 24,
                    CY + 96,
                    "gateway",
                    &dash(n.gateway),
                    None,
                );
                kv(s, xr + 24, W - M - 24, CY + 126, "DNS", &dash(n.dns), None);
            }
            None => {
                let t = match d.net {
                    Now::Busy => "busy",
                    _ => "no sharing on USB-C, no Wi-Fi network",
                };
                s.label(M + 24, CY + 82, "offline", Face::Head, p.dim);
                s.label(M + 24, CY + 126, t, Face::Small, p.dim);
            }
        }

        let (y0, h) = (CY + ih + GAP, CB - CY - ih - GAP);
        titled(s, xl, y0, w, h, "USB-C Ethernet");
        let name = if d.eth_name.is_empty() {
            "usb0"
        } else {
            d.eth_name
        };
        link_badge(s, d, xl + w - 24, y0 + 30, name, d.eth == Eth::Up);
        let (et, ec) = eth_text(d, false);
        s.label(xl + 24, y0 + 72, et, Face::Small, ec);
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
            kv(s, xl + 24, xl + w - 24, y0 + 114, "interface", name, None);
            kv(s, xl + 24, xl + w - 24, y0 + 144, "MAC", &mac, None);
            kv(s, xl + 24, xl + w - 24, y0 + 174, "USB", speed, None);
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
            s.label(xl + 24, y0 + 226 + i * 28, t, Face::Small, p.dim);
        }

        titled(s, xr, y0, w, h, "Wi-Fi");
        let connected = wifi_connected(d);
        let bw = link_badge(s, d, xr + w - 24, y0 + 30, "wlan0", connected.is_some());
        let (by, btw) = (y0 + h - 20 - 52, (w - 80) / 3);
        let state = match d.wifi {
            Seen::Value(wf) => wf.state,
            _ => wifi::ST_DOWN,
        };
        let adapter = matches!(d.wifi, Seen::Value(_))
            && !matches!(state, wifi::ST_DOWN | wifi::ST_RADIO_OFF);
        let busy = self.busy;
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
        s.label(xr + 24, y0 + 72, t, Face::Small, c);
        if let Seen::Value(wf) = d.wifi {
            if matches!(wf.state, wifi::ST_CONNECTED | wifi::ST_CONNECTING) {
                s.label(xr + 24, y0 + 112, wf.ssid(), Face::TitleB, p.text);
                self.ssid = wf.ssid().into();
            }
            if wf.state == wifi::ST_CONNECTED {
                kv(
                    s,
                    xr + 24,
                    xr + w - 24,
                    y0 + 156,
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
                s.label(
                    xr + 24,
                    y0 + 108,
                    "Networks lists what is in range",
                    Face::Small,
                    p.dim,
                );
            } else if wf.state == wifi::ST_RADIO_OFF {
                s.label(xr + 24, y0 + 108, "Flight Mode is on", Face::Small, p.dim);
            }
        }
        let joined = matches!(state, wifi::ST_CONNECTED | wifi::ST_CONNECTING);
        let list_ok = adapter && !busy;
        let retry_ok = !matches!(d.wifi, Seen::Missing | Seen::Unknown)
            && state != wifi::ST_RADIO_OFF
            && !busy;
        self.button(
            s,
            Id::WifiOpen,
            xr + 24,
            by,
            btw,
            52,
            "Networks",
            Tone::Accent,
            list_ok,
        );
        self.button(
            s,
            Id::Retry,
            xr + 40 + btw,
            by,
            btw,
            52,
            "Retry",
            Tone::Plain,
            retry_ok,
        );
        let lx = xr + 56 + 2 * btw;
        self.button(
            s,
            Id::Leave,
            lx,
            by,
            btw,
            52,
            "Leave",
            Tone::Warn,
            joined && !busy,
        );
        self.page_hints(s);
    }

    pub(crate) fn terminal(&mut self, s: &mut Screen, d: &Data, term: &Term, now: u64) {
        let p = pal();
        let (x, y, w, h) = (M, CY, W - 2 * M, CH);
        s.fill_round(x, y, w, h, 8, TERM_BG);
        s.fill_round(x, y, w, 56, 8, p.surface);
        s.fill(x, y + 48, w, 8, p.surface);
        let (ut, uc) = usb_status(d);
        s.disc(x + 28, y + 28, 6, uc);
        s.label(
            x + 46,
            y + 28,
            &ut,
            Face::Small,
            if uc == p.dim { p.dim } else { p.text },
        );
        let bx = x + w - 12 - 136;
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
            let tw = s.label_r(bx - 20, y + 28, &t, Face::Small, p.dim);
            if u.errors != 0 {
                s.label_r(
                    bx - 40 - tw,
                    y + 28,
                    &format!("{} errors", u.errors),
                    Face::Small,
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
        self.page_hints(s);
    }

    pub(crate) fn logs(&mut self, s: &mut Screen, msgs: &Messages) {
        let p = pal();
        self.unread = 0;
        let (x, y, w, h) = (M, CY, W - 2 * M, CH);
        card(s, x, y, w, h);
        heading(s, x + 24, y + 30, "Service and kernel messages");
        let cx = x + w - 12 - 116;
        let label = if self.log_scale == 1 {
            "Text x2"
        } else {
            "Text x1"
        };
        self.button(
            s,
            Id::LogClear,
            cx,
            y + 10,
            116,
            40,
            "Clear",
            Tone::Plain,
            !msgs.is_empty(),
        );
        self.button(
            s,
            Id::LogSize,
            cx - 12 - 136,
            y + 10,
            136,
            40,
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
            let chars: Vec<char> = l.chars().collect();
            let mut parts: Vec<String> = chars.chunks(cols).map(|c| c.iter().collect()).collect();
            if parts.is_empty() {
                parts.push(String::new());
            }
            wrapped.extend(parts.into_iter().rev());
        }
        wrapped.reverse();
        if wrapped.is_empty() {
            s.label(x + 24, y + 90, "no messages", Face::Body, p.dim);
        }
        for (r, t) in wrapped[wrapped.len().saturating_sub(rows)..]
            .iter()
            .enumerate()
        {
            s.mono(x + 24, y + 68 + r * 16 * sc, t, sc, p.text, p.surface);
        }
        self.page_hints(s);
    }
}

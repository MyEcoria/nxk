//! System Settings: the categories on the left with a vertical rule, the
//! chosen category's rows on the right, as on the Switch 2.

use alloc::format;
use alloc::string::String;

use libnx::abi::layout::PAGE_SIZE;
use libnx::abi::proto::{bt, power};

use crate::backlight::MIN_PCT;
use crate::font::Face;
use crate::screen::{logo_size, Bytes, Hms, Ipv4, Screen};
use crate::stats::{Battery, Data, Seen};
use crate::status::{cpu_text, eth_text, fan_text, gpu_text, wifi_short};
use crate::theme::pal;
use crate::ui::{Id, Ui, HEADER_RULE_Y, LOGS, M, NETWORK, RULE_Y, SYSTEM, TERMINAL, W};
use crate::widgets::{battery, icon, press, slider, switch, Icon, Tone};

pub const CATS: [&str; 8] = [
    "Display",
    "Sound",
    "Internet",
    "Controllers",
    "Performance",
    "Power",
    "Battery",
    "System",
];
const SLEEP_LABELS: [&str; 5] = ["Never", "1 min", "5 min", "10 min", "30 min"];
/// The left category sidebar, and the chosen category's right column.
const LW: usize = 356;
const X: usize = M + LW + 44;
const XR: usize = W - M;
/// First row's center line in the right pane.
const Y: usize = 224;

/// A row: its label on the left, and its value on the right.
fn row(s: &mut Screen, cy: usize, label: &str, value: &str, c: u32) {
    let p = pal();
    s.label(X, cy, label, Face::Body, p.text);
    s.label_r(XR, cy, value, Face::Body, c);
}

fn rule(s: &mut Screen, y: usize) {
    s.fill(X, y, XR - X, 1, pal().line);
}

/// The color of the charge level: red when low and not charging.
fn battery_color(b: &Battery, charging: bool) -> u32 {
    let p = pal();
    match b.percent() {
        _ if charging => p.good,
        0..=15 => p.bad,
        16..=30 => p.warn,
        _ => p.text,
    }
}

impl Ui {
    pub(crate) fn settings(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        for (i, name) in CATS.iter().enumerate() {
            let y = 120 + i * 64;
            let on = self.cat == i;
            let down = self.pressed == Some(Id::Cat(i));
            if on || down {
                s.fill_round(M, y, LW, 56, 6, press(p.surface, down));
            }
            if on {
                s.fill_round(M + 10, y + 12, 5, 32, 2, p.accent);
            }
            let (f, c) = if on {
                (Face::BodyB, p.accent)
            } else {
                (Face::Body, p.dim)
            };
            s.label(M + 32, y + 28, name, f, c);
            self.zone(Id::Cat(i), M, y, LW, 56, 6);
        }
        s.fill(
            M + LW + 22,
            HEADER_RULE_Y,
            1,
            RULE_Y - HEADER_RULE_Y,
            p.rule,
        );
        s.label(X, 162, CATS[self.cat], Face::TitleB, p.text);
        match self.cat {
            0 => self.cat_display(s),
            1 => self.cat_sound(s),
            2 => self.cat_network(s, d),
            3 => self.cat_controllers(s, d),
            4 => self.cat_performance(s, d),
            5 => self.cat_power(s, d),
            6 => self.cat_battery(s, d),
            _ => self.cat_system(s, d),
        }
        self.page_hints(s);
    }

    fn cat_sound(&mut self, s: &mut Screen) {
        let p = pal();
        let vol = self.cfg.volume;
        let (vtext, vc) = if vol == 0 {
            ("Muted".into(), p.warn)
        } else {
            (format!("{}%", vol), p.good)
        };
        row(s, Y, "Speaker Volume", &vtext, vc);
        icon(s, Icon::Speaker, X + 14, Y + 56, p.text, p.bg);
        slider(s, X + 52, Y + 56, XR - X - 196, vol, true);
        self.zone(Id::Volume, X + 32, Y + 30, XR - X - 176, 52, 8);
        self.chip(
            s,
            Id::Mute,
            XR - 120,
            Y + 34,
            120,
            44,
            if vol == 0 { "Unmute" } else { "Mute" },
            vol == 0,
            true,
        );
        rule(s, Y + 104);
        s.label(X, Y + 140, "Sound Test", Face::Body, p.text);
        s.label_r(
            XR,
            Y + 140,
            "48 kHz 16-bit stereo via ALC5639 (I2S1)",
            Face::Cap,
            p.dim,
        );
        let bw = (XR - X - 16) / 2;
        self.button(
            s,
            Id::PlayMusic,
            X,
            Y + 174,
            bw,
            56,
            "Play Test Melody",
            Tone::Accent,
            true,
        );
        self.button(
            s,
            Id::PlayTone,
            X + bw + 16,
            Y + 174,
            bw,
            56,
            "440 Hz Reference Tone",
            Tone::Plain,
            true,
        );
    }

    fn cat_display(&mut self, s: &mut Screen) {
        let p = pal();
        s.label(X, Y, "Theme", Face::Body, p.text);
        let light = self.cfg.light;
        self.chip(
            s,
            Id::Theme(false),
            XR - 292,
            Y - 22,
            140,
            44,
            "Basic Black",
            !light,
            true,
        );
        self.chip(
            s,
            Id::Theme(true),
            XR - 140,
            Y - 22,
            140,
            44,
            "Basic White",
            light,
            true,
        );
        rule(s, Y + 42);
        let pct = self.cfg.brightness.max(MIN_PCT);
        row(s, Y + 76, "Screen Brightness", &format!("{}%", pct), p.good);
        let span = (pct - MIN_PCT) as usize * 100 / (100 - MIN_PCT) as usize;
        icon(s, Icon::Sun, X + 14, Y + 130, p.text, p.bg);
        slider(s, X + 52, Y + 130, XR - X - 72, span as u8, true);
        self.zone(Id::Bright, X + 32, Y + 104, XR - X - 32, 52, 8);
        rule(s, Y + 174);
        s.label(X, Y + 208, "Sleep after", Face::Body, p.text);
        s.label_r(
            XR,
            Y + 208,
            "Screen off when nothing is touched",
            Face::Cap,
            p.dim,
        );
        let bw = (XR - X - 4 * 12) / SLEEP_LABELS.len();
        for (i, &label) in SLEEP_LABELS.iter().enumerate() {
            let on = self.cfg.sleep as usize == i;
            self.chip(
                s,
                Id::AutoSleep(i),
                X + i * (bw + 12),
                Y + 238,
                bw,
                44,
                label,
                on,
                true,
            );
        }
    }

    fn cat_network(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        s.label(X, Y, "Flight Mode", Face::Body, p.text);
        s.label(
            X,
            Y + 32,
            "Turns the Wi-Fi radio off: no scan, no join",
            Face::Cap,
            p.dim,
        );
        switch(s, XR - 56, Y - 6, self.cfg.flight);
        self.zone(Id::Flight, X - 8, Y - 30, XR - X + 16, 76, 6);
        rule(s, Y + 62);
        let (wt, wc) = wifi_short(d);
        row(s, Y + 96, "Wi-Fi", wt, wc);
        let (et, ec) = eth_text(d, true);
        row(s, Y + 140, "USB-C Ethernet", et, ec);
        let ip: String = match d.active() {
            Some(n) if n.addr != 0 => format!("{}/{}", Ipv4(n.addr), n.prefix),
            Some(_) => "waiting for DHCP".into(),
            None => "offline".into(),
        };
        row(s, Y + 184, "IP Address", &ip, p.dim);
        self.button(
            s,
            Id::OpenPage(NETWORK),
            X,
            Y + 228,
            320,
            56,
            "Open the Network page",
            Tone::Accent,
            true,
        );
    }

    fn cat_controllers(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        self.bt_pairing = false;
        self.bt_pads.clear();
        let b = match &d.bt {
            Seen::Value(b) => b,
            Seen::Unknown => {
                s.label(X, Y, "Starting Bluetooth...", Face::Body, p.dim);
                return;
            }
            Seen::Missing => {
                row(s, Y, "Bluetooth", "no service", p.warn);
                return;
            }
        };
        let (st, sc) = match b.state {
            bt::ST_READY => ("on", p.good),
            bt::ST_PAIRING => ("looking for controllers", p.accent),
            bt::ST_STARTING => ("starting", p.dim),
            _ => ("no adapter", p.warn),
        };
        row(s, Y, "Bluetooth", st, sc);
        // The chip and its firmware, or why there is no adapter.
        let what: String = b
            .what
            .split(';')
            .next()
            .unwrap_or("")
            .chars()
            .take(84)
            .collect();
        s.label(X, Y + 32, &what, Face::Cap, p.dim);
        let up = b.state == bt::ST_READY || b.state == bt::ST_PAIRING;
        self.bt_pairing = b.state == bt::ST_PAIRING;
        self.button(
            s,
            Id::BtPair,
            X,
            Y + 64,
            320,
            56,
            if self.bt_pairing {
                "Stop Pairing"
            } else {
                "Pair a Controller"
            },
            if self.bt_pairing {
                Tone::Warn
            } else {
                Tone::Accent
            },
            up,
        );
        let hint = if self.bt_pairing {
            "Hold the sync button: on the rail side of a Joy-Con,"
        } else {
            "Joy-Con and Pro Controller: pair once, then any"
        };
        let hint2 = if self.bt_pairing {
            "next to the USB-C port of a Pro Controller."
        } else {
            "button press reconnects them."
        };
        s.label(X + 344, Y + 82, hint, Face::Cap, p.dim);
        s.label(X + 344, Y + 106, hint2, Face::Cap, p.dim);
        rule(s, Y + 140);
        if b.pads.is_empty() {
            s.label(X, Y + 176, "No controller paired yet.", Face::Body, p.dim);
            return;
        }
        const ROWS: usize = 4;
        for (i, pad) in b.pads.iter().take(ROWS).enumerate() {
            let cy = Y + 176 + i * 52;
            s.label(X, cy, &pad.name, Face::Body, p.text);
            let state = match (pad.connected, pad.battery) {
                (true, bt::BATTERY_UNKNOWN) => "connected".into(),
                (true, b) => format!("connected, battery {}/4", b),
                (false, _) => String::from("paired"),
            };
            s.label_r(
                XR - 140,
                cy,
                &state,
                Face::Small,
                if pad.connected { p.good } else { p.dim },
            );
            self.bt_pads.push((pad.addr, pad.name.clone()));
            self.chip(
                s,
                Id::BtForget(i),
                XR - 120,
                cy - 20,
                120,
                40,
                "Forget",
                false,
                true,
            );
            rule(s, cy + 26);
        }
        if b.pads.len() > ROWS {
            s.label(
                X,
                Y + 176 + ROWS * 52,
                &format!("and {} more (bt list in the shell)", b.pads.len() - ROWS),
                Face::Cap,
                p.dim,
            );
        }
    }

    fn cat_performance(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let w = XR - X;
        let (t, c) = match d.cpu {
            Seen::Value(c) => cpu_text(&c),
            Seen::Missing => ("no clock control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(s, Id::CpuOpen, X, w, Y - 24, "CPU clock", &t, c);
        let (t, c) = gpu_text(&d.gpu);
        self.open_row(s, Id::GpuOpen, X, w, Y + 40, "GPU clock", &t, c);
        let (t, c) = match d.fan {
            Seen::Value(f) => fan_text(&f),
            Seen::Missing => ("no fan control".into(), p.dim),
            Seen::Unknown => ("starting".into(), p.dim),
        };
        self.open_row(s, Id::FanOpen, X, w, Y + 104, "Fan", &t, c);
        s.label(
            X,
            Y + 196,
            "Clocks and the fan fall back to safe values when",
            Face::Cap,
            p.dim,
        );
        s.label(X, Y + 222, "the SoC is hot.", Face::Cap, p.dim);
    }

    fn cat_power(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let src = match d.supply {
            Seen::Value(power::SUPPLY_EXTERNAL) => "input power (USB-C)",
            Seen::Value(_) => "battery",
            _ => "unknown",
        };
        row(s, Y, "Power source", src, p.dim);
        rule(s, Y + 38);
        let bw = (XR - X - 32) / 3;
        let by = Y + 66;
        self.button(
            s,
            Id::SleepNow,
            X,
            by,
            bw,
            64,
            "Sleep Mode",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::Reboot,
            X + bw + 16,
            by,
            bw,
            64,
            "Restart",
            Tone::Warn,
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            X + 2 * (bw + 16),
            by,
            bw,
            64,
            "Power Off",
            Tone::Bad,
            true,
        );
        s.label(
            X,
            Y + 172,
            "Sleep Mode turns the screen off and slows the CPU;",
            Face::Cap,
            p.dim,
        );
        s.label(
            X,
            Y + 198,
            "touch or a button wakes it. Power off and restart go",
            Face::Cap,
            p.dim,
        );
        s.label(
            X,
            Y + 224,
            "back to RCM: inject nxk again from the PC.",
            Face::Cap,
            p.dim,
        );
    }

    fn cat_battery(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let charging = matches!(d.supply, Seen::Value(power::SUPPLY_EXTERNAL));
        let b = match d.battery {
            Seen::Value(b) => b,
            Seen::Unknown => {
                s.label(X, Y, "Reading the fuel gauge...", Face::Body, p.dim);
                return;
            }
            Seen::Missing => {
                row(s, Y, "Fuel gauge", "no answer", p.warn);
                s.label(
                    X,
                    Y + 44,
                    "The MAX17050 on I2C1 did not reply: no battery",
                    Face::Cap,
                    p.dim,
                );
                s.label(X, Y + 70, "level or health to show.", Face::Cap, p.dim);
                return;
            }
        };
        let pct = b.percent();
        let level_c = battery_color(&b, charging);
        battery(s, XR - 20, 162, Some(pct), p.text, level_c);
        s.label_r(XR - 52, 162, &format!("{}%", pct), Face::TitleB, level_c);
        let bar = XR - X;
        s.fill_round(X, Y - 36, bar, 14, 7, p.track);
        let fw = (bar * pct as usize / 100).max(14);
        s.fill_round(X, Y - 36, fw, 14, 7, level_c);
        let (st, sc) = match (charging, b.charge) {
            (true, power::CHARGE_PRE | power::CHARGE_FAST) if b.ua < -50_000 => {
                ("input too weak, still draining", p.warn)
            }
            (true, power::CHARGE_PRE) => ("charging (pre-charge)", p.good),
            (true, power::CHARGE_FAST) => ("charging", p.good),
            (true, power::CHARGE_DONE) => ("fully charged", p.good),
            (true, _) => ("on input power, not charging", p.warn),
            (false, _) => ("discharging", p.dim),
        };
        let mah = |v: u64| format!("{} mAh", v);
        let cur = {
            let (sign, a) = if b.ua < 0 { ("-", -b.ua) } else { ("+", b.ua) };
            format!("{}{} mA", sign, a / 1000)
        };
        let temp = {
            let (sign, a) = if b.mc < 0 { ("-", -b.mc) } else { ("", b.mc) };
            format!("{}{}.{} \u{b0}C", sign, a / 1000, a % 1000 / 100)
        };
        let rows: [(&str, String, u32); 7] = [
            ("Status", st.into(), sc),
            (
                "Charge",
                format!("{} of {}", mah(b.rep_mah), mah(b.full_mah)),
                p.dim,
            ),
            (
                "Cell voltage",
                format!("{}.{:03} V", b.mv / 1000, b.mv % 1000),
                p.dim,
            ),
            ("Current", cur, p.dim),
            (
                "Temperature",
                temp,
                if b.mc >= 45_000 { p.warn } else { p.dim },
            ),
            (
                "Health",
                format!("{}% of the {} design capacity", b.age, mah(b.design_mah)),
                if b.age < 80 { p.warn } else { p.dim },
            ),
            (
                "Charge cycles",
                format!("{}.{:02}", b.cycles / 100, b.cycles % 100),
                p.dim,
            ),
        ];
        for (i, (k, v, c)) in rows.iter().enumerate() {
            let cy = Y + 24 + i * 48;
            row(s, cy, k, v, *c);
            rule(s, cy + 24);
        }
    }

    fn cat_system(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let (lw, lh) = logo_size();
        s.logo(XR - lw * 2, 162 - lh, 2);
        let rows: [(&str, String); 6] = [
            ("System", "nxk, AArch64 microkernel".into()),
            ("SoC", "NVIDIA Tegra X1 (4x Cortex-A57)".into()),
            ("Uptime", format!("{}", Hms(d.up_s))),
            (
                "Memory",
                d.mem.map_or("-".into(), |m| {
                    format!("{}", Bytes(m.total_pages * PAGE_SIZE))
                }),
            ),
            (
                "SD card",
                match d.disk {
                    Seen::Value((_, total)) => format!("{}", Bytes(total)),
                    _ => "none".into(),
                },
            ),
            ("Screen", {
                let (w, h) = s.size();
                format!(
                    "{}x{}{}",
                    w,
                    h,
                    if s.turned() { ", panel turned" } else { "" }
                )
            }),
        ];
        for (i, (k, v)) in rows.iter().enumerate() {
            let cy = Y - 6 + i * 48;
            row(s, cy, k, v, p.dim);
            rule(s, cy + 24);
        }
        let bw = (XR - X - 32) / 3;
        let by = Y + 308;
        self.button(
            s,
            Id::OpenPage(SYSTEM),
            X,
            by,
            bw,
            56,
            "System Monitor",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::OpenPage(TERMINAL),
            X + bw + 16,
            by,
            bw,
            56,
            "USB-C Terminal",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::OpenPage(LOGS),
            X + 2 * (bw + 16),
            by,
            bw,
            56,
            "System Logs",
            Tone::Plain,
            true,
        );
    }
}

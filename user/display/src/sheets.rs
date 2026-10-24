//! Sheets over the dimmed page (fan, CPU and GPU clocks, power) and the
//! confirmation of what cannot be undone, in the drawer's colors.

use alloc::format;
use alloc::string::String;

use libnx::abi::proto::{cpufreq, fan, gpu, power};

use crate::font::Face;
use crate::screen::{Celsius, Screen};
use crate::stats::Seen;
use crate::status::{cpu_text, gpu_mhz, gpu_text, mhz};
use crate::theme::{pal, BLACK};
use crate::ui::{Id, Ui, H, W};
use crate::widgets::{bar, tone_color, Tone};

impl Ui {
    /// A sheet of `w` x `h` centered over the dimmed page, with its title;
    /// returns its place.
    pub(crate) fn frame(
        &mut self,
        s: &mut Screen,
        title: &str,
        w: usize,
        h: usize,
    ) -> (usize, usize, usize, usize) {
        let p = pal();
        s.fill_alpha(0, 0, W, H, p.scrim, 160);
        self.zone(Id::Scrim, 0, 0, W, H, 0);
        let (x, y) = ((W - w) / 2, (H - h) / 2);
        s.shadow(x, y, w, h, 10, BLACK);
        s.fill_round(x, y, w, h, 10, p.surface);
        s.label(x + 40, y + 50, title, Face::TitleB, p.text);
        (x, y, w, h)
    }

    fn sheet_frame(&mut self, s: &mut Screen, title: &str) -> (usize, usize, usize, usize) {
        self.frame(s, title, 960, 592)
    }

    fn close_button(&mut self, s: &mut Screen, x: usize, y: usize, w: usize, h: usize) {
        self.button(
            s,
            Id::SheetClose,
            x + w - 40 - 220,
            y + h - 40 - 60,
            220,
            60,
            "Close",
            Tone::Plain,
            true,
        );
    }

    pub(crate) fn fan_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "Fan");
        let Seen::Value(f) = self.fan else {
            let t = if matches!(self.fan, Seen::Missing) {
                "no fan control"
            } else {
                "starting"
            };
            s.label(x + 40, y + 130, t, Face::Body, p.dim);
            return self.close_button(s, x, y, w, h);
        };
        let mut state = String::new();
        if let Some(t) = f.temp {
            state = format!("SoC {}", Celsius(t));
        }
        if let Some(rpm) = f.rpm {
            state = format!("{}   {} rpm", state, rpm);
        }
        s.label_r(x + w - 40, y + 50, &state, Face::Small, p.dim);
        let manual = self.fan_manual();
        let bw = (w - 104) / 2;
        self.chip(
            s,
            Id::FanAuto,
            x + 40,
            y + 108,
            bw,
            56,
            "Auto",
            manual.is_none(),
            true,
        );
        self.chip(
            s,
            Id::FanManual,
            x + 64 + bw,
            y + 108,
            bw,
            56,
            "Manual",
            manual.is_some(),
            true,
        );
        let forced = f.state & (fan::ST_HOT | fan::ST_NO_SENSOR) != 0;
        let shown = manual.unwrap_or(f.percent);
        let can = |ok: bool| manual.is_some() && ok;
        self.button(
            s,
            Id::FanDown,
            x + 40,
            y + 208,
            160,
            120,
            "-10%",
            Tone::Plain,
            can(shown > 0),
        );
        let up_x = x + w - 40 - 160;
        self.button(
            s,
            Id::FanUp,
            up_x,
            y + 208,
            160,
            120,
            "+10%",
            Tone::Plain,
            can(shown < 100),
        );
        let c = if forced { p.warn } else { p.text };
        s.label_c(x + w / 2, y + 260, &format!("{}%", shown), Face::Big, c);
        bar(
            s,
            x + 240,
            y + 316,
            w - 480,
            8,
            f.percent,
            if forced { p.warn } else { p.good },
        );
        let (note, nc) = if f.state & fan::ST_HOT != 0 {
            ("SoC hot: full speed until below 57\u{b0}C", p.warn)
        } else if f.state & fan::ST_NO_SENSOR != 0 {
            ("No temperature: at least 43%", p.warn)
        } else if manual.is_some() {
            ("Full speed anyway from 60\u{b0}C", p.dim)
        } else {
            ("Follows the SoC temperature", p.dim)
        };
        s.label(x + 40, y + 384, note, Face::Small, nc);
        self.close_button(s, x, y, w, h);
    }

    /// Steps in rows of six from `y`: each a chip.
    fn step_grid(&self, x: usize, y: usize, w: usize, i: usize) -> (usize, usize, usize, usize) {
        let (cols, gap) = (6, 14);
        let (bw, bh) = ((w - 80 - (cols - 1) * gap) / cols, 64);
        (
            x + 40 + (i % cols) * (bw + gap),
            y + (i / cols) * (bh + gap),
            bw,
            bh,
        )
    }

    pub(crate) fn cpu_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "CPU clock");
        match self.cpu {
            Seen::Value(c) if c.state & cpufreq::ST_UNSUPPORTED == 0 => {
                let (t, tone) = cpu_text(&c);
                s.label_r(x + w - 40, y + 50, &t, Face::Small, tone);
                let mut top = 0;
                for (i, &m) in cpufreq::STEPS_MHZ.iter().enumerate() {
                    let (bx, by, bw, bh) = self.step_grid(x, y + 108, w, i);
                    let runs = c.allowed & 1 << i != 0;
                    if runs {
                        top = m;
                    }
                    let on = runs && !self.cpu_sent;
                    self.chip(
                        s,
                        Id::CpuStep(i),
                        bx,
                        by,
                        bw,
                        bh,
                        &format!("{}", m),
                        c.step == m,
                        on,
                    );
                }
                let by = y + 108 + 2 * 78 + 8;
                let boot = c.step == cpufreq::BOOT;
                self.chip(
                    s,
                    Id::CpuBoot,
                    x + 40,
                    by,
                    200,
                    56,
                    "Boot",
                    boot,
                    !self.cpu_sent,
                );
                s.label(x + 264, by + 28, "the loader's clock", Face::Small, p.dim);
                let (note, nc) = if c.state & cpufreq::ST_HOT != 0 {
                    ("SoC hot: back to Boot", p.warn)
                } else {
                    ("Back to Boot at 80\u{b0}C", p.dim)
                };
                s.label(x + 40, y + 412, note, Face::Small, nc);
                let t = format!("speedo {}: {} MHz max", c.speedo, top);
                s.label(x + 40, y + 446, &t, Face::Small, p.dim);
            }
            Seen::Value(c) => {
                s.label(
                    x + 40,
                    y + 130,
                    &format!("{} MHz", mhz(c.khz)),
                    Face::Head,
                    p.text,
                );
                s.label(
                    x + 40,
                    y + 190,
                    "Boot clock only on this console",
                    Face::Body,
                    p.dim,
                );
            }
            Seen::Missing | Seen::Unknown => {
                let t = if matches!(self.cpu, Seen::Missing) {
                    "no clock control"
                } else {
                    "starting"
                };
                s.label(x + 40, y + 130, t, Face::Body, p.dim);
            }
        }
        self.close_button(s, x, y, w, h);
    }

    pub(crate) fn gpu_sheet(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = self.sheet_frame(s, "GPU clock");
        match self.gpu {
            Seen::Value(g) if g.steps != 1 => {
                let (t, tone) = gpu_text(&self.gpu);
                s.label_r(x + w - 40, y + 50, &t, Face::Small, tone);
                let mut top = 0;
                for (i, &k) in gpu::STEPS_KHZ.iter().enumerate() {
                    let (bx, by, bw, bh) = self.step_grid(x, y + 108, w, i);
                    if g.steps & 1 << i != 0 {
                        top = k;
                    }
                    let runs = g.steps == 0 || g.steps & 1 << i != 0;
                    let (set, on) = (g.step == k, runs && !self.gpu_sent);
                    self.chip(s, Id::GpuStep(i), bx, by, bw, bh, &gpu_mhz(k), set, on);
                    if k > g.cap && !set && on {
                        s.ring_round(bx, by, bw, bh, 6, 2, p.warn);
                    }
                }
                let source = match g.supply {
                    power::SUPPLY_EXTERNAL => "on input power",
                    gpu::NO_SUPPLY => "(charger unread)",
                    _ => "on battery",
                };
                let forced = g.ck & gpu::CK_FORCED != 0;
                let mut cap = format!("cap {} MHz {}", gpu_mhz(g.cap), source);
                if forced {
                    cap.push_str(", forced past it");
                }
                s.label(
                    x + 40,
                    y + 292,
                    &cap,
                    Face::Small,
                    if forced { p.warn } else { p.text },
                );
                let clock: String = match (g.state, g.measured) {
                    (gpu::ON, 0) => "not measured yet".into(),
                    (gpu::ON, m) => format!("measured {} MHz after the last change", gpu_mhz(m)),
                    _ => "off: the step applies at power-up".into(),
                };
                s.label(x + 40, y + 326, &clock, Face::Small, p.dim);
                let (note, nc) = if g.ck & gpu::CK_HOT != 0 {
                    ("SoC hot: back to 76.8 MHz", p.warn)
                } else if g.ck & gpu::CK_CAPPED != 0 {
                    ("Power source changed: down to its cap", p.warn)
                } else {
                    ("Back to 76.8 MHz at 80\u{b0}C", p.dim)
                };
                s.label(x + 40, y + 360, note, Face::Small, nc);
                if top != 0 {
                    let t = format!("{} MHz max on this chip", gpu_mhz(top));
                    s.label(x + 40, y + h - 70, &t, Face::Small, p.dim);
                }
            }
            Seen::Value(_) => {
                s.label(x + 40, y + 130, "76.8 MHz", Face::Head, p.text);
                s.label(
                    x + 40,
                    y + 190,
                    "Lowest step only on this console",
                    Face::Body,
                    p.dim,
                );
            }
            Seen::Missing | Seen::Unknown => {
                let t = if matches!(self.gpu, Seen::Missing) {
                    "no GPU service"
                } else {
                    "starting"
                };
                s.label(x + 40, y + 130, t, Face::Body, p.dim);
            }
        }
        self.close_button(s, x, y, w, h);
    }

    pub(crate) fn power_sheet(&mut self, s: &mut Screen) {
        let (x, y, w, _) = self.frame(s, "Power", 560, 440);
        let bw = w - 80;
        self.button(
            s,
            Id::SleepNow,
            x + 40,
            y + 104,
            bw,
            64,
            "Sleep Mode",
            Tone::Plain,
            true,
        );
        self.button(
            s,
            Id::Reboot,
            x + 40,
            y + 184,
            bw,
            64,
            "Restart",
            Tone::Warn,
            true,
        );
        self.button(
            s,
            Id::PowerOff,
            x + 40,
            y + 264,
            bw,
            64,
            "Power Off",
            Tone::Bad,
            true,
        );
        self.button(
            s,
            Id::SheetClose,
            x + 40,
            y + 352,
            bw,
            56,
            "Cancel",
            Tone::Plain,
            true,
        );
    }

    pub(crate) fn modal(&mut self, s: &mut Screen) {
        let Some(m) = self.modal.take() else { return };
        let p = pal();
        let tc = match m.tone {
            Tone::Plain => p.text,
            t => tone_color(t),
        };
        let (x, y, w, h) = self.frame(s, "", 800, 340);
        s.label(x + 40, y + 50, &m.title, Face::TitleB, tc);
        for (i, t) in m.lines.iter().enumerate() {
            s.label(x + 40, y + 116 + i * 38, t, Face::Body, p.text);
        }
        let (bw, by) = ((w - 104) / 2, y + h - 40 - 64);
        self.button(s, Id::No, x + 40, by, bw, 64, "Cancel", Tone::Plain, true);
        self.button(s, Id::Yes, x + 64 + bw, by, bw, 64, m.ok, m.tone, true);
        self.modal = Some(m);
    }
}

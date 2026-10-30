//! TOOLS: utilities of the console itself. The first is the game card
//! reader: the cartridge switch, and a probe that powers the reader's
//! ASIC and shows how far its bring-up gets. Reading the cartridge needs
//! Nintendo's signed ASIC firmware and the console's keys, which nxk does
//! not have (loading that firmware burns OTP fuses), so the page says so.

use alloc::format;
use alloc::string::String;

use libnx::abi::proto::gc;

use crate::font::Face;
use crate::screen::Screen;
use crate::stats::{Data, Reader, Seen};
use crate::theme::pal;
use crate::ui::{Id, Ui, CY, M, W};
use crate::widgets::{card, heading, Tone};

/// The bring-up steps a probe reports, in order.
const STEPS: [(u64, &str); 5] = [
    (gc::ST_RAIL, "ASIC rail up (LDO3, 3.1 V)"),
    (gc::ST_RESET, "Reset released, SDMMC2 clocked"),
    (gc::ST_HOST, "Host: 8-bit bus, 1.8 V, HS100"),
    (gc::ST_TUNED, "Tuning found a tap: the ASIC answers"),
    (gc::ST_STATUS, "Status command answered"),
];

fn present_text(r: &Reader) -> (&'static str, bool) {
    match r.present {
        gc::PRESENT_YES => ("Game card inserted", true),
        gc::PRESENT_NO => ("No game card", false),
        _ => ("Game card switch unreadable", false),
    }
}

impl Ui {
    pub(crate) fn tools_page(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let Seen::Value(r) = d.gc else {
            let text = match d.gc {
                Seen::Missing => "The game card reader service is not running.",
                _ => "Asking the game card reader...",
            };
            s.label(M, CY + 28, text, Face::Body, p.dim);
            return;
        };
        let (text, inserted) = present_text(&r);
        let w = W - 2 * M;
        card(s, M, CY, w, 92);
        s.disc(M + 36, CY + 46, 9, if inserted { p.good } else { p.faint });
        s.label(M + 62, CY + 46, "Game card", Face::BodyB, p.text);
        s.label(M + 260, CY + 46, text, Face::Body, p.dim);
        self.button(
            s,
            Id::GcProbe,
            W - M - 24 - 280,
            CY + 18,
            280,
            56,
            if self.gc_probing {
                "Probing..."
            } else {
                "Probe the reader"
            },
            Tone::Accent,
            !self.gc_probing,
        );

        let y = CY + 92 + 16;
        let h = 300;
        card(s, M, y, w, h);
        heading(s, M + 24, y + 30, "Reader bring-up");
        if !r.probed() {
            s.label(
                M + 24,
                y + 84,
                "Not probed yet. The ASIC stays unpowered until you probe it.",
                Face::Body,
                p.dim,
            );
        } else {
            for (i, (bit, name)) in STEPS.iter().enumerate() {
                let cy = y + 66 + i * 34;
                let done = r.stages & bit != 0;
                s.disc(M + 36, cy, 7, if done { p.good } else { p.faint });
                s.label(
                    M + 62,
                    cy,
                    name,
                    Face::Body,
                    if done { p.text } else { p.dim },
                );
            }
            let mut line = format!(
                "Rail as found: cfg {:#04x}, cfg2 {:#04x}, fps {:#04x}. {} ms.",
                r.rail & 0xFF,
                r.rail >> 8 & 0xFF,
                r.rail >> 16 & 0xFF,
                r.ms
            );
            if let Some(st) = r.status {
                line = format!("{} Status {:#010x}.", line, st);
            }
            s.label(M + 24, y + 66 + 5 * 34 + 14, &line, Face::Small, p.dim);
            if r.error != 0 {
                let why = format!("Stopped: {}", libnx::abi::err::name(r.error));
                s.label(M + 24, y + 66 + 5 * 34 + 46, &why, Face::Small, p.bad);
            }
        }

        let y2 = y + h + 16;
        card(s, M, y2, w, 60);
        heading(s, M + 24, y2 + 20, "Dump the card's information");
        let note: String = "Needs the reader's signed firmware and this console's keys; loading the firmware burns OTP fuses, so nxk does not.".into();
        s.label(M + 24, y2 + 42, &note, Face::Small, p.dim);
        self.page_hints(s);
    }
}

//! The drawer on the right, as the Switch's quick settings: Sleep Mode,
//! brightness, volume, Flight Mode, System Settings.

use crate::backlight::MIN_PCT;
use crate::font::Face;
use crate::screen::Screen;
use crate::stats::Data;
use crate::theme::{pal, BLACK};
use crate::ui::{Id, Ui, H, W};
use crate::widgets::{icon, press, slider, Icon};

/// The drawer's left edge, its content's.
const X: usize = 797;
const L: usize = 822;
const R: usize = W - 31;
/// Rows' text, inset from the content's edge, and the sliders' track.
const TX: usize = L + 23;
const SX: usize = 893;
const IX: usize = 860;

impl Ui {
    pub(crate) fn quick_panel(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        s.fill_alpha(0, 0, X, H, p.scrim, 60);
        self.zone(Id::Scrim, 0, 0, X, H, 0);
        for i in 0..12 {
            s.fill_alpha(X - 12 + i, 0, 1, H, BLACK, 4 + i * 6);
        }
        s.fill(X, 0, W - X, H, p.surface);
        self.status(s, d, W - 32, crate::ui::STATUS_Y, p.surface);

        // Sleep Mode.
        let (y, h) = (95, 76);
        let bg = press(p.raised, self.pressed == Some(Id::QSleep));
        s.fill_round(L, y, R - L, h, 6, bg);
        icon(s, Icon::Power, IX, y + h / 2, p.text, bg);
        s.label(SX, y + h / 2, "Sleep Mode", Face::Body, p.text);
        self.zone(Id::QSleep, L, y, R - L, h, 6);

        // No light sensor read yet: brightness is set by hand.
        s.label(TX, 213, "Automatic Brightness", Face::Body, p.text);
        s.label_r(R - 24, 213, "Off", Face::Body, p.dim);

        let pct = self.cfg.brightness.max(MIN_PCT);
        let span = (pct - MIN_PCT) as usize * 100 / (100 - MIN_PCT) as usize;
        icon(s, Icon::Sun, IX, 292, p.text, p.surface);
        slider(s, SX, 292, R - 23 - SX, span as u8, true);
        self.zone(Id::QBright, SX - 20, 292 - 26, R - 23 - SX + 40, 52, 8);

        // The codec's volume is not controlled yet: shown, not movable.
        icon(s, Icon::Speaker, IX, 372, p.faint, p.surface);
        slider(s, SX, 372, R - 23 - SX, 50, false);

        let fy = 451;
        s.label(TX, fy, "Flight Mode", Face::Body, p.text);
        let (t, c) = if self.cfg.flight {
            ("On", p.good)
        } else {
            ("Off", p.dim)
        };
        s.label_r(R - 24, fy, t, Face::Body, c);
        s.label(
            TX,
            504,
            "If you turn flight mode on, you will no longer",
            Face::Cap,
            p.dim,
        );
        s.label(
            TX,
            531,
            "be able to use wireless controllers.",
            Face::Cap,
            p.dim,
        );
        self.zone(Id::QFlight, L, fy - 30, R - L, 126, 6);

        let sy = 584;
        if self.pressed == Some(Id::QSettings) {
            s.fill_round(L, sy - 26, R - L, 52, 6, p.raised);
        }
        icon(s, Icon::Sliders, IX, sy, p.text, p.surface);
        s.label(SX, sy, "System Settings", Face::Body, p.text);
        self.zone(Id::QSettings, L, sy - 26, R - L, 52, 6);

        self.hints(s, L - 1, R, p.surface, &[("B", "Back"), ("A", "OK")]);
    }
}

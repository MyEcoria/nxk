//! The Switch 2 home screen: a horizontal row of square game icons with
//! a cyan selection frame and the selected game's title above it, and a
//! centered row of round system buttons underneath.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{input, power};

use crate::art;
use crate::font::{self, Face};
use crate::games::GameInfo;
use crate::screen::Screen;
use crate::stats::{Data, Seen};
use crate::status::usb_short;
use crate::theme::{pal, BLACK, WHITE};
use crate::ui::{Action, Id, Ui, LOGS, NETWORK, SETTINGS, SYSTEM, TERMINAL, W};
use crate::widgets::{fit, icon, Icon};

/// The round buttons under the game row, in `art::button` / `art::head` order.
pub const BUTTONS: [&str; 6] = [
    "System",
    "Network",
    "Terminal",
    "Logs",
    "System Settings",
    "Power",
];

/// Minimum number of square slots shown in the game row (empty slots fill
/// the rest, as on the Switch).
const MIN_SLOTS: usize = 4;
/// Left edge of the first square icon when the row is at rest.
const X0: usize = 104;
/// Top and size of the square game icons, their corner radius, the gap
/// between them, and the cyan selection frame around the chosen one.
const ICON_Y: usize = 166;
const ICON: usize = 256;
const ICON_R: usize = 14;
const GAP: usize = 22;
const FRAME: usize = 8;
const FRAME_T: usize = 5;
/// Center line of the selected game's title and play time above the row.
const TITLE_Y: usize = 126;
/// The centered row of round buttons underneath (`BUTTONS`).
const BTN: usize = 76;
const BTN_GAP: usize = 28;
const BTN_CY: usize = 512;
const BTN_LABEL_Y: usize = 592;

impl Ui {
    /// What A or a tap on round button `k` opens.
    pub(crate) fn open_tile(&mut self, k: usize) -> Option<Action> {
        match k {
            0 => self.go(SYSTEM),
            1 => self.go(NETWORK),
            2 => self.go(TERMINAL),
            3 => self.go(LOGS),
            4 => self.go(SETTINGS),
            _ => self.open_power(),
        }
        None
    }

    /// D-pad and A on the home screen: moves along the game row, drops to
    /// the round buttons on Down (or past the last game), and returns to
    /// the game row on Up.
    pub(crate) fn home_pad(&mut self, code: u64) -> Option<Option<Action>> {
        let n = self.games.len();
        let btn = self.home.checked_sub(n);
        match (code, btn) {
            (input::BTN_LEFT, None) if self.home > 0 => {
                self.home -= 1;
                self.last_game = self.home;
            }
            (input::BTN_RIGHT, None) if self.home + 1 < n => {
                self.home += 1;
                self.last_game = self.home;
            }
            (input::BTN_RIGHT, None) => {
                self.home = n;
            }
            (input::BTN_DOWN, None) => {
                // Map the visible game slot to the nearest round button.
                let k = (self.home * BUTTONS.len() / n.max(1)).min(BUTTONS.len() - 1);
                self.home = n + k;
            }
            (input::BTN_LEFT, Some(k)) if k > 0 => self.home -= 1,
            (input::BTN_LEFT, Some(0)) if n > 0 => {
                self.home = n - 1;
                self.last_game = self.home;
            }
            (input::BTN_RIGHT, Some(k)) if k + 1 < BUTTONS.len() => self.home += 1,
            (input::BTN_UP, Some(_)) if n > 0 => {
                self.home = self.last_game.min(n - 1);
            }
            (input::BTN_A, None) if n > 0 => return Some(Some(Action::Launch(self.home))),
            (input::BTN_A, Some(k)) => return Some(self.open_tile(k)),
            (input::BTN_LEFT | input::BTN_RIGHT | input::BTN_UP | input::BTN_DOWN, _) => {}
            _ => return None,
        }
        Some(None)
    }

    /// Short status shown next to the selected round button's name.
    fn button_info(&self, k: usize, d: &Data) -> String {
        match k {
            0 => {
                let on: Vec<u64> = d.load.iter().flatten().copied().collect();
                let avg = on.iter().sum::<u64>() / (on.len() as u64).max(1);
                format!("{} cores \u{b7} {}% load", on.len(), avg)
            }
            1 => match d.active() {
                Some(n) if n.addr != 0 => format!("online \u{b7} {}", n.link_label()),
                Some(_) => "waiting for an address".into(),
                None => "offline".into(),
            },
            2 => format!("USB-C console: {}", usb_short(d).0),
            3 => match self.unread {
                0 => "no new messages".into(),
                1 => "1 new message".into(),
                n => format!("{} new messages", n),
            },
            4 => format!(
                "{} theme \u{b7} brightness {}%",
                if self.cfg.light { "white" } else { "black" },
                self.cfg.brightness.max(crate::backlight::MIN_PCT)
            ),
            _ => match d.supply {
                Seen::Value(power::SUPPLY_EXTERNAL) => "on input power".into(),
                Seen::Value(_) => "on battery".into(),
                _ => "sleep, restart, power off".into(),
            },
        }
    }

    pub(crate) fn home_page(&mut self, s: &mut Screen, d: &Data) {
        let p = pal();
        let n = self.games.len();
        if n == 0 && self.home < BUTTONS.len() {
            self.home = 0;
        } else {
            self.home = self.home.min(n + BUTTONS.len() - 1);
        }
        let sel_btn = if n == 0 {
            Some(self.home.min(BUTTONS.len() - 1))
        } else {
            self.home.checked_sub(n)
        };
        if sel_btn.is_none() {
            self.last_game = self.home;
        }
        let active_game = self.last_game.min(n.saturating_sub(1));
        let slots = n.max(MIN_SLOTS);
        let sel_x = X0 + active_game * (ICON + GAP);
        let end = X0 + slots * ICON + slots.saturating_sub(1) * GAP;
        self.scroll_to = (sel_x - X0).min(end.saturating_sub(W - X0)) as f32;
        self.scroll += (self.scroll_to - self.scroll) * 0.35;
        if (self.scroll - self.scroll_to).abs() <= 0.5 {
            self.scroll = self.scroll_to;
        }
        let off = self.scroll as i32;

        // 1. Horizontal row of square game icons (and empty Switch slots).
        s.dx = -off;
        for i in 0..slots {
            let x = X0 + i * (ICON + GAP);
            if i < n {
                s.shadow(x, ICON_Y, ICON, ICON, ICON_R, BLACK);
                game_icon(s, &self.games[i], x, ICON_Y);
                if sel_btn.is_none() && self.home == i {
                    s.ring_round(
                        x - FRAME,
                        ICON_Y - FRAME,
                        ICON + 2 * FRAME,
                        ICON + 2 * FRAME,
                        ICON_R + 6,
                        FRAME_T,
                        p.focus,
                    );
                }
                self.zone_at(Id::Game(i), x, ICON_Y, ICON, ICON, ICON_R, off);
            } else {
                s.fill_round(x, ICON_Y, ICON, ICON, ICON_R, p.surface);
                s.ring_round(x, ICON_Y, ICON, ICON, ICON_R, 2, p.rule);
                if n == 0 && i == 0 {
                    s.label_c(x + ICON / 2, ICON_Y + 108, "No games", Face::BodyB, p.text);
                    s.label_c(
                        x + ICON / 2,
                        ICON_Y + 148,
                        "Copy them to /games",
                        Face::Cap,
                        p.dim,
                    );
                }
            }
        }
        s.dx = 0;

        // 2. Selected game's title and play time above its square icon.
        if sel_btn.is_none() && n > 0 {
            let g = &self.games[self.home];
            let play = self.play.label(&g.name, self.wall);
            let pw = font::width(Face::CapB, &play);
            let sx = (sel_x as i32 - off).clamp(64, W as i32 - 360) as usize;
            icon(s, Icon::GameCard, sx + 10, TITLE_Y, p.accent, p.bg);
            let max_tw = (W - 64 - pw - 28).saturating_sub(sx + 30);
            let title = fit(Face::Title, &g.title, max_tw);
            let tw = s.label(sx + 30, TITLE_Y, &title, Face::Title, p.accent);
            s.label(sx + 30 + tw + 20, TITLE_Y + 2, &play, Face::CapB, p.dim);
        }

        // 3. Centered row of round system buttons underneath.
        let total_w = BUTTONS.len() * BTN + (BUTTONS.len() - 1) * BTN_GAP;
        let bx0 = (W - total_w) / 2;
        for k in 0..BUTTONS.len() {
            let bx = bx0 + k * (BTN + BTN_GAP);
            let cx = bx + BTN / 2;
            let r = BTN / 2;
            s.shadow(bx, BTN_CY - r, BTN, BTN, r, BLACK);
            if let Some(img) = art::get(art::UI, art::button(k)) {
                s.image(&img, bx, BTN_CY - r, BTN, BTN, r);
            } else {
                s.disc(cx, BTN_CY, r, p.surface);
            }
            if crate::theme::is_light() {
                s.ring(cx, BTN_CY, r, 1, p.rule);
            }
            if k == 3 && self.unread > 0 {
                s.disc(cx + 24, BTN_CY - 24, 6, p.accent);
            }
            if sel_btn == Some(k) {
                s.ring(cx, BTN_CY, r + 6, 4, p.focus);
            }
            self.zone(Id::Tile(k), bx, BTN_CY - r, BTN, BTN, r);
        }

        // 4. Selected round button's name and live status below the row.
        if let Some(k) = sel_btn {
            let name = BUTTONS[k];
            let info = self.button_info(k, d);
            let nw = font::width(Face::BodyB, name);
            let sep = "  \u{b7}  ";
            let sw = font::width(Face::Small, sep);
            let iw = font::width(Face::Small, &info);
            let x = (W.saturating_sub(nw + sw + iw)) / 2;
            s.label(x, BTN_LABEL_Y, name, Face::BodyB, p.accent);
            s.label(x + nw, BTN_LABEL_Y, sep, Face::Small, p.dim);
            s.label(x + nw + sw, BTN_LABEL_Y, &info, Face::Small, p.dim);
        }

        // 5. Bottom bar: controller icon on the left, button hints on the right.
        if sel_btn.is_none() && n > 0 {
            self.hints(
                s,
                30,
                W - 54,
                p.bg,
                &[("Y", "Theme"), ("+", "Options"), ("A", "Start")],
            );
        } else {
            self.hints(
                s,
                30,
                W - 54,
                p.bg,
                &[("Y", "Theme"), ("+", "Options"), ("A", "OK")],
            );
        }
    }

    /// A zone for an item drawn at row coordinate `x`, the row shifted by
    /// `off`: only its visible part on the screen.
    #[allow(clippy::too_many_arguments)]
    fn zone_at(&mut self, id: Id, x: usize, y: usize, w: usize, h: usize, r: usize, off: i32) {
        let (l, right) = (
            (x as i32 - off).max(0),
            (x as i32 + w as i32 - off).min(W as i32),
        );
        if right > l {
            self.zone(id, l as usize, y, (right - l) as usize, h, r);
        }
    }
}

/// A game's square icon from its `cover.bin`, or its title on its color.
fn game_icon(s: &mut Screen, g: &GameInfo, x: usize, y: usize) {
    match g.cover.as_deref().and_then(|c| art::get(c, art::ICON)) {
        Some(img) => s.image(&img, x, y, ICON, ICON, ICON_R),
        None => {
            s.fill_round(x, y, ICON, ICON, ICON_R, g.color);
            s.fade(x, y + ICON / 2, ICON, ICON / 2, 120);
            let t = fit(Face::TitleB, &g.title, ICON - 24);
            s.label_c(x + ICON / 2, y + ICON / 2, &t, Face::TitleB, WHITE);
        }
    }
}

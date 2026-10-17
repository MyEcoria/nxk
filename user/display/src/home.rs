//! The home page: the row of games, the last played first and the
//! selected one large, as the Switch's, then the app tiles two by column;
//! the play time above the selection, its name under the row.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{input, power};

use crate::art;
use crate::font::Face;
use crate::games::GameInfo;
use crate::screen::Screen;
use crate::stats::{Data, Seen};
use crate::status::usb_short;
use crate::theme::{pal, WHITE};
use crate::ui::{Action, Id, Ui, LOGS, NETWORK, SETTINGS, SYSTEM, TERMINAL, W};
use crate::widgets::{button_glyph, fit, heading, icon, Icon};

/// The tiles' names, in `art::UI` order.
pub const TILES: [&str; 6] = [
    "System",
    "Network",
    "Terminal",
    "Logs",
    "System Settings",
    "Power",
];
/// Left edge of the selection when the row is scrolled to it.
const X0: usize = 108;
/// The row: its cards and tiles, the selected game's art and its frame.
const ROW_Y: usize = 203;
const BIG: (usize, usize) = (272, 360);
const BIG_Y: usize = 201;
const FRAME: usize = 7;
const SMALL: (usize, usize) = (200, 297);
const TILE: usize = 140;
const TILE_GAP: usize = 17;
const GAP: usize = 22;
/// Center lines: the play time, the selection's name, the Y hint.
const ABOVE_Y: usize = 167;
const NAME_Y: usize = 541;
const BELOW_Y: usize = 600;

#[derive(Clone, Copy)]
enum Slot {
    Game(usize),
    /// Tiles `2k` and `2k + 1`.
    Column(usize),
    /// No game on the card: where they would be.
    Empty,
}

impl Ui {
    fn slots(&self) -> Vec<(Slot, usize, usize)> {
        let mut out = Vec::new();
        let mut x = X0;
        let mut push = |slot, w: usize| {
            out.push((slot, x, w));
            x += w + GAP;
        };
        if self.games.is_empty() {
            push(Slot::Empty, BIG.0);
        }
        for i in 0..self.games.len() {
            push(Slot::Game(i), if self.home == i { BIG.0 } else { SMALL.0 });
        }
        for k in 0..TILES.len() / 2 {
            push(Slot::Column(k), TILE);
        }
        out
    }

    /// What A or a tap on tile `k` opens.
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

    /// The gamepad on the row: `Some` when the button was for it.
    pub(crate) fn home_pad(&mut self, code: u64) -> Option<Option<Action>> {
        let n = self.games.len();
        let tile = self.home.checked_sub(n);
        match (code, tile) {
            (input::BTN_LEFT, Some(k)) if k >= 2 => self.home -= 2,
            (input::BTN_LEFT, Some(_)) if n > 0 => self.home = n - 1,
            (input::BTN_LEFT, None) if self.home > 0 => self.home -= 1,
            (input::BTN_RIGHT, None) => self.home += 1,
            (input::BTN_RIGHT, Some(k)) if k + 2 < TILES.len() => self.home += 2,
            (input::BTN_UP, Some(k)) if k % 2 == 1 => self.home -= 1,
            (input::BTN_DOWN, Some(k)) if k % 2 == 0 => self.home += 1,
            (input::BTN_A, None) => return Some(Some(Action::Launch(self.home))),
            (input::BTN_A, Some(k)) => return Some(self.open_tile(k)),
            (input::BTN_LEFT | input::BTN_RIGHT | input::BTN_UP | input::BTN_DOWN, _) => {}
            _ => return None,
        }
        Some(None)
    }

    /// The line above tile `k`: what its page would show first.
    fn tile_info(&self, k: usize, d: &Data) -> String {
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
                if self.cfg.light { "light" } else { "dark" },
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
        self.home = self.home.min(n + TILES.len() - 1);
        let slots = self.slots();
        let sel_tile = self.home.checked_sub(n);
        let sel_x = slots
            .iter()
            .find(|(slot, _, _)| match (*slot, sel_tile) {
                (Slot::Game(i), None) => i == self.home,
                (Slot::Column(k), Some(t)) => k == t / 2,
                _ => false,
            })
            .map_or(X0, |t| t.1);
        let end = slots.last().map_or(X0, |t| t.1 + t.2);
        self.scroll_to = (sel_x - X0).min(end.saturating_sub(W - 48)) as f32;
        self.scroll += (self.scroll_to - self.scroll) * 0.35;
        if (self.scroll - self.scroll_to).abs() <= 0.5 {
            self.scroll = self.scroll_to;
        }
        let off = self.scroll as i32;

        // The row, drawn shifted, its zones where they show.
        s.dx = -off;
        for &(slot, x, w) in &slots {
            match slot {
                Slot::Empty => {
                    s.ring_round(
                        x - FRAME,
                        BIG_Y - FRAME,
                        BIG.0 + 2 * FRAME,
                        BIG.1 + 2 * FRAME,
                        8,
                        2,
                        p.faint,
                    );
                    s.label_c(x + w / 2, BIG_Y + 150, "No games", Face::Title, p.text);
                    s.label_c(
                        x + w / 2,
                        BIG_Y + 196,
                        "Copy them to /games",
                        Face::Small,
                        p.dim,
                    );
                    s.label_c(x + w / 2, BIG_Y + 224, "on the SD card", Face::Small, p.dim);
                }
                Slot::Game(i) if self.home == i => self.big_card(s, i, x),
                Slot::Game(i) => {
                    cover(s, &self.games[i], art::COVER_SMALL, x, ROW_Y, SMALL);
                    self.zone_at(Id::Game(i), x, ROW_Y, w, SMALL.1, off);
                }
                Slot::Column(k) => {
                    for t in [2 * k, 2 * k + 1] {
                        let y = ROW_Y + (t % 2) * (TILE + TILE_GAP);
                        if let Some(img) = art::get(art::UI, t) {
                            s.image(&img, x, y, TILE, TILE, 4);
                        }
                        if sel_tile == Some(t) {
                            s.ring_round(
                                x - FRAME,
                                y - FRAME,
                                TILE + 2 * FRAME,
                                TILE + 2 * FRAME,
                                8,
                                3,
                                p.focus,
                            );
                        }
                        self.zone_at(Id::Tile(t), x, y, TILE, TILE, off);
                    }
                }
            }
        }
        s.dx = 0;

        // Around the selection, on the screen.
        let sx = (sel_x as i32 - off).max(0) as usize;
        match sel_tile {
            None if n > 0 => {
                let g = &self.games[self.home];
                heading(s, sx + 2, ABOVE_Y, &self.play.label(&g.name, self.wall));
                let nx = sx + BIG.0 + 36;
                icon(s, Icon::GameCard, nx, NAME_Y, p.accent, p.bg);
                let title = fit(Face::Title, &g.title, W - 32 - (nx + 24));
                s.label(nx + 24, NAME_Y, &title, Face::Title, p.accent);
                button_glyph(s, sx + 13, BELOW_Y, "Y", p.text, p.bg);
                s.label(sx + 33, BELOW_Y, "Change Theme", Face::Body, p.text);
            }
            None => {}
            Some(t) => {
                let info = self.tile_info(t, d);
                heading(s, sx + 2, ABOVE_Y, &fit(Face::CapB, &info, W - 32 - sx));
                let title = fit(Face::Title, TILES[t], W - 32 - sx);
                s.label(sx, NAME_Y, &title, Face::Title, p.accent);
            }
        }
        let a = if sel_tile.is_none() && n > 0 {
            "Start"
        } else {
            "Open"
        };
        self.hints(s, 26, W - 31, p.bg, &[("A", a), ("+", "Options")]);
    }

    /// A zone for something drawn at row coordinate `x`, the row shifted
    /// by `off`: only its part on the screen.
    fn zone_at(&mut self, id: Id, x: usize, y: usize, w: usize, h: usize, off: i32) {
        let (l, r) = (
            (x as i32 - off).max(0),
            (x as i32 + w as i32 - off).min(W as i32),
        );
        if r > l {
            self.zone(id, l as usize, y, (r - l) as usize, h, 4);
        }
    }

    /// The selected game: its large art in a frame, the avatar on the
    /// one played last.
    fn big_card(&mut self, s: &mut Screen, i: usize, x: usize) {
        let p = pal();
        let (w, h) = BIG;
        let g = &self.games[i];
        s.ring_round(
            x - FRAME,
            BIG_Y - FRAME,
            w + 2 * FRAME,
            h + 2 * FRAME,
            8,
            3,
            p.focus,
        );
        cover(s, g, art::COVER_BIG, x, BIG_Y, BIG);
        if self.play.last() == Some(g.name.as_str()) {
            s.fade(x, BIG_Y + h - 150, w, 150, 210);
            let (ax, ay) = (x + 14, BIG_Y + h - 68);
            if let Some(img) = art::get(art::UI, art::AVATAR_SMALL) {
                s.image(&img, ax, ay, 56, 56, 28);
            }
            s.ring(ax + 28, ay + 28, 28, 2, WHITE);
            s.label(x + 85, ay + 28, "Last played", Face::Body, WHITE);
        }
        let off = self.scroll as i32;
        self.zone_at(
            Id::Game(i),
            x - FRAME,
            BIG_Y - FRAME,
            w + 2 * FRAME,
            h + 2 * FRAME,
            off,
        );
    }
}

/// A game's art, `which` of its cover, or its title on its color.
fn cover(s: &mut Screen, g: &GameInfo, which: usize, x: usize, y: usize, (w, h): (usize, usize)) {
    match g.cover.as_deref().and_then(|c| art::get(c, which)) {
        Some(img) => s.image(&img, x, y, w, h, 4),
        None => {
            s.fill_round(x, y, w, h, 4, g.color);
            s.fade(x, y + h / 2, w, h / 2, 120);
            let t = fit(Face::TitleB, &g.title, w - 24);
            s.label_c(x + w / 2, y + h / 3, &t, Face::TitleB, WHITE);
        }
    }
}

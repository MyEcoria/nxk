//! The Switch 2 home screen: a horizontal row of square game icons with
//! a cyan selection frame and the selected game's title above it, and a
//! centered row of round system buttons underneath.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::{input, power, provider};

use crate::art;
use crate::font::{self, Face};
use crate::games::GameInfo;
use crate::screen::Screen;
use crate::stats::{Data, Seen};
use crate::status::usb_short;
use crate::theme::{pal, BLACK, WHITE};
use crate::ui::{
    Action, Id, Ui, CONTROLLERS, H, LOGS, NETWORK, SETTINGS, STATUS_Y, SYSTEM, TERMINAL, W,
};
use crate::widgets::{button_glyph, fit, icon, Icon, Tone};

/// The round buttons under the game row, in `art::button` / `art::head` order.
pub const BUTTONS: [&str; 7] = [
    "System",
    "Network",
    "Terminal",
    "Logs",
    "Controllers",
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
const ICON_R: usize = 22;
const GAP: usize = 22;
const FRAME: usize = 8;
const FRAME_T: usize = 5;
/// Center line of the selected game's title and play time above the row.
const TITLE_Y: usize = 124;
/// The unified pill dock underneath (`BUTTONS`) and the protruding circle
/// of the selected dock button.
const BTN: usize = 76;
const BTN_GAP: usize = 18;
const DOCK_H: usize = 82;
const DOCK_R: usize = DOCK_H / 2;
const DOCK_PAD: usize = 24;
const SEL_R: usize = 47;
const BTN_CY: usize = 504;
const BTN_TITLE_Y: usize = 572;
const BTN_INFO_Y: usize = 602;

impl Ui {
    /// What A or a tap on round button `k` opens.
    pub(crate) fn open_tile(&mut self, k: usize) -> Option<Action> {
        match k {
            0 => self.go(SYSTEM),
            1 => self.go(NETWORK),
            2 => self.go(TERMINAL),
            3 => self.go(LOGS),
            4 => self.go(CONTROLLERS),
            5 => self.go(SETTINGS),
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

    /// Short status shown below the selected dock button's name.
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
                Seen::Value(src) => {
                    let how = if src == power::SUPPLY_EXTERNAL {
                        "on input power"
                    } else {
                        "on battery"
                    };
                    match d.battery {
                        Seen::Value(b) => format!("{}% \u{b7} {}", b.percent(), how),
                        _ => how.into(),
                    }
                }
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
        let last_idx = self
            .play
            .last()
            .and_then(|name| self.games.iter().position(|g| g.name == name))
            .unwrap_or(0);

        // 1. Horizontal row of square game icons (and empty Switch slots),
        // or the catalog's sign-in.
        if self.signing_in() {
            self.login_panel(s);
        } else {
            s.dx = -off;
            for i in 0..slots {
                let x = X0 + i * (ICON + GAP);
                if i < n {
                    game_icon(s, &self.games[i], x, ICON_Y);
                    kind_badge(s, x, ICON_Y, self.games[i].cloud);
                    if i == last_idx {
                        game_badge(
                            s,
                            x,
                            ICON_Y,
                            self.users.active().map(|u| (u.color, u.glyph)),
                        );
                    }
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
                        s.ring_round(x - 3, ICON_Y - 3, ICON + 6, ICON + 6, ICON_R + 2, 2, WHITE);
                    }
                    self.zone_at(Id::Game(i), x, ICON_Y, ICON, ICON, ICON_R, off);
                } else {
                    s.fill_round(x, ICON_Y, ICON, ICON, ICON_R, p.surface);
                    s.ring_round(x, ICON_Y, ICON, ICON, ICON_R, 2, p.rule);
                    if n == 0 && i == 0 {
                        let (head, line) = self.empty_text();
                        let line = fit(Face::Cap, &line, ICON - 24);
                        s.label_c(x + ICON / 2, ICON_Y + 108, head, Face::BodyB, p.text);
                        s.label_c(x + ICON / 2, ICON_Y + 148, &line, Face::Cap, p.dim);
                    }
                }
            }
            s.dx = 0;
        }

        // 2. Selected game's title and play time above its square icon.
        if sel_btn.is_none() && n > 0 {
            let g = &self.games[self.home];
            let play = self.play.label(&g.name, self.wall);
            let pw = font::width(Face::CapB, &play);
            let sx = (sel_x as i32 - off).clamp(64, W as i32 - 360) as usize;
            let kind = if g.cloud { Icon::Cloud } else { Icon::GameCard };
            icon(s, kind, sx + 10, TITLE_Y, p.accent, p.bg);
            let max_tw = (W - 64 - pw - 28).saturating_sub(sx + 30);
            let title = fit(Face::Title, &g.title, max_tw);
            let tw = s.label(sx + 30, TITLE_Y, &title, Face::Title, p.accent);
            s.label(sx + 30 + tw + 20, TITLE_Y + 2, &play, Face::CapB, p.dim);
        }

        // 3. Switch 2 unified pill dock underneath.
        let total_w = BUTTONS.len() * BTN + (BUTTONS.len() - 1) * BTN_GAP;
        let bx0 = (W - total_w) / 2;
        let dock_x = bx0 - DOCK_PAD;
        let dock_w = total_w + 2 * DOCK_PAD;
        let dock_bg = if crate::theme::is_light() {
            WHITE
        } else {
            p.surface
        };
        s.fill_round(dock_x, BTN_CY - DOCK_R, dock_w, DOCK_H, DOCK_R, dock_bg);
        if crate::theme::is_light() {
            s.ring_round(dock_x, BTN_CY - DOCK_R, dock_w, DOCK_H, DOCK_R, 1, p.rule);
        }
        let r = BTN / 2;
        for k in 0..BUTTONS.len() {
            let bx = bx0 + k * (BTN + BTN_GAP);
            let cx = bx + r;
            if sel_btn != Some(k) {
                if let Some(img) = art::get(art::UI, art::button(k, false)) {
                    s.image(&img, bx, BTN_CY - r, BTN, BTN, r);
                }
                if k == 3 && self.unread > 0 {
                    s.disc(cx + 22, BTN_CY - 22, 6, p.accent);
                }
            }
            self.zone(Id::Tile(k), bx, BTN_CY - r, BTN, BTN, r);
        }

        // 4. Protruding circle, glowing ring, and centered title for the selected dock button.
        if let Some(k) = sel_btn {
            let bx = bx0 + k * (BTN + BTN_GAP);
            let cx = bx + r;
            let sel_bg = if crate::theme::is_light() {
                WHITE
            } else {
                p.raised
            };
            s.disc(cx, BTN_CY, SEL_R, sel_bg);
            if let Some(img) = art::get(art::UI, art::button(k, true)) {
                s.image(&img, bx, BTN_CY - r, BTN, BTN, r);
            }
            if k == 3 && self.unread > 0 {
                s.disc(cx + 22, BTN_CY - 22, 6, p.accent);
            }
            s.ring(cx, BTN_CY, SEL_R + 3, 4, p.focus);
            s.ring(cx, BTN_CY, SEL_R, 2, WHITE);
            s.label_c(cx, BTN_TITLE_Y, BUTTONS[k], Face::Body, p.accent);
            let info = self.button_info(k, d);
            s.label_c(cx, BTN_INFO_Y, &info, Face::Cap, p.dim);
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

    /// What an empty game row says: the console's games, or the catalog's
    /// state (loading, an account to sign in, why it is unavailable).
    fn empty_text(&self) -> (&'static str, String) {
        if self.service.is_empty() {
            return ("No games", "Copy them to /games".into());
        }
        let Some(v) = &self.prov else {
            return ("Starting", "The service is on its way".into());
        };
        let first = v.text.lines().next().unwrap_or("").into();
        match v.state {
            provider::ST_LOADING => ("Loading", first),
            provider::ST_LOGIN => ("Sign in", first),
            provider::ST_ERROR => ("Unavailable", first),
            _ if first.is_empty() => ("No games", "This service lists none".into()),
            _ => ("No games", first),
        }
    }

    /// The catalog shown asks for an account.
    fn signing_in(&self) -> bool {
        !self.service.is_empty()
            && self
                .prov
                .as_ref()
                .is_some_and(|v| v.state == provider::ST_LOGIN)
    }

    /// Sign-in in place of the game row: the code to scan on white (whole
    /// modules, square corners, as a QR code needs), then the page and the
    /// code to type (the catalog's text: the code, then the page).
    fn login_panel(&mut self, s: &mut Screen) {
        let p = pal();
        let (x, y, w, h) = (X0, ICON_Y - 16, W - 2 * X0, ICON + 32);
        s.fill_round(x, y, w, h, ICON_R, p.surface);
        let qr = 256;
        let (qx, qy) = (x + 16, y + 16);
        s.fill(qx, qy, qr, qr, WHITE);
        if let Some(img) = self.login_art.as_ref().and_then(|c| c.image()) {
            let side = img.w.max(1);
            let px = side * (qr / side).max(1);
            let off = (qr - px.min(qr)) / 2;
            s.image(&img, qx + off, qy + off, px.min(qr), px.min(qr), 0);
        }
        let text = self.prov.as_ref().map_or(String::new(), |v| v.text.clone());
        let mut lines = text.lines();
        let code = String::from(lines.next().unwrap_or("").trim());
        let page = String::from(lines.next().unwrap_or("").trim());
        let tx = qx + qr + 40;
        let tw = x + w - tx - 32;
        let title = self.services[self.service_index()].1.clone();
        let head = fit(Face::TitleB, &format!("Sign in to {}", title), tw);
        s.label(tx, y + 50, &head, Face::TitleB, p.text);
        s.label(tx, y + 100, "Scan the code, or open", Face::Body, p.dim);
        let page = fit(Face::BodyB, &page, tw);
        s.label(tx, y + 136, &page, Face::BodyB, p.accent);
        s.label(tx, y + 172, "and enter", Face::Body, p.dim);
        let code = fit(Face::Head, &code, tw);
        s.label(tx, y + 230, &code, Face::Head, p.text);
    }

    /// The catalogs' tabs at the top of the home screen (L and R move
    /// between them), when the card has any.
    pub(crate) fn service_tabs(&mut self, s: &mut Screen) {
        if self.services.len() < 2 {
            return;
        }
        let p = pal();
        const PAD: usize = 22;
        const GAP: usize = 10;
        const TAB_H: usize = 44;
        let widths: Vec<usize> = self
            .services
            .iter()
            .map(|(_, t)| font::width(Face::BodyB, t) + 2 * PAD)
            .collect();
        let total = widths.iter().sum::<usize>() + GAP * (widths.len() - 1);
        let mut x = W.saturating_sub(total) / 2;
        let y = STATUS_Y - TAB_H / 2;
        let cur = self.service_index();
        button_glyph(s, x.saturating_sub(28), STATUS_Y, "L", p.dim, p.bg);
        for (k, w) in widths.into_iter().enumerate() {
            let title = self.services[k].1.clone();
            if k == cur {
                s.fill_round(x, y, w, TAB_H, TAB_H / 2, p.accent);
                s.label_c(x + w / 2, STATUS_Y, &title, Face::BodyB, p.on_accent);
            } else {
                s.ring_round(x, y, w, TAB_H, TAB_H / 2, 2, p.rule);
                s.label_c(x + w / 2, STATUS_Y, &title, Face::Body, p.dim);
            }
            self.zone(Id::Service(k), x, y, w, TAB_H, TAB_H / 2);
            x += w + GAP;
        }
        button_glyph(s, x - GAP + 28, STATUS_Y, "R", p.dim, p.bg);
    }

    /// A catalog game starting: its cover, the catalog's words, the place
    /// in its queue and the time waited; B cancels.
    pub(crate) fn launch_card(&mut self, s: &mut Screen, now: u64) {
        let Some((title, cover, since)) = &self.starting else {
            return;
        };
        let p = pal();
        let (title, since) = (title.clone(), *since);
        s.fill_alpha(0, 0, W, H, BLACK, 150);
        let (cw, ch) = (640, 300);
        let (x, y) = ((W - cw) / 2, (H - ch) / 2);
        s.fill_round(x, y, cw, ch, 24, p.surface);
        let art = 180;
        match cover.as_ref().and_then(|c| c.image()) {
            Some(img) => s.image(&img, x + 40, y + 40, art, art, ICON_R),
            None => s.fill_round(x + 40, y + 40, art, art, ICON_R, p.raised),
        }
        let tx = x + 40 + art + 32;
        let tw = cw - (tx - x) - 32;
        let head = fit(Face::TitleB, &format!("Starting {}", title), tw);
        s.label(tx, y + 66, &head, Face::TitleB, p.text);
        let v = self.prov.as_ref();
        let text = v.map_or(String::new(), |v| v.text.clone());
        let text = fit(Face::Body, text.lines().next().unwrap_or(""), tw);
        s.label(tx, y + 112, &text, Face::Body, p.dim);
        if let Some(place) = v.map(|v| v.place).filter(|&q| q > 0) {
            s.label(
                tx,
                y + 150,
                &format!("Place in queue: {}", place),
                Face::CapB,
                p.accent,
            );
        }
        let secs = now.saturating_sub(since) / 1_000_000_000;
        s.label(tx, y + 182, &format!("{} s", secs), Face::Cap, p.dim);
        self.button(
            s,
            Id::Cancel,
            tx,
            y + ch - 84,
            180,
            52,
            "Cancel",
            Tone::Plain,
            true,
        );
        button_glyph(s, tx + 200, y + ch - 58, "B", p.dim, p.surface);
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
    match g.cover.as_ref().and_then(|c| c.image()) {
        Some(img) => s.image(&img, x, y, ICON, ICON, ICON_R),
        None => {
            s.fill_round(x, y, ICON, ICON, ICON_R, g.color);
            s.fade(x, y + ICON / 2, ICON, ICON / 2, 120);
            let t = fit(Face::TitleB, &g.title, ICON - 24);
            s.label_c(x + ICON / 2, y + ICON / 2, &t, Face::TitleB, WHITE);
        }
    }
}

/// Where the game runs, top right of its icon: a cloud (streamed from a
/// server) or a game card (on this console).
fn kind_badge(s: &mut Screen, x: usize, y: usize, cloud: bool) {
    let (bx, by) = (x + ICON - 58, y + 14);
    s.round_a(bx, by, 44, 44, 22, BLACK, 190);
    let kind = if cloud { Icon::Cloud } else { Icon::GameCard };
    icon(s, kind, bx + 22, by + 22, WHITE, BLACK);
}

/// Switch 2 bottom-left avatar and black status pill on the last-played game card.
fn game_badge(s: &mut Screen, x: usize, y: usize, user: Option<(u8, u8)>) {
    let ay = y + ICON - 56;
    match user {
        Some((c, g)) => crate::avatar::avatar(s, x + 34, ay + 22, 22, c, g),
        None => {
            if let Some(img) = art::get(art::UI, art::AVATAR_SMALL) {
                s.image(&img, x + 12, ay, 44, 44, 22);
            }
        }
    }
    s.ring(x + 34, ay + 22, 22, 2, crate::theme::rgb(0x2E2E2E));
    let (px, py, pw, ph) = (x + 64, ay + 2, 180, 40);
    s.fill_round(px, py, pw, ph, 20, BLACK);
    s.ring_round(px, py, pw, ph, 20, 2, crate::theme::rgb(0xD8D8D8));
    s.label_c(px + pw / 2, py + ph / 2, "Last played", Face::Small, WHITE);
}

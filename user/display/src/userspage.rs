//! USERS: the profiles on the left, the chosen one's details on the right
//! (switch, rename, look, PIN, delete) and what each game keeps for them,
//! with the means to erase it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;


use crate::avatar::{avatar, padlock};
use crate::font::Face;
use crate::screen::{Bytes, Screen};
use crate::theme::pal;
use crate::ui::{Action, Id, Ui, CB, CY, M, W};
use crate::users::MAX_USERS;
use crate::widgets::{fit, heading, icon, press, Icon, Tone};

const LW: usize = 360;
const X: usize = M + LW + 44;
const XR: usize = W - M;
const ROW: usize = 66;
/// Games listed at once.
const SHOWN: usize = 4;

impl Ui {
    /// The profile whose data the page is about.
    fn selected(&self) -> Option<usize> {
        (self.usel < self.users.list.len()).then_some(self.usel)
    }

    /// The page was opened or its user changed: the saves are read again.
    pub(crate) fn want_saves(&mut self) {
        self.saves_req = true;
        self.saves_page = 0;
    }

    /// The user whose saves are wanted, if the page asked.
    pub fn take_saves_request(&mut self) -> Option<String> {
        if !core::mem::take(&mut self.saves_req) {
            return None;
        }
        Some(self.users.list.get(self.usel)?.id.clone())
    }

    pub fn set_saves(&mut self, id: &str, v: Vec<(String, u64)>) {
        if self.users.list.get(self.usel).is_some_and(|p| p.id == id) {
            self.saves = v;
        }
    }

    /// What each game keeps for user `usel`: the title if the game is on the card.
    fn game_title(&self, name: &str) -> String {
        self.games
            .iter()
            .find(|g| g.name == name)
            .map_or(name.into(), |g| g.title.clone())
    }

    pub(crate) fn users_page(&mut self, s: &mut Screen) {
        let p = pal();
        if self.usel >= self.users.list.len() {
            self.usel = self.users.current.unwrap_or(0);
        }
        // The profiles.
        for i in 0..self.users.list.len() {
            let y = CY + i * ROW;
            let on = self.usel == i;
            let down = self.pressed == Some(Id::UserRow(i));
            if on || down {
                s.fill_round(M, y, LW, ROW - 8, 6, press(p.surface, down));
            }
            if on {
                s.fill_round(M + 10, y + 12, 5, ROW - 32, 2, p.accent);
            }
            let u = &self.users.list[i];
            avatar(s, M + 52, y + (ROW - 8) / 2, 24, u.color, u.glyph);
            let (name, active, locked) = (
                u.name.clone(),
                self.users.current == Some(i),
                u.pin.is_some(),
            );
            let f = if on { Face::BodyB } else { Face::Body };
            let c = if on { p.accent } else { p.text };
            if active {
                s.label(M + 90, y + 19, &fit(f, &name, 200), f, c);
                s.label(M + 90, y + 43, "Playing now", Face::Cap, p.dim);
            } else {
                s.label(M + 90, y + (ROW - 8) / 2, &fit(f, &name, 220), f, c);
            }
            if locked {
                padlock(s, M + LW - 28, y + (ROW - 8) / 2, p.dim);
            }
            self.zone(Id::UserRow(i), M, y, LW, ROW - 8, 6);
        }
        let n = self.users.list.len();
        if n < MAX_USERS {
            let y = CY + n * ROW;
            let down = self.pressed == Some(Id::UserAdd);
            s.fill_round(M, y, LW, ROW - 8, 6, press(p.surface, down));
            s.ring_round(M, y, LW, ROW - 8, 6, 2, p.rule);
            let (cx, cy) = (M + 52, y + (ROW - 8) / 2);
            s.disc(cx, cy, 24, p.raised);
            s.fill(cx - 9, cy - 1, 18, 3, p.text);
            s.fill(cx - 1, cy - 9, 3, 18, p.text);
            s.label(M + 90, cy, "Add User", Face::Body, p.text);
            self.zone(Id::UserAdd, M, y, LW, ROW - 8, 6);
        }
        s.fill(M + LW + 22, CY - 20, 1, CB - CY + 20, p.rule);

        let Some(i) = self.selected() else {
            return self.page_hints(s);
        };
        let u = self.users.list[i].clone();
        let open = self.users.open(i);
        let active = self.users.current == Some(i);

        avatar(s, X + 46, CY + 48, 46, u.color, u.glyph);
        s.label(
            X + 112,
            CY + 34,
            &fit(Face::TitleB, &u.name, XR - X - 140),
            Face::TitleB,
            p.text,
        );
        let sub = if active {
            "Playing now"
        } else if !open {
            "Protected by a PIN"
        } else {
            "Not playing"
        };
        let theme = if u.light {
            "Basic White"
        } else {
            "Basic Black"
        };
        let lock = if u.pin.is_some() {
            " \u{b7} PIN set"
        } else {
            ""
        };
        let line = if open {
            format!("{} \u{b7} {}{}", sub, theme, lock)
        } else {
            sub.into()
        };
        s.label(X + 112, CY + 70, &line, Face::Small, p.dim);

        let by = CY + 112;
        if !open {
            self.button(
                s,
                Id::UserUnlock,
                X,
                by,
                220,
                48,
                "Unlock",
                Tone::Accent,
                true,
            );
            s.label(
                X + 240,
                by + 24,
                "Type the PIN to manage this profile.",
                Face::Small,
                p.dim,
            );
            return self.page_hints(s);
        }
        let mut x = X;
        if !active {
            self.button(
                s,
                Id::UserSwitch,
                x,
                by,
                170,
                48,
                "Switch to",
                Tone::Accent,
                true,
            );
            x += 182;
        }
        let pin = if u.pin.is_some() { "PIN" } else { "Add PIN" };
        for (id, label, w, tone) in [
            (Id::UserRename, "Rename", 120, Tone::Plain),
            (Id::UserLook, "Look", 100, Tone::Plain),
            (Id::UserPin, pin, 120, Tone::Plain),
            (Id::UserDelete, "Delete", 120, Tone::Bad),
        ] {
            self.button(s, id, x, by, w, 48, label, tone, true);
            x += w + 12;
        }

        // Save data.
        let ry = by + 76;
        s.fill(X, ry - 22, XR - X, 1, p.line);
        heading(s, X, ry + 4, "Save data");
        let total: u64 = self.saves.iter().map(|e| e.1).sum();
        let pages = self.saves.len().div_ceil(SHOWN);
        let mut right = XR;
        if pages > 1 {
            self.saves_page = self.saves_page.min(pages - 1);
            let (prev, next) = (self.saves_page > 0, self.saves_page + 1 < pages);
            self.chip(
                s,
                Id::SavesNext,
                XR - 84,
                ry - 14,
                84,
                36,
                "Next",
                false,
                next,
            );
            self.chip(
                s,
                Id::SavesPrev,
                XR - 84 - 92,
                ry - 14,
                84,
                36,
                "Prev",
                false,
                prev,
            );
            right = XR - 84 - 92 - 16;
        }
        s.label_r(
            right,
            ry + 4,
            &format!("{}", Bytes(total)),
            Face::Small,
            p.dim,
        );
        if self.saves.is_empty() {
            s.label(
                X,
                ry + 60,
                "No game has saved anything for this user yet.",
                Face::Small,
                p.dim,
            );
        }
        let first = self.saves_page * SHOWN;
        for k in first..(first + SHOWN).min(self.saves.len()) {
            let cy = ry + 52 + (k - first) * 50;
            let (name, size) = self.saves[k].clone();
            let title = self.game_title(&name);
            icon(s, Icon::GameCard, X + 14, cy, p.accent, p.bg);
            s.label(
                X + 44,
                cy,
                &fit(Face::Body, &title, 330),
                Face::Body,
                p.text,
            );
            s.label_r(
                XR - 124,
                cy,
                &format!("{}", Bytes(size)),
                Face::Small,
                p.dim,
            );
            self.chip(
                s,
                Id::SaveErase(k),
                XR - 100,
                cy - 18,
                100,
                36,
                "Erase",
                false,
                true,
            );
        }
        self.page_hints(s);
    }

    /// A tap on the page's buttons.
    pub(crate) fn users_tap(&mut self, id: Id) -> Option<Option<Action>> {
        let i = self.selected();
        match id {
            Id::UserRow(k) => {
                self.usel = k;
                self.want_saves();
            }
            Id::UserAdd => self.start_create(),
            Id::UserSwitch => {
                let i = i?;
                if self.users.open(i) {
                    return Some(Some(Action::UserActivate(i)));
                }
                self.start_verify(i, true);
            }
            Id::UserUnlock => self.start_verify(i?, false),
            Id::UserRename => self.start_rename(i?),
            Id::UserLook => self.start_look(i?),
            Id::UserPin => self.start_pin(i?),
            Id::UserDelete => {
                let name = self.users.list.get(i?)?.name.clone();
                let only = self.users.list.len() == 1;
                self.confirm(
                    &format!("Delete {}?", fit(Face::Body, &name, 360)),
                    if only {
                        [
                            "The setup starts again, as on a new",
                            "console. Their save data is erased.",
                        ]
                    } else {
                        ["Their save data is erased too.", "This cannot be undone."]
                    },
                    "Delete",
                    Tone::Bad,
                    Action::UserDelete(i?),
                );
            }
            Id::SaveErase(k) => {
                let (game, _) = self.saves.get(k)?.clone();
                let who = self.users.list.get(i?)?.name.clone();
                let title = self.game_title(&game);
                self.confirm(
                    &format!("Erase {}?", fit(Face::Body, &title, 360)),
                    [
                        &format!("{}'s save data for this game", fit(Face::Small, &who, 200)),
                        "is erased.",
                    ],
                    "Erase",
                    Tone::Warn,
                    Action::EraseSave(i?, k),
                );
            }
            Id::SavesPrev => self.saves_page = self.saves_page.saturating_sub(1),
            Id::SavesNext => self.saves_page += 1,
            _ => return None,
        }
        Some(None)
    }

    /// User `i` was removed from the card: who plays next.
    pub fn remove_user(&mut self, i: usize) -> Option<usize> {
        let was_current = self.users.current == Some(i);
        self.users.remove(i);
        self.usel = self
            .usel
            .saturating_sub((self.usel >= i && self.usel > 0) as usize);
        self.want_saves();
        if !was_current {
            return None;
        }
        if self.users.list.is_empty() {
            self.go(crate::ui::LIBRARY);
            self.start_setup();
            return None;
        }
        if self.users.list[0].pin.is_some() {
            self.start_verify(0, true);
            return None;
        }
        Some(0)
    }
}

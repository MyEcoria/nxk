//! The games on the card: `/games/NAME/NAME` (the program),
//! `/games/NAME/game.txt` (`title=`, `publisher=`, `version=`,
//! `description=`, `color=RRGGBB`) and `/games/NAME/cover.bin` (its art,
//! tools/mkcovers.py; without it, the title on its color).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::fs;

use crate::art;

const DIR: &str = "/games";

pub struct GameInfo {
    /// The directory's name: the key of its play time.
    pub name: String,
    pub title: String,
    pub publisher: String,
    pub version: String,
    pub desc: String,
    /// Card color, A B G R (as the screen's).
    pub color: u32,
    pub exe: String,
    /// A catalog's game: its id there (`exe` is empty).
    pub id: String,
    /// Streamed from a server (`GAME_CLOUD`); the console's own games and
    /// a catalog's local ones run here.
    pub cloud: bool,
    pub cover: Option<Cover>,
}

/// A game's art, checked: `cover.bin` (paletted) or a catalog's
/// (`art::direct`).
#[derive(Clone)]
pub enum Cover {
    Art(Vec<u8>),
    Direct(Vec<u8>),
}

impl Cover {
    pub fn image(&self) -> Option<art::Image<'_>> {
        match self {
            Cover::Art(c) => art::get(c, art::ICON),
            Cover::Direct(c) => art::direct(c),
        }
    }
}

/// The games, by title; nothing while the card is not mounted. Covers
/// already read in `old` are kept, not read again.
pub fn scan(mut old: Vec<GameInfo>) -> Vec<GameInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(DIR) else {
        return out;
    };
    for e in entries {
        let exe = format!("{}/{}/{}", DIR, e.name, e.name);
        if fs::stat(&exe).is_err() {
            continue;
        }
        let kept = old
            .iter_mut()
            .find(|g| g.name == e.name)
            .and_then(|g| g.cover.take());
        let cover = kept.or_else(|| {
            fs::read(&format!("{}/{}/cover.bin", DIR, e.name))
                .ok()
                .filter(|c| art::get(c, art::ICON).is_some_and(|i| i.w == i.h))
                .map(Cover::Art)
        });
        let mut g = GameInfo {
            name: e.name.clone(),
            title: e.name.clone(),
            publisher: String::new(),
            version: String::new(),
            desc: String::new(),
            color: 0xFFF8_A824,
            exe,
            id: String::new(),
            cloud: false,
            cover,
        };
        if let Ok(text) = fs::read(&format!("{}/{}/game.txt", DIR, e.name)) {
            parse(&String::from_utf8_lossy(&text), &mut g);
        }
        out.push(g);
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

pub fn parse(text: &str, g: &mut GameInfo) {
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "title" => g.title = v.chars().take(40).collect(),
            "publisher" => g.publisher = v.chars().take(40).collect(),
            "version" => g.version = v.chars().take(16).collect(),
            "description" => g.desc = v.chars().take(80).collect(),
            "color" => {
                if let Ok(rgb) = u32::from_str_radix(v.trim_start_matches('#'), 16) {
                    g.color = crate::theme::rgb(rgb);
                }
            }
            _ => {}
        }
    }
}

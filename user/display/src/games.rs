//! The games on the card: `/games/NAME/NAME` (the program) and
//! `/games/NAME/game.txt` (`title=`, `description=`, `color=RRGGBB`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::fs;

const DIR: &str = "/games";

#[derive(Clone)]
pub struct GameInfo {
    pub title: String,
    pub desc: String,
    /// Card color, A B G R (as the screen's).
    pub color: u32,
    pub exe: String,
}

/// The games, by title; nothing while the card is not mounted.
pub fn scan() -> Vec<GameInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(DIR) else {
        return out;
    };
    for e in entries {
        let exe = format!("{}/{}/{}", DIR, e.name, e.name);
        if fs::stat(&exe).is_err() {
            continue;
        }
        let mut g = GameInfo {
            title: e.name.clone(),
            desc: String::new(),
            color: 0xFFF0_A04C,
            exe,
        };
        if let Ok(text) = fs::read(&format!("{}/{}/game.txt", DIR, e.name)) {
            parse(&String::from_utf8_lossy(&text), &mut g);
        }
        out.push(g);
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

fn parse(text: &str, g: &mut GameInfo) {
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "title" => g.title = v.chars().take(24).collect(),
            "description" => g.desc = v.chars().take(80).collect(),
            "color" => {
                if let Ok(rgb) = u32::from_str_radix(v.trim_start_matches('#'), 16) {
                    let (r, gr, b) = (rgb >> 16 & 0xFF, rgb >> 8 & 0xFF, rgb & 0xFF);
                    g.color = 0xFF00_0000 | b << 16 | gr << 8 | r;
                }
            }
            _ => {}
        }
    }
}

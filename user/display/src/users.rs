//! The console's users, on the SD card: `/users/ID/profile.cfg` (`key=value`
//! lines: name, avatar, theme, PIN), `/users/ID/playtime.cfg` and
//! `/users/ID/saves/GAME/`, the directory display hands a game as
//! `NXK_SAVE_DIR`. `/users/last` names the user of the last session.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err::ENOENT;
use libnx::fs::{self, Kind};

pub const ROOT: &str = "/users";
const LAST: &str = "/users/last";
pub const MAX_USERS: usize = 6;
pub const NAME_MAX: usize = 12;
pub const PIN_LEN: usize = 4;
/// Avatar colors (R G B) and pixel-art faces drawn on them.
pub const COLORS: [u32; 8] = [
    0xFF4655, 0xFF9F1C, 0xFFD23F, 0x3DDC84, 0x2EC4B6, 0x4D8DFF, 0x9B5DE5, 0xF15BB5,
];
pub const GLYPHS: usize = 8;

#[derive(Clone)]
pub struct Profile {
    /// The directory's name under `/users`.
    pub id: String,
    pub name: String,
    pub color: u8,
    pub glyph: u8,
    pub light: bool,
    /// Hash of the PIN (`pin_hash`).
    pub pin: Option<u64>,
}

impl Profile {
    pub fn new(id: String, name: String, color: u8, glyph: u8, light: bool) -> Self {
        Self {
            id,
            name,
            color,
            glyph,
            light,
            pin: None,
        }
    }

    pub fn dir(&self) -> String {
        dir(&self.id)
    }

    pub fn text(&self) -> String {
        let mut t = format!(
            "name={}\ncolor={}\nglyph={}\ntheme={}\n",
            self.name,
            self.color,
            self.glyph,
            if self.light { "light" } else { "dark" }
        );
        if let Some(p) = self.pin {
            t.push_str(&format!("pin={:016x}\n", p));
        }
        t
    }

    fn parse(id: &str, text: &str) -> Self {
        let mut p = Self::new(id.into(), id.into(), 5, 0, false);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "name" => p.name = clean(v),
                "color" => p.color = v.parse::<u8>().unwrap_or(5) % COLORS.len() as u8,
                "glyph" => p.glyph = v.parse::<u8>().unwrap_or(0) % GLYPHS as u8,
                "theme" => p.light = v == "light",
                "pin" => p.pin = u64::from_str_radix(v, 16).ok(),
                _ => {}
            }
        }
        if p.name.is_empty() {
            p.name = id.into();
        }
        p
    }

    /// The PIN is `pin`.
    pub fn check(&self, pin: &str) -> bool {
        self.pin == Some(pin_hash(&self.id, pin))
    }
}

/// A name as typed: letters, digits and single spaces, `NAME_MAX` long.
pub fn clean(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || (c == ' ' && !out.is_empty() && !out.ends_with(' '));
        if ok && out.chars().count() < NAME_MAX {
            out.push(c);
        }
    }
    out.trim_end().into()
}

/// FNV-1a of the user and the PIN: not encryption (the card is in the
/// clear), only a PIN that is not written down.
pub fn pin_hash(id: &str, pin: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes().chain(core::iter::once(b':')).chain(pin.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn dir(id: &str) -> String {
    format!("{}/{}", ROOT, id)
}

pub fn playtime_path(id: &str) -> String {
    format!("{}/{}/playtime.cfg", ROOT, id)
}

/// What a game keeps for this user (`NXK_SAVE_DIR`).
pub fn save_dir(id: &str, game: &str) -> String {
    format!("{}/{}/saves/{}", ROOT, id, game)
}

/// All the directories on the way to `path`, as `mkdir -p`.
pub fn mkdirs(path: &str) {
    let mut at = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        at.push('/');
        at.push_str(part);
        let _ = fs::mkdir(&at);
    }
}

/// `path` and all it holds.
pub fn remove_tree(path: &str) -> Result<(), i64> {
    if let Ok(entries) = fs::read_dir(path) {
        for e in entries {
            let p = format!("{}/{}", path, e.name);
            match e.stat.kind {
                Kind::Dir => remove_tree(&p)?,
                Kind::File => fs::remove_file(&p)?,
            }
        }
    }
    fs::remove_dir(path)
}

/// Bytes in the files under `path`.
pub fn tree_size(path: &str, depth: usize) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .iter()
        .map(|e| match e.stat.kind {
            Kind::File => e.stat.size,
            Kind::Dir if depth > 0 => tree_size(&format!("{}/{}", path, e.name), depth - 1),
            Kind::Dir => 0,
        })
        .sum()
}

/// The games `id` has data for, with its size, by name.
pub fn saves(id: &str) -> Vec<(String, u64)> {
    let base = format!("{}/{}/saves", ROOT, id);
    let mut v: Vec<(String, u64)> = fs::read_dir(&base)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.stat.kind == Kind::Dir)
        .map(|e| {
            let size = tree_size(&format!("{}/{}", base, e.name), 4);
            (e.name, size)
        })
        .collect();
    v.sort();
    v
}

/// Writes `profile.cfg`, making its directory.
pub fn write_profile(p: &Profile) -> Result<(), i64> {
    mkdirs(&p.dir());
    fs::write(&format!("{}/profile.cfg", p.dir()), p.text().as_bytes())
}

/// The users on the card, in creation order, and the last session's;
/// `None` while the card is not readable.
pub fn load() -> Option<(Vec<Profile>, Option<String>)> {
    let entries = match fs::read_dir(ROOT) {
        Ok(e) => e,
        // No directory yet counts only once the card is mounted.
        Err(e) if e == -ENOENT && fs::statfs().is_ok() => Vec::new(),
        Err(_) => return None,
    };
    let mut list: Vec<Profile> = entries
        .iter()
        .filter(|e| e.stat.kind == Kind::Dir)
        .filter_map(|e| {
            let t = fs::read(&format!("{}/{}/profile.cfg", ROOT, e.name)).ok()?;
            Some(Profile::parse(&e.name, &String::from_utf8_lossy(&t)))
        })
        .collect();
    list.sort_by_key(|p| id_number(&p.id));
    let last = fs::read(LAST)
        .ok()
        .map(|t| String::from_utf8_lossy(&t).trim().into());
    Some((list, last))
}

pub fn write_last(id: &str) -> Result<(), i64> {
    let _ = fs::mkdir(ROOT);
    fs::write(LAST, id.as_bytes())
}

fn id_number(id: &str) -> u32 {
    id.trim_start_matches('u').parse().unwrap_or(u32::MAX)
}

/// What the screen knows of the users.
#[derive(Default)]
pub struct Users {
    pub list: Vec<Profile>,
    /// The user playing.
    pub current: Option<usize>,
    /// Protected users whose PIN was typed this session.
    unlocked: Vec<String>,
    /// The current user's profile changed (its theme): write it.
    dirty: bool,
}

impl Users {
    pub fn new(list: Vec<Profile>) -> Self {
        Self {
            list,
            current: None,
            unlocked: Vec::new(),
            dirty: false,
        }
    }

    pub fn active(&self) -> Option<&Profile> {
        self.current.and_then(|i| self.list.get(i))
    }

    pub fn next_id(&self) -> String {
        (1u32..)
            .map(|n| format!("u{}", n))
            .find(|id| self.list.iter().all(|p| &p.id != id))
            .unwrap_or_default()
    }

    /// May the screen act on user `i` without a PIN?
    pub fn open(&self, i: usize) -> bool {
        match self.list.get(i) {
            Some(p) => p.pin.is_none() || self.current == Some(i) || self.unlocked.contains(&p.id),
            None => false,
        }
    }

    pub fn unlock(&mut self, i: usize) {
        if let Some(p) = self.list.get(i) {
            if !self.unlocked.contains(&p.id) {
                self.unlocked.push(p.id.clone());
            }
        }
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }

    /// The current user, if its profile needs writing.
    pub fn take_dirty(&mut self) -> Option<usize> {
        core::mem::take(&mut self.dirty)
            .then_some(self.current)
            .flatten()
    }

    /// Drops user `i`, keeping `current` on the same user.
    pub fn remove(&mut self, i: usize) {
        let gone = self.list.remove(i);
        self.unlocked.retain(|id| id != &gone.id);
        self.current = match self.current {
            Some(c) if c == i => None,
            Some(c) if c > i => Some(c - 1),
            c => c,
        };
    }
}

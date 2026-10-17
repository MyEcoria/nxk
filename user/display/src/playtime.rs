//! Time played per game, counted by display around the games it starts,
//! in `/config/playtime.cfg`: one `NAME total week week_time order` line
//! per game (seconds; `week`: the week of `week_time`, Monday first, 0
//! while the clock is unset; `order`: larger was started later).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::fs;

const DIR: &str = "/config";
const FILE: &str = "/config/playtime.cfg";
/// 2001-01-01: the clock service has set the wall clock past it.
const CLOCK_SET_S: u64 = 978_307_200;

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub total: u64,
    pub week: u64,
    pub week_time: u64,
    pub order: u64,
}

#[derive(Default)]
pub struct Playtime {
    games: Vec<Entry>,
}

/// Week number since 1970 (a Thursday) with weeks from Monday, or 0
/// when the wall clock is not set.
fn week_of(wall_ns: u64) -> u64 {
    let s = wall_ns / 1_000_000_000;
    if s < CLOCK_SET_S {
        0
    } else {
        (s / 86_400 + 3) / 7
    }
}

/// `7 HOURS`, `1 MINUTE`, ...
fn span(secs: u64) -> String {
    let (n, unit) = match secs {
        0..60 => return "LESS THAN A MINUTE".into(),
        60..3600 => (secs / 60, "MINUTE"),
        _ => (secs / 3600, "HOUR"),
    };
    format!("{} {}{}", n, unit, if n == 1 { "" } else { "S" })
}

impl Playtime {
    /// The file, or `None` while the card is not readable.
    pub fn load() -> Option<Self> {
        let text = match fs::read(FILE) {
            Ok(t) => String::from_utf8_lossy(&t).into_owned(),
            Err(e) if e == -libnx::abi::err::ENOENT && fs::statfs().is_ok() => String::new(),
            Err(_) => return None,
        };
        let games = text
            .lines()
            .filter_map(|l| {
                let mut f = l.split_ascii_whitespace();
                let name = f.next()?.into();
                let mut n = || f.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                Some(Entry {
                    name,
                    total: n(),
                    week: n(),
                    week_time: n(),
                    order: n(),
                })
            })
            .collect();
        Some(Self { games })
    }

    /// The file's text.
    pub fn text(&self) -> String {
        self.games
            .iter()
            .map(|e| {
                format!(
                    "{} {} {} {} {}\n",
                    e.name, e.total, e.week, e.week_time, e.order
                )
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.games.iter().find(|e| e.name == name)
    }

    /// Name of the most recently started game, if any.
    pub fn last(&self) -> Option<&str> {
        self.games
            .iter()
            .filter(|e| e.order > 0)
            .max_by_key(|e| e.order)
            .map(|e| e.name.as_str())
    }

    fn entry(&mut self, name: &str) -> &mut Entry {
        let i = match self.games.iter().position(|e| e.name == name) {
            Some(i) => i,
            None => {
                self.games.push(Entry {
                    name: name.into(),
                    total: 0,
                    week: 0,
                    week_time: 0,
                    order: 0,
                });
                self.games.len() - 1
            }
        };
        &mut self.games[i]
    }

    /// `name` starts: it becomes the last played.
    pub fn started(&mut self, name: &str) {
        let next = self.games.iter().map(|e| e.order).max().unwrap_or(0) + 1;
        self.entry(name).order = next;
    }

    /// `name` ran `secs` seconds, ending at `wall_ns`.
    pub fn ended(&mut self, name: &str, secs: u64, wall_ns: u64) {
        let week = week_of(wall_ns);
        let e = self.entry(name);
        e.total += secs;
        if e.week != week {
            e.week = week;
            e.week_time = 0;
        }
        e.week_time += secs;
    }

    /// The home row's line above a game: this week's time, else the
    /// total, in capitals.
    pub fn label(&self, name: &str, wall_ns: u64) -> String {
        match self.get(name) {
            Some(e) if e.order > 0 || e.total > 0 => {
                let week = week_of(wall_ns);
                if week != 0 && e.week == week {
                    format!("THIS WEEK: {}", span(e.week_time))
                } else {
                    format!("PLAYED: {}", span(e.total))
                }
            }
            _ => "NOT PLAYED YET".into(),
        }
    }
}

pub fn save(text: &str) -> Result<(), i64> {
    let _ = fs::mkdir(DIR);
    fs::write(FILE, text.as_bytes())
}

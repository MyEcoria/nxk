//! The library kept on the card, in the catalog's own directory: the games
//! with their names (`library.json`) and each cover's JPEG as Amazon sent
//! it (`cover-HASH.jpg`), so the home screen has them at once at the next
//! start, with no network, and only new covers are downloaded. A cover
//! file is named after its game and its address: a new address is a new
//! file. Ids may hold `:`, which FAT refuses, hence the hash.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalog;
use crate::cover;

const FILE: &str = "library.json";

/// A game as kept: what the home screen shows, and where its cover comes from.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub genres: Vec<String>,
    /// The picture the cover is made from (`cover::source_url`).
    #[serde(default)]
    pub cover_url: Option<String>,
}

impl Entry {
    /// `g`, its cover to be a `side` square.
    pub fn of(g: &catalog::Game, side: u32) -> Self {
        Self {
            id: g.id.clone(),
            title: g.title.clone(),
            publisher: g.publisher.clone().unwrap_or_default(),
            genres: g.genres.clone(),
            cover_url: cover::source_url(g.portrait.as_deref(), g.landscape.as_deref(), side),
        }
    }

    /// The name of the file holding this game's cover, if it has an address.
    pub fn cover_file(&self) -> Option<String> {
        let url = self.cover_url.as_deref()?;
        Some(format!("cover-{:016x}.jpg", fnv1a(&self.id, url)))
    }
}

/// FNV-1a of the id and the address (a name, not a secret).
fn fnv1a(id: &str, url: &str) -> u64 {
    id.bytes()
        .chain(std::iter::once(0))
        .chain(url.bytes())
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// The games kept, in the order they are shown.
#[derive(Clone, Default, PartialEq, Debug, Serialize, Deserialize)]
pub struct Library {
    pub games: Vec<Entry>,
}

impl Library {
    pub fn of(games: &[catalog::Game], side: u32) -> Self {
        Self {
            games: games.iter().map(|g| Entry::of(g, side)).collect(),
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.games.iter().any(|e| e.id == id)
    }
}

/// The library on the card; empty if there is none or it is unreadable.
pub fn load(dir: &Path) -> Library {
    let p = dir.join(FILE);
    fs::read(&p)
        .or_else(|_| fs::read(p.with_extension("json.new")))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Written whole beside, then put in place: a cut never leaves half a file.
fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let new = path.with_extension(format!(
        "{}.new",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    fs::write(&new, bytes)?;
    let _ = fs::remove_file(path);
    fs::rename(&new, path)
}

pub fn save(dir: &Path, lib: &Library) -> io::Result<()> {
    let text = serde_json::to_vec(lib).map_err(io::Error::other)?;
    write_whole(&dir.join(FILE), &text)
}

/// The JPEG kept for `e`'s cover, if any.
pub fn read_cover(dir: &Path, e: &Entry) -> Option<Vec<u8>> {
    fs::read(dir.join(e.cover_file()?)).ok()
}

pub fn write_cover(dir: &Path, e: &Entry, jpeg: &[u8]) -> io::Result<()> {
    let name = e.cover_file().ok_or(io::ErrorKind::InvalidInput)?;
    write_whole(&dir.join(name), jpeg)
}

/// The cover files of `old` that `new` no longer uses are removed.
pub fn prune(dir: &Path, old: &Library, new: &Library) {
    let kept: Vec<String> = new.games.iter().filter_map(Entry::cover_file).collect();
    for name in old.games.iter().filter_map(Entry::cover_file) {
        if !kept.contains(&name) {
            let _ = fs::remove_file(dir.join(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(id: &str, url: Option<&str>) -> Entry {
        Entry {
            id: id.into(),
            title: format!("Game {}", id),
            publisher: "Pub".into(),
            genres: vec!["Action".into(), "Indie".into()],
            cover_url: url.map(str::to_owned),
        }
    }

    /// An empty directory of its own.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lunacore-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_cover_file_follows_the_game_and_the_address() {
        let a = entry("amzn1.g:1", Some("https://x/a._SL384_.png"));
        let name = a.cover_file().unwrap();
        assert_eq!(name, a.cover_file().unwrap());
        assert!(name.starts_with("cover-") && name.ends_with(".jpg"));
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'));
        assert_ne!(
            Some(name.clone()),
            entry("amzn1.g:2", a.cover_url.as_deref()).cover_file()
        );
        assert_ne!(
            Some(name),
            entry("amzn1.g:1", Some("https://x/b._SL384_.png")).cover_file()
        );
        assert_eq!(entry("g", None).cover_file(), None);
    }

    #[test]
    fn the_library_comes_back_as_it_was_saved() {
        let d = scratch("roundtrip");
        assert_eq!(load(&d), Library::default());
        let lib = Library {
            games: vec![entry("a", Some("https://x/a.png")), entry("b", None)],
        };
        save(&d, &lib).unwrap();
        assert_eq!(load(&d), lib);
        assert!(lib.has("b") && !lib.has("c"));
        // Garbage is an empty library, not a failure.
        fs::write(d.join(FILE), b"{ not json").unwrap();
        assert_eq!(load(&d), Library::default());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cut_save_leaves_the_last_whole_library() {
        let d = scratch("cut");
        let lib = Library {
            games: vec![entry("a", None)],
        };
        // The new file written but not yet put in place.
        fs::write(
            d.join("library.json.new"),
            serde_json::to_vec(&lib).unwrap(),
        )
        .unwrap();
        assert_eq!(load(&d), lib);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn covers_are_kept_and_the_ones_left_out_are_removed() {
        let d = scratch("covers");
        let (a, b) = (
            entry("a", Some("https://x/a.png")),
            entry("b", Some("https://x/b.png")),
        );
        assert_eq!(read_cover(&d, &a), None);
        write_cover(&d, &a, b"jpeg a").unwrap();
        write_cover(&d, &b, b"jpeg b").unwrap();
        assert_eq!(read_cover(&d, &a).as_deref(), Some(&b"jpeg a"[..]));
        let old = Library {
            games: vec![a.clone(), b.clone()],
        };
        // `a` stays with the same picture, `b` left, `c` is new.
        let new = Library {
            games: vec![a.clone(), entry("c", Some("https://x/c.png"))],
        };
        prune(&d, &old, &new);
        assert!(read_cover(&d, &a).is_some());
        assert_eq!(read_cover(&d, &b), None);
        assert!(write_cover(&d, &entry("n", None), b"x").is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_catalog_game_becomes_an_entry_with_its_cover_address() {
        let g: catalog::Game = serde_json::from_value(serde_json::json!({
            "gameId": "g1", "title": "T", "publishersText": "P",
            "genreTags": ["Racing"], "imagePortrait": "https://m.media-amazon.com/images/I/p.png"
        }))
        .unwrap();
        let e = Entry::of(&g, 256);
        assert_eq!(
            (e.id.as_str(), e.title.as_str(), e.publisher.as_str()),
            ("g1", "T", "P")
        );
        assert_eq!(e.genres, ["Racing"]);
        assert_eq!(
            e.cover_url.as_deref(),
            Some("https://m.media-amazon.com/images/I/p._SL384_.png")
        );
    }
}

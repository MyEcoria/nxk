//! What `display` knows of a catalog, kept on the card so that its tab shows
//! at once, before the program is up: the list of games and the covers it
//! read, per user and catalog, in `/users/ID/cache/NAME/`. A thread of its
//! own reads and writes them (the card is slow, and a Wi-Fi scan holds the
//! worker for seconds); the main loop only posts jobs and takes results.
//!
//! `list.bin`: `NXC2`, the count, then per game five strings (u16 length,
//! bytes: id, title, publisher, version, description), the color and a
//! flags byte (1: cloud). `NXC1` (no flags byte) is read as local games.
//! `cHASH.bin`: a cover: u16 id length, the id (the hash of it names the
//! file, since FAT refuses some characters of an id), then the catalog's
//! cover bytes as `display` received them (RGB565, no decoding).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::proto::provider as proto;
use libnx::fs;
use libnx::sync::{Condvar, Mutex};
use libnx::{sys, thread};

use crate::users;

/// The cache thread's notification bit for `display`.
pub const EV_CACHE: u64 = 32;
const MAGIC: &[u8; 4] = b"NXC2";
const MAGIC_V1: &[u8; 4] = b"NXC1";
const FLAG_CLOUD: u8 = 1;
const COVER_BYTES: usize = 4 + 2 * proto::COVER_MAX * proto::COVER_MAX;

/// A game as kept: what the catalog said, without the cover.
#[derive(Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub publisher: String,
    pub version: String,
    pub desc: String,
    pub color: u32,
    pub cloud: bool,
}

/// The directory of user `user`'s cache of catalog `service`.
pub fn dir(user: &str, service: &str) -> String {
    format!("{}/cache/{}", users::dir(user), service)
}

/// FNV-1a of an id: a file name that is always valid.
fn hash(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn cover_path(dir: &str, id: &str) -> String {
    format!("{}/c{:016x}.bin", dir, hash(id))
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    let n = s.len().min(u16::MAX as usize);
    b.extend_from_slice(&(n as u16).to_le_bytes());
    b.extend_from_slice(&s.as_bytes()[..n]);
}

pub fn encode_list(games: &[Entry]) -> Vec<u8> {
    let mut b = MAGIC.to_vec();
    b.extend_from_slice(&(games.len() as u32).to_le_bytes());
    for g in games {
        for s in [&g.id, &g.title, &g.publisher, &g.version, &g.desc] {
            put_str(&mut b, s);
        }
        b.extend_from_slice(&g.color.to_le_bytes());
        b.push(if g.cloud { FLAG_CLOUD } else { 0 });
    }
    b
}

/// A cursor over the file's bytes.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<String> {
        let n = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        Some(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

pub fn decode_list(b: &[u8]) -> Option<Vec<Entry>> {
    let mut r = Reader(b);
    let v1 = match r.take(4)? {
        m if m == MAGIC => false,
        m if m == MAGIC_V1 => true,
        _ => return None,
    };
    let n = r.u32()? as usize;
    if n > proto::MAX_GAMES {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let g = Entry {
            id: r.str()?,
            title: r.str()?,
            publisher: r.str()?,
            version: r.str()?,
            desc: r.str()?,
            color: r.u32()?,
            cloud: !v1 && r.u8()? & FLAG_CLOUD != 0,
        };
        if !proto::valid_id(&g.id) || out.iter().any(|o: &Entry| o.id == g.id) {
            return None;
        }
        out.push(g);
    }
    Some(out)
}

pub fn encode_cover(id: &str, cover: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + id.len() + cover.len());
    put_str(&mut b, id);
    b.extend_from_slice(cover);
    b
}

/// The cover of file `b` if it is the one of `id` and well formed: square,
/// at most `COVER_MAX`, all there.
pub fn decode_cover(id: &str, b: &[u8]) -> Option<Vec<u8>> {
    let mut r = Reader(b);
    if r.str()? != id {
        return None;
    }
    let c = r.0;
    let side = u16::from_le_bytes(c.get(..2)?.try_into().ok()?) as usize;
    let ok = side > 0
        && side <= proto::COVER_MAX
        && u16::from_le_bytes(c.get(2..4)?.try_into().ok()?) as usize == side
        && c.len() == 4 + 2 * side * side
        && c.len() <= COVER_BYTES;
    ok.then(|| c.to_vec())
}

/// The file `path`, written whole under a temporary name first.
fn write_atomic(path: &str, data: &[u8]) {
    let tmp = format!("{}.tmp", path);
    if fs::write(&tmp, data).is_ok() {
        let _ = fs::remove_file(path);
        let _ = fs::rename(&tmp, path);
    }
}

/// What is known of one user's cache of one catalog.
#[derive(Default)]
pub struct Kept {
    /// The list was asked for (`Job::LoadList`).
    pub asked: bool,
    /// The card answered (`Out::List`).
    pub loaded: bool,
    pub games: Option<Vec<Entry>>,
    /// Ids whose cover is kept.
    pub have: Vec<String>,
}

pub enum Job {
    /// The list, and which covers are on the card.
    LoadList(String, String),
    /// Covers `ids` of list version `list`.
    LoadCovers(String, String, u64, Vec<String>),
    SaveList(String, String, Vec<Entry>),
    SaveCover(String, String, String, Vec<u8>),
    /// Delete the covers of the games that are not in `keep`.
    Prune(String, String, Vec<String>),
    /// Delete everything kept for the catalog (its account is gone).
    Forget(String, String),
}

pub enum Out {
    /// User, catalog, the list (none: no usable file) and the ids whose
    /// cover is kept.
    List(String, String, Option<Vec<Entry>>, Vec<String>),
    Cover(String, String, u64, String, Vec<u8>),
    /// These covers are not kept.
    Absent(String, String, u64, Vec<String>),
}

pub struct Cache {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    out: Mutex<VecDeque<Out>>,
    notif: u32,
}

impl Cache {
    pub fn start(notif: u32) -> alloc::sync::Arc<Self> {
        let c = alloc::sync::Arc::new(Self {
            jobs: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            out: Mutex::new(VecDeque::new()),
            notif,
        });
        let t = c.clone();
        let _ = thread::spawn(move || t.run());
        c
    }

    pub fn post(&self, j: Job) {
        self.jobs.lock().push_back(j);
        self.wake.notify_one();
    }

    pub fn take(&self) -> Vec<Out> {
        self.out.lock().drain(..).collect()
    }

    fn give(&self, o: Out) {
        self.out.lock().push_back(o);
        let _ = sys::notify_signal(self.notif, EV_CACHE);
    }

    fn run(&self) {
        loop {
            let job = {
                let mut q = self.jobs.lock();
                loop {
                    if let Some(j) = q.pop_front() {
                        break j;
                    }
                    q = self.wake.wait(q);
                }
            };
            self.one(job);
        }
    }

    fn one(&self, job: Job) {
        match job {
            Job::LoadList(user, svc) => {
                let d = dir(&user, &svc);
                let list = fs::read(&format!("{}/list.bin", d))
                    .ok()
                    .and_then(|b| decode_list(&b));
                let have = list
                    .iter()
                    .flatten()
                    .filter(|g| fs::stat(&cover_path(&d, &g.id)).is_ok())
                    .map(|g| g.id.clone())
                    .collect();
                self.give(Out::List(user, svc, list, have));
            }
            Job::LoadCovers(user, svc, list, ids) => {
                let d = dir(&user, &svc);
                let mut absent = Vec::new();
                for id in ids {
                    let c = fs::read(&cover_path(&d, &id))
                        .ok()
                        .and_then(|b| decode_cover(&id, &b));
                    match c {
                        Some(c) => self.give(Out::Cover(user.clone(), svc.clone(), list, id, c)),
                        None => absent.push(id),
                    }
                }
                if !absent.is_empty() {
                    self.give(Out::Absent(user, svc, list, absent));
                }
            }
            Job::SaveList(user, svc, games) => {
                let d = dir(&user, &svc);
                users::mkdirs(&d);
                write_atomic(&format!("{}/list.bin", d), &encode_list(&games));
            }
            Job::SaveCover(user, svc, id, bytes) => {
                let d = dir(&user, &svc);
                users::mkdirs(&d);
                write_atomic(&cover_path(&d, &id), &encode_cover(&id, &bytes));
            }
            Job::Prune(user, svc, keep) => {
                let d = dir(&user, &svc);
                let wanted: Vec<String> = keep
                    .iter()
                    .map(|id| format!("c{:016x}.bin", hash(id)))
                    .collect();
                for e in fs::read_dir(&d).unwrap_or_default() {
                    let cover = e.name.starts_with('c') && e.name.ends_with(".bin");
                    if (cover && !wanted.contains(&e.name)) || e.name.ends_with(".tmp") {
                        let _ = fs::remove_file(&format!("{}/{}", d, e.name));
                    }
                }
            }
            Job::Forget(user, svc) => {
                let _ = users::remove_tree(&dir(&user, &svc));
            }
        }
    }
}

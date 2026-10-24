//! Game catalogs from the card ("services" on screen, `abi::proto::
//! provider`): `/services/NAME/NAME` and its `service.txt`. The one the
//! user picked runs while it is picked; two threads of ours talk to it
//! (its state, then the commands and covers), so a slow or broken
//! catalog never holds the UI. Its replies are checked, never trusted.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use libnx::abi::err::{ENOENT, ESTALE, ETIMEDOUT};
use libnx::abi::proto::provider as proto;
use libnx::fs;
use libnx::ipc::{self, Msg};
use libnx::sync::{Condvar, Mutex};
use libnx::{sys, thread, time};

use crate::games::{Cover, GameInfo};
use crate::users::{self, Profile};

const DIR: &str = "/services";
/// A catalog's main notification bit for `display` (state, list, covers,
/// its end).
pub const EV_PROV: u64 = 8;
/// How long `STATE` waits for news, and our call around it.
const POLL_NS: u64 = 5_000_000_000;
const CALL_NS: u64 = 1_000_000_000;
const LAUNCH_NS: u64 = 2_000_000_000;
/// A cover's bytes at most.
const COVER_BYTES: usize = 4 + 2 * proto::COVER_MAX * proto::COVER_MAX;

/// A catalog on the card.
#[derive(Clone)]
pub struct ServiceInfo {
    /// The directory's name: what a user's profile keeps.
    pub name: String,
    pub title: String,
}

/// The catalogs, by title.
pub fn scan() -> Vec<ServiceInfo> {
    let mut out = Vec::new();
    for e in fs::read_dir(DIR).unwrap_or_default() {
        if fs::stat(&exe(&e.name)).is_err() {
            continue;
        }
        let mut title: String = e.name.chars().take(24).collect();
        if let Ok(t) = fs::read(&format!("{}/{}/service.txt", DIR, e.name)) {
            for line in String::from_utf8_lossy(&t).lines() {
                if let Some(("title", v)) = line.split_once('=').map(|(k, v)| (k.trim(), v)) {
                    title = v.trim().chars().take(24).collect();
                }
            }
        }
        out.push(ServiceInfo {
            name: e.name,
            title,
        });
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

fn exe(name: &str) -> String {
    format!("{}/{}/{}", DIR, name, name)
}

/// What the home screen shows of the catalog.
#[derive(Clone)]
pub struct View {
    /// Its version (0: made up by `display`, not the catalog's).
    pub version: u64,
    pub state: u64,
    pub text: String,
    /// `ST_LAUNCHING`: the place in the queue (0: none).
    pub place: u64,
    /// The covers' version: a new one may answer an `ENOENT`.
    pub covers: u64,
}

impl View {
    /// A state of `display`'s own (the catalog not running).
    pub fn local(state: u64, text: String) -> Self {
        Self {
            version: 0,
            state,
            text,
            place: 0,
            covers: 0,
        }
    }
}

enum Cmd {
    Launch(String),
    Stop,
    /// Covers of these games (ids) of list version `.0`.
    Covers(u64, Vec<String>),
    /// The sign-in image (`ART_LOGIN`).
    LoginArt,
}

/// What changed since the last `take`.
#[derive(Default)]
pub struct Update {
    pub view: Option<View>,
    /// A new list and its version.
    pub games: Option<(u64, Vec<GameInfo>)>,
    /// Covers: list version, game id, the cover.
    pub covers: Vec<(u64, String, Vec<u8>)>,
    /// Covers not read: list version, game id, and whether the catalog
    /// has none yet (`ENOENT`) rather than the read being cut short.
    pub covers_missed: Vec<(u64, String, bool)>,
    /// A `LAUNCH` taken: the version of its `ST_LAUNCHING` state.
    pub launched: Option<u64>,
    /// A `STOP` answered.
    pub stopped: bool,
    /// The sign-in image read again: the image, or none.
    pub login_art: Option<Option<Vec<u8>>>,
    /// Calls that failed, for a toast.
    pub notes: Vec<String>,
    /// `STOP` got no answer in time: the catalog has to go.
    pub stop_failed: bool,
    /// It ended, with this code.
    pub exit: Option<i64>,
    /// It could not be spawned (the errno).
    pub start_failed: Option<i64>,
}

struct Shared {
    /// Its endpoint (0 until its hello).
    ep: u32,
    quit: bool,
    cmds: VecDeque<Cmd>,
    up: Update,
    /// The list last read: its version and ids, in the catalog's order.
    ids: (u64, Vec<String>),
}

struct Link {
    sh: Mutex<Shared>,
    wake: Condvar,
    notif: u32,
    /// The process, 0 until `launch` has it; closed with the last user of
    /// the link.
    proc: AtomicU32,
}

impl Link {
    fn post(&self, f: impl FnOnce(&mut Update)) {
        f(&mut self.sh.lock().up);
        let _ = sys::notify_signal(self.notif, EV_PROV);
    }

    fn quit(&self) -> bool {
        self.sh.lock().quit
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let ep = self.sh.lock().ep;
        if ep != 0 {
            let _ = sys::handle_close(ep);
        }
        let h = self.proc.load(Ordering::Acquire);
        if h != 0 {
            let _ = sys::handle_close(h);
        }
    }
}

/// The running catalog, for the sleep steps (they run on the worker).
static SLEEPER: Mutex<Option<Arc<Link>>> = Mutex::new(None);

/// Its `SUSPEND` or `RESUME`, if a catalog runs.
pub fn tell_sleeper(label: u32, timeout_ns: u64) {
    let link = SLEEPER.lock().clone();
    if let Some(l) = link {
        let ep = l.sh.lock().ep;
        if ep != 0 {
            let _ = ipc::call_timeout(ep, &Msg::new(label, 0, 0), timeout_ns);
        }
    }
}

/// A catalog started for a user.
pub struct Provider {
    pub name: String,
    pub title: String,
    pub user: String,
    key: u64,
    pid: Option<u32>,
    pub since: u64,
    link: Arc<Link>,
}

impl Provider {
    /// Start catalog `s` for user `p`: its data under `/users/ID/services/NAME`.
    /// Returns at once: the program is read from the card and spawned by a
    /// thread of ours (init reads it whole, which holds the caller for
    /// hundreds of ms), its failure comes back as `Update::start_failed`.
    pub fn start(s: &ServiceInfo, p: &Profile, notif: u32) -> Result<Self, i64> {
        let mut k = [0u8; 8];
        sys::getrandom(&mut k);
        let key = u64::from_le_bytes(k) >> 1;
        let dir = format!("{}/services/{}", p.dir(), s.name);
        let mut env = libnx::process::current_env();
        env.push(format!("{}={}", proto::KEY_ENV, key));
        env.push(format!("{}={}", proto::DIR_ENV, dir));
        env.push(format!("NXK_USER={}", p.name));
        env.push(format!("NXK_USER_ID={}", p.id));
        let link = Arc::new(Link {
            sh: Mutex::new(Shared {
                ep: 0,
                quit: false,
                cmds: VecDeque::new(),
                up: Update::default(),
                ids: (0, Vec::new()),
            }),
            wake: Condvar::new(),
            notif,
            proc: AtomicU32::new(0),
        });
        let (path, l) = (exe(&s.name), link.clone());
        let threads = [
            thread::spawn({
                let l = link.clone();
                move || watch_state(&l)
            }),
            thread::spawn({
                let l = link.clone();
                move || run_cmds(&l)
            }),
            thread::spawn(move || launch(&l, &path, &dir, &env)),
        ];
        if threads.iter().any(|t| t.is_err()) {
            link.sh.lock().quit = true;
            link.wake.notify_all();
            return Err(-libnx::abi::err::EAGAIN);
        }
        Ok(Self {
            name: s.name.clone(),
            title: s.title.clone(),
            user: p.id.clone(),
            key,
            pid: None,
            since: time::now_ns(),
            link,
        })
    }

    /// Its hello (`display::PROVIDER`): its endpoint, kept if the key is
    /// the one it was given and it is the first.
    pub fn hello(&mut self, key: u64, pid: u32, ep: u32) -> bool {
        if key != self.key || self.pid.is_some() || ep == 0 {
            return false;
        }
        self.pid = Some(pid);
        self.link.sh.lock().ep = ep;
        self.link.wake.notify_all();
        *SLEEPER.lock() = Some(self.link.clone());
        true
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn take(&self) -> Update {
        core::mem::take(&mut self.link.sh.lock().up)
    }

    fn command(&self, c: Cmd) {
        let mut s = self.link.sh.lock();
        // A newer cover request replaces the waiting one.
        if matches!(c, Cmd::Covers(..)) {
            s.cmds.retain(|c| !matches!(c, Cmd::Covers(..)));
        }
        s.cmds.push_back(c);
        drop(s);
        self.link.wake.notify_all();
    }

    pub fn launch(&self, id: &str) {
        self.command(Cmd::Launch(id.into()));
    }

    pub fn stop(&self) {
        self.command(Cmd::Stop);
    }

    /// Read the sign-in image again (its state changed).
    pub fn login_art(&self) {
        let mut s = self.link.sh.lock();
        s.cmds.retain(|c| !matches!(c, Cmd::LoginArt));
        s.cmds.push_back(Cmd::LoginArt);
        drop(s);
        self.link.wake.notify_all();
    }

    pub fn covers(&self, list: u64, games: Vec<String>) {
        self.command(Cmd::Covers(list, games));
    }

    /// End it; one still being spawned dies as soon as `launch` has it.
    pub fn kill(&self) {
        self.link.sh.lock().quit = true;
        self.link.wake.notify_all();
        let h = self.link.proc.load(Ordering::Acquire);
        if h != 0 {
            let _ = sys::process_kill(h);
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.kill();
        let mut s = SLEEPER.lock();
        if s.as_ref().is_some_and(|l| Arc::ptr_eq(l, &self.link)) {
            *s = None;
        }
    }
}

/// Spawn the catalog, then wait for its end.
fn launch(l: &Arc<Link>, path: &str, dir: &str, env: &[String]) {
    users::mkdirs(dir);
    let spawned = if l.quit() {
        Err(-libnx::abi::err::EINTR)
    } else {
        libnx::process::spawn_with(&libnx::process::Strings::new(&[path], env).kill_on_close())
    };
    let h = match spawned {
        Ok(h) => h,
        Err(e) => {
            l.sh.lock().quit = true;
            l.wake.notify_all();
            l.post(|u| u.start_failed = Some(e));
            return;
        }
    };
    l.proc.store(h, Ordering::Release);
    // Another catalog was picked meanwhile: `kill` did not see the process.
    if l.quit() {
        let _ = sys::process_kill(h);
    }
    let code = libnx::process::wait(h);
    l.sh.lock().quit = true;
    l.wake.notify_all();
    l.post(|u| u.exit = Some(code));
}

/// Its endpoint once it said hello; `None` once it is gone.
fn endpoint(l: &Link) -> Option<u32> {
    let mut s = l.sh.lock();
    loop {
        if s.quit {
            return None;
        }
        if s.ep != 0 {
            return Some(s.ep);
        }
        s = l.wake.wait(s);
    }
}

/// The state, waiting for news; the list read again when it changed.
fn watch_state(l: &Arc<Link>) {
    let Some(ep) = endpoint(l) else {
        return;
    };
    let (mut known, mut list) = (0u64, 0u64);
    let mut text = alloc::vec![0u8; proto::MAX_TEXT];
    while !l.quit() {
        let m = Msg::from_words(proto::STATE, &[known, POLL_NS]);
        let r = match ipc::call_timeout(ep, &m, POLL_NS + CALL_NS).and_then(ipc::checked) {
            Ok(r) => r,
            Err(e) if e == -ETIMEDOUT => continue,
            Err(_) => {
                time::sleep_ms(500);
                continue;
            }
        };
        let w: [u64; 6] = core::array::from_fn(|i| ipc::header(&r, i));
        let n = ipc::unpack_at(&r, 6, &mut text);
        let view = View {
            version: w[0],
            state: w[1],
            text: String::from_utf8_lossy(&text[..n])
                .chars()
                .take(300)
                .collect(),
            place: w[4],
            covers: w[5],
        };
        known = w[0];
        l.post(|u| u.view = Some(view));
        if w[2] != list {
            if let Some(games) = read_list(ep, w[2], w[3] as usize) {
                list = w[2];
                l.sh.lock().ids = (list, games.iter().map(|g| g.id.clone()).collect());
                l.post(|u| u.games = Some((list, games)));
            }
        }
    }
}

/// List version `list`: `None` if it changed meanwhile.
fn read_list(ep: u32, list: u64, count: usize) -> Option<Vec<GameInfo>> {
    let mut out: Vec<GameInfo> = Vec::new();
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    for i in 0..count.min(proto::MAX_GAMES) {
        let m = Msg::from_words(proto::GAME, &[i as u64, list]);
        let r = match ipc::call_timeout(ep, &m, CALL_NS).and_then(ipc::checked) {
            Ok(r) => r,
            Err(e) if e == -ESTALE => return None,
            Err(_) => continue,
        };
        let color = ipc::header(&r, 0) as u32 & 0xFF_FFFF;
        let n = ipc::unpack_at(&r, 2, &mut buf);
        let f: Vec<String> = buf[..n]
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let field = |k: usize, max: usize| -> String {
            f.get(k)
                .map_or(String::new(), |s| s.chars().take(max).collect())
        };
        let id = field(0, proto::MAX_ID + 1);
        if !proto::valid_id(&id) || out.iter().any(|g| g.id == id) {
            continue;
        }
        let title = field(1, 40);
        out.push(GameInfo {
            // `merge` names it after the catalog.
            name: String::new(),
            title: if title.is_empty() { id.clone() } else { title },
            publisher: field(2, 40),
            version: field(3, 16),
            desc: field(4, 80),
            color: crate::theme::rgb(color),
            exe: String::new(),
            id,
            cover: None,
        });
    }
    Some(out)
}

fn run_cmds(l: &Arc<Link>) {
    let Some(ep) = endpoint(l) else {
        return;
    };
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let cmd = {
            let mut s = l.sh.lock();
            loop {
                if s.quit {
                    return;
                }
                if let Some(c) = s.cmds.pop_front() {
                    break c;
                }
                s = l.wake.wait(s);
            }
        };
        match cmd {
            Cmd::Launch(id) => {
                let m = ipc::pack(proto::LAUNCH, id.as_bytes());
                match ipc::call_timeout(ep, &m, LAUNCH_NS).and_then(ipc::checked) {
                    Ok(r) => {
                        let v = ipc::header(&r, 0);
                        l.post(|u| u.launched = Some(v));
                    }
                    Err(e) => {
                        let note = format!("cannot start {}: {}", id, libnx::abi::err::name(e));
                        l.post(|u| u.notes.push(note));
                    }
                }
            }
            Cmd::Stop => {
                let m = Msg::new(proto::STOP, 0, 0);
                match ipc::call_timeout(ep, &m, proto::STOP_NS).and_then(ipc::checked) {
                    Ok(_) => l.post(|u| u.stopped = true),
                    Err(_) => l.post(|u| u.stop_failed = true),
                }
            }
            Cmd::Covers(list, games) => read_covers(l, ep, list, games, &mut buf),
            Cmd::LoginArt => {
                let art = |off: usize| Msg::from_words(proto::ART, &[proto::ART_LOGIN, off as u64]);
                let got = read_image(ep, art, &mut buf).ok();
                l.post(|u| u.login_art = Some(got));
            }
        }
    }
}

/// Covers `games` (ids) of list `list`, one by one; the ones not read go
/// back to the UI, which asks again (an `ENOENT` once the covers change).
fn read_covers(l: &Link, ep: u32, list: u64, games: Vec<String>, buf: &mut [u8]) {
    let mut left = games.into_iter();
    while let Some(id) = left.next() {
        let at = {
            let s = l.sh.lock();
            if s.quit || !s.cmds.is_empty() || s.ids.0 != list {
                None
            } else {
                Some(s.ids.1.iter().position(|g| *g == id))
            }
        };
        let Some(at) = at else {
            // Cut short by a newer request: the rest is asked again.
            let rest: Vec<(u64, String, bool)> = core::iter::once(id)
                .chain(left)
                .map(|id| (list, id, false))
                .collect();
            l.post(|u| u.covers_missed.extend(rest));
            return;
        };
        match at.map(|i| read_cover(ep, list, i, buf)) {
            Some(Ok(c)) => l.post(|u| u.covers.push((list, id, c))),
            Some(Err(e)) => l.post(|u| u.covers_missed.push((list, id, e == -ENOENT))),
            None => l.post(|u| u.covers_missed.push((list, id, true))),
        }
    }
}

/// Cover `i` of list `list` (see `read_image`).
fn read_cover(ep: u32, list: u64, i: usize, buf: &mut [u8]) -> Result<Vec<u8>, i64> {
    let cover = |off: usize| Msg::from_words(proto::COVER, &[i as u64, list, off as u64]);
    read_image(ep, cover, buf)
}

/// An image the catalog sends by parts (`ask(offset)`), checked: square,
/// `COVER_MAX` at most, all there (`EINVAL` when it is not).
fn read_image(ep: u32, ask: impl Fn(usize) -> Msg, buf: &mut [u8]) -> Result<Vec<u8>, i64> {
    let bad = -libnx::abi::err::EINVAL;
    let mut out: Vec<u8> = Vec::new();
    let mut total = 0usize;
    loop {
        let m = ask(out.len());
        let r = ipc::call_timeout(ep, &m, CALL_NS).and_then(ipc::checked)?;
        let size = ipc::header(&r, 0) as usize;
        if out.is_empty() {
            if !(5..=COVER_BYTES).contains(&size) {
                return Err(bad);
            }
            total = size;
            out.reserve_exact(total);
        } else if size != total {
            return Err(bad);
        }
        let n = ipc::unpack_at(&r, 1, buf);
        if n == 0 || out.len() + n > total {
            return Err(bad);
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() == total {
            break;
        }
    }
    let side = u16::from_le_bytes([out[0], out[1]]) as usize;
    let ok = side > 0
        && side <= proto::COVER_MAX
        && u16::from_le_bytes([out[2], out[3]]) as usize == side
        && total == 4 + 2 * side * side;
    if ok {
        Ok(out)
    } else {
        Err(bad)
    }
}

/// A catalog's games as the UI keeps them, named `NAME~ID` (their play
/// time's key): covers already read kept.
pub fn merge(name: &str, mut new: Vec<GameInfo>, old: &mut [GameInfo]) -> Vec<GameInfo> {
    for g in new.iter_mut() {
        g.name = format!("{}~{}", name, g.id);
        if let Some(o) = old.iter_mut().find(|o| o.id == g.id) {
            g.cover = o.cover.take();
        }
    }
    new
}

/// A checked cover for the UI.
pub fn cover(bytes: Vec<u8>) -> Cover {
    Cover::Direct(bytes)
}

//! Game catalog SDK: a catalog ("service" on screen) is a `std` program
//! `display` starts from `/services/NAME/NAME`. `connect` hands its
//! endpoint to `display` and serves `abi::proto::provider` on a thread
//! of its own; the program fills the `Catalog` and answers `Request`s.

pub mod screen;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use nxgfx::os::{self, Endpoint, Service};
use nxk_abi::err::{EINVAL, ENOENT, EPERM, ESTALE};
use nxk_abi::proto::{display, provider as proto};
use nxk_abi::rights;
use nxk_abi::user::{self, Msg};

pub use nxk_abi::proto::provider::valid_id;

pub type Result<T> = core::result::Result<T, i64>;

/// A game of the catalog.
#[derive(Clone, Default)]
pub struct Game {
    /// `[A-Za-z0-9._-]`, 1 to 40 bytes (`valid_id`).
    pub id: String,
    pub title: String,
    pub publisher: String,
    pub version: String,
    pub description: String,
    /// RGB, the card's color while it has no cover.
    pub color: u32,
}

/// A square cover: RGB565 pixels, rows from the top.
pub struct Cover {
    side: usize,
    px: Vec<u16>,
}

impl Cover {
    /// `side` x `side` pixels from `rgb(x, y)` (0xRRGGBB); `side` is cut to
    /// `COVER_MAX`.
    pub fn from_fn(side: usize, rgb: impl Fn(usize, usize) -> u32) -> Self {
        let side = side.clamp(1, proto::COVER_MAX);
        let px = (0..side * side)
            .map(|i| rgb565(rgb(i % side, i / side)))
            .collect();
        Self { side, px }
    }

    /// The protocol's bytes: side, side, pixels.
    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(4 + self.px.len() * 2);
        let s = (self.side as u16).to_le_bytes();
        b.extend_from_slice(&s);
        b.extend_from_slice(&s);
        for p in &self.px {
            b.extend_from_slice(&p.to_le_bytes());
        }
        b
    }
}

/// 0xRRGGBB to RGB565.
pub fn rgb565(rgb: u32) -> u16 {
    let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    ((r >> 3) << 11 | (g >> 2) << 5 | b >> 3) as u16
}

/// What the catalog shows on the home screen.
#[derive(Clone)]
pub enum State {
    Loading(String),
    /// The games are listed; the text, if any, says what just happened.
    Ready(String),
    /// An account is needed: the code and the URL, one per line.
    Login(String),
    /// A game is starting; `place` in the queue (0: none).
    Launching {
        text: String,
        place: u32,
    },
    Playing,
    Error(String),
}

impl State {
    fn code(&self) -> u64 {
        match self {
            State::Loading(_) => proto::ST_LOADING,
            State::Ready(_) => proto::ST_READY,
            State::Login(_) => proto::ST_LOGIN,
            State::Launching { .. } => proto::ST_LAUNCHING,
            State::Playing => proto::ST_PLAYING,
            State::Error(_) => proto::ST_ERROR,
        }
    }

    fn text(&self) -> &str {
        match self {
            State::Loading(t) | State::Ready(t) | State::Login(t) | State::Error(t) => t,
            State::Launching { text, .. } => text,
            State::Playing => "",
        }
    }

    fn place(&self) -> u64 {
        match self {
            State::Launching { place, .. } => *place as u64,
            _ => 0,
        }
    }
}

/// A request that needs an answer once done (`Done::done`).
pub struct Done(u64);

impl Done {
    pub fn done(self) {
        let _ = Endpoint::reply_to(self.0, &Msg::new(0, 0, 0));
    }
}

/// What `display` asks of the catalog.
pub enum Request {
    /// Start the game with this id (already checked to be listed).
    Launch(String),
    /// End the game or the launch: release the screen, close the rest.
    Stop(Done),
    /// The console sleeps: the network goes; end what needs it.
    Suspend(Done),
    Resume(Done),
}

struct Shared {
    version: u64,
    state: State,
    list_version: u64,
    games: Vec<Game>,
    covers: HashMap<String, Arc<Vec<u8>>>,
    /// `STATE` callers waiting for a new version: token, its deadline.
    waiters: Vec<(u64, u64)>,
}

/// The catalog as `display` sees it; cheap to clone, shared by threads.
#[derive(Clone)]
pub struct Catalog {
    shared: Arc<Mutex<Shared>>,
}

impl Catalog {
    pub fn set_state(&self, st: State) {
        let mut s = self.shared.lock().unwrap();
        s.state = st;
        s.version += 1;
        answer_waiters(&mut s, u64::MAX);
    }

    /// A new list (ids not `valid_id`, and past `MAX_GAMES`, left out).
    pub fn set_games(&self, games: Vec<Game>) {
        let mut s = self.shared.lock().unwrap();
        s.games = games
            .into_iter()
            .filter(|g| valid_id(&g.id))
            .take(proto::MAX_GAMES)
            .collect();
        let Shared { games, covers, .. } = &mut *s;
        covers.retain(|id, _| games.iter().any(|g| &g.id == id));
        s.list_version += 1;
        s.version += 1;
        answer_waiters(&mut s, u64::MAX);
    }

    /// Game `id`'s cover: `display` reads it when it shows the game.
    pub fn set_cover(&self, id: &str, c: &Cover) {
        self.shared
            .lock()
            .unwrap()
            .covers
            .insert(id.to_string(), Arc::new(c.encode()));
    }

    pub fn game(&self, id: &str) -> Option<Game> {
        let s = self.shared.lock().unwrap();
        s.games.iter().find(|g| g.id == id).cloned()
    }
}

/// The state reply, built on the calling thread (its UTCB).
fn state_msg(s: &Shared) -> Msg {
    let text = s.state.text().as_bytes();
    let text = &text[..floor_char(s.state.text(), proto::MAX_TEXT)];
    let head = [
        s.version,
        s.state.code(),
        s.list_version,
        s.games.len() as u64,
        s.state.place(),
    ];
    user::pack_at(0, &head, text).0
}

/// The longest prefix of `t` of at most `max` bytes ending on a char.
fn floor_char(t: &str, max: usize) -> usize {
    let mut n = t.len().min(max);
    while !t.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Reply to the waiters whose deadline is at or before `now` (all of
/// them with `u64::MAX`: the version changed).
fn answer_waiters(s: &mut Shared, now: u64) {
    let (due, kept): (Vec<_>, Vec<_>) = s.waiters.iter().partition(|w| w.1 <= now);
    s.waiters = kept;
    if due.is_empty() {
        return;
    }
    let m = state_msg(s);
    for (token, _) in due {
        let _ = Endpoint::reply_to(token, &m);
    }
}

/// `STATE` waits this long at most, whatever is asked.
const MAX_WAIT_NS: u64 = 30_000_000_000;

/// Hand the catalog to `display` (the key it gave in the environment)
/// and serve it. The requests come on the receiver.
pub fn connect() -> Result<(Catalog, Receiver<Request>)> {
    let key: u64 = std::env::var(proto::KEY_ENV)
        .ok()
        .and_then(|k| k.parse().ok())
        .ok_or(-EPERM)?;
    let ep = Endpoint::new()?;
    let send = os::handle_dup(ep.handle(), rights::SEND | rights::GRANT)?;
    let disp = Service::lookup("display")?;
    let said = disp.call_with(Msg::from_words(display::PROVIDER, &[key]), send);
    os::handle_close(send);
    said?;
    let cat = Catalog {
        shared: Arc::new(Mutex::new(Shared {
            version: 1,
            state: State::Loading(String::new()),
            list_version: 1,
            games: Vec::new(),
            covers: HashMap::new(),
            waiters: Vec::new(),
        })),
    };
    let (tx, rx) = mpsc::channel();
    let c2 = cat.clone();
    std::thread::spawn(move || serve(ep, c2, tx));
    Ok((cat, rx))
}

fn serve(ep: Endpoint, cat: Catalog, tx: Sender<Request>) {
    loop {
        let next = {
            let s = cat.shared.lock().unwrap();
            s.waiters.iter().map(|w| w.1).min()
        };
        let now = os::now_ns();
        let wait = next.map_or(0, |d| d.saturating_sub(now).max(1));
        let rx = match ep.recv(wait) {
            Ok(rx) => rx,
            Err(_) => {
                answer_waiters(&mut cat.shared.lock().unwrap(), os::now_ns());
                continue;
            }
        };
        let m = &rx.msg;
        let reply = match m.label() {
            proto::STATE => {
                let mut s = cat.shared.lock().unwrap();
                let (known, ns) = (user::header(m, 0), user::header(m, 1));
                if known != s.version || ns == 0 {
                    Some(state_msg(&s))
                } else {
                    let at = os::now_ns() + ns.min(MAX_WAIT_NS);
                    s.waiters.push((rx.token, at));
                    None
                }
            }
            proto::GAME => Some(game_msg(&cat, m)),
            proto::COVER => Some(cover_msg(&cat, m)),
            proto::LAUNCH => Some(launch(&cat, m, &tx)),
            proto::STOP => send(&tx, Request::Stop(Done(rx.token))),
            nxk_abi::proto::SUSPEND => send(&tx, Request::Suspend(Done(rx.token))),
            nxk_abi::proto::RESUME => send(&tx, Request::Resume(Done(rx.token))),
            _ => Some(Msg::error(-EINVAL)),
        };
        if let Some(r) = reply {
            let _ = Endpoint::reply_to(rx.token, &r);
        }
    }
}

/// A request answered later; at once if nobody reads them any more.
fn send(tx: &Sender<Request>, r: Request) -> Option<Msg> {
    match tx.send(r) {
        Ok(()) => None,
        Err(mpsc::SendError(r)) => {
            if let Request::Stop(d) | Request::Suspend(d) | Request::Resume(d) = r {
                d.done();
            }
            None
        }
    }
}

fn game_msg(cat: &Catalog, m: &Msg) -> Msg {
    let s = cat.shared.lock().unwrap();
    let (i, list) = (user::header(m, 0) as usize, user::header(m, 1));
    if list != s.list_version {
        return Msg::error(-ESTALE);
    }
    let Some(g) = s.games.get(i) else {
        return Msg::error(-ENOENT);
    };
    let mut b = Vec::new();
    for f in [&g.id, &g.title, &g.publisher, &g.version, &g.description] {
        let f = f.replace('\0', "");
        b.extend_from_slice(&f.as_bytes()[..floor_char(&f, 400)]);
        b.push(0);
    }
    user::pack_at(0, &[g.color as u64 & 0xFF_FFFF, 0], &b).0
}

fn cover_msg(cat: &Catalog, m: &Msg) -> Msg {
    let (i, list, off) = (
        user::header(m, 0) as usize,
        user::header(m, 1),
        user::header(m, 2) as usize,
    );
    let blob = {
        let s = cat.shared.lock().unwrap();
        if list != s.list_version {
            return Msg::error(-ESTALE);
        }
        s.games.get(i).and_then(|g| s.covers.get(&g.id)).cloned()
    };
    match blob {
        Some(b) if off <= b.len() => user::pack_at(0, &[b.len() as u64], &b[off..]).0,
        Some(_) => Msg::error(-EINVAL),
        None => Msg::error(-ENOENT),
    }
}

fn launch(cat: &Catalog, m: &Msg, tx: &Sender<Request>) -> Msg {
    let mut buf = [0u8; proto::MAX_ID + 1];
    let n = user::unpack_at(m, 0, &mut buf);
    let id = match std::str::from_utf8(&buf[..n]) {
        Ok(id) if valid_id(id) && cat.game(id).is_some() => id.to_string(),
        _ => return Msg::error(-ENOENT),
    };
    match tx.send(Request::Launch(id)) {
        Ok(()) => Msg::new(0, 0, 0),
        Err(_) => Msg::error(-EINVAL),
    }
}

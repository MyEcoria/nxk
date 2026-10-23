//! Amazon Luna as a game catalog of nxk (`abi::proto::provider`): the
//! sign-in by code, the games the account can play and their covers, and
//! playing them: the cloud session, the stream, the gamepad (the picture and
//! the sound are not decoded yet). The Amazon domain of the account is
//! `domain=` in this catalog's `service.txt` (amazon.com by default).

mod account;
mod api;
mod audio;
mod launch;
mod pad;
mod player;
mod qr;
mod store;
mod stream;
mod video;

use std::time::Duration;

use nxsvc::{Request, State};

/// `domain=` of the `service.txt` beside the program.
fn domain() -> String {
    let exe = std::env::args().next().unwrap_or_default();
    let dir = exe.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    std::fs::read_to_string(format!("{}/service.txt", dir))
        .ok()
        .and_then(|t| {
            t.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == "domain")
                .map(|(_, v)| v.trim().to_string())
        })
        .filter(|d| {
            !d.is_empty()
                && d.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
        .unwrap_or_else(|| "amazon.com".into())
}

/// A stop waits this long for the cloud session to end (display's limit is 8 s).
const STOP_WAIT: Duration = Duration::from_secs(6);

fn main() {
    let (cat, rx) = match nxsvc::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("luna: display refused the catalog ({})", e);
            std::process::exit(1);
        }
    };
    cat.set_state(State::Loading("Starting".into()));
    let user = std::env::var("NXK_USER").unwrap_or_default();
    let (c2, d) = (cat.clone(), domain());
    std::thread::spawn(move || account::run(c2, d, user));
    let mut playing: Option<launch::Launch> = None;
    while let Ok(r) = rx.recv() {
        match r {
            Request::Launch(id) => {
                if playing.as_ref().is_some_and(|l| l.running()) {
                    cat.set_state(State::Ready("A game is already starting".into()));
                } else {
                    playing = Some(launch::Launch::start(cat.clone(), id));
                }
            }
            // Asleep the network goes: the stream and its session end too.
            Request::Stop(d) | Request::Suspend(d) => {
                if let Some(l) = playing.take() {
                    l.stop(STOP_WAIT);
                }
                d.done();
            }
            Request::Resume(d) => d.done(),
        }
    }
}

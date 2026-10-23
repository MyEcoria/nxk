//! Amazon Luna as a game catalog of nxk (`abi::proto::provider`): the
//! sign-in by code, the games the account can play and their covers.
//! Playing (the stream) comes later. The Amazon domain of the account is
//! `domain=` in this catalog's `service.txt` (amazon.com by default).

mod account;
mod api;
mod qr;
mod store;

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
    while let Ok(r) = rx.recv() {
        match r {
            Request::Launch(id) => {
                let title = cat.game(&id).map(|g| g.title).unwrap_or(id);
                cat.set_state(State::Ready(format!(
                    "{}: playing comes in a later version",
                    title
                )));
            }
            Request::Stop(d) | Request::Suspend(d) | Request::Resume(d) => d.done(),
        }
    }
}

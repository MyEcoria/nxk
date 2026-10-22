//! A game catalog with no network: three games whose launch waits in a
//! pretend queue, then fills the screen with the game's color (B or HOME
//! ends it), "Offline", which never starts, and "Crash", which exits while
//! launching. Covers come 1.5 s after the list, as downloads would. It
//! exercises `display`'s side of the provider protocol in the emulator.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::sleep;
use std::time::Duration;

use nxk_abi::proto::input;
use nxsvc::screen::Screen;
use nxsvc::{Catalog, Cover, Game, Request, State};

const GAMES: [(&str, &str, u32, &str); 5] = [
    (
        "aurora",
        "Aurora",
        0x2E7DFF,
        "Northern lights over a still lake",
    ),
    (
        "basalt",
        "Basalt",
        0xE0592A,
        "Columns of cooled lava by the sea",
    ),
    (
        "cobalt",
        "Cobalt",
        0x16A085,
        "A blue mine deep under the hills",
    ),
    (
        "offline",
        "Offline",
        0x5D6D7E,
        "Never starts: no machine is free",
    ),
    (
        "crash",
        "Crash",
        0x808080,
        "Its catalog exits while it launches",
    ),
];

fn main() {
    let (cat, rx) = match nxsvc::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("demo: display refused the catalog ({})", e);
            std::process::exit(1);
        }
    };
    cat.set_state(State::Loading("Reading the demo catalog".into()));
    sleep(Duration::from_millis(600));
    let games: Vec<Game> = GAMES
        .iter()
        .map(|&(id, title, color, description)| Game {
            id: id.into(),
            title: title.into(),
            publisher: "nxk demo".into(),
            version: "1.0".into(),
            description: description.into(),
            color,
        })
        .collect();
    cat.set_games(games);
    cat.set_state(State::Ready(String::new()));
    let c2 = cat.clone();
    std::thread::spawn(move || {
        sleep(Duration::from_millis(1500));
        for &(id, _, color, _) in &GAMES[..4] {
            c2.set_cover(id, &cover(color));
        }
    });
    println!("demo: {} games listed", GAMES.len());
    while let Ok(r) = rx.recv() {
        match r {
            Request::Launch(id) => play(&cat, &rx, &id),
            Request::Stop(d) | Request::Suspend(d) | Request::Resume(d) => d.done(),
        }
    }
}

/// Diagonal bands of `rgb`, lighter toward the top left.
fn cover(rgb: u32) -> Cover {
    Cover::from_fn(256, |x, y| {
        let band = ((x + y) / 32) % 2 == 0;
        let k = 256 - (x + y) / 4;
        let ch = |s: u32| {
            let c = ((rgb >> s) & 0xFF) as usize * k / 256;
            (if band { c } else { c * 3 / 4 }) as u32
        };
        ch(16) << 16 | ch(8) << 8 | ch(0)
    })
}

fn play(cat: &Catalog, rx: &Receiver<Request>, id: &str) {
    let Some(g) = cat.game(id) else {
        return;
    };
    if id == "offline" {
        cat.set_state(State::Ready(format!("{}: no machine is free", g.title)));
        return;
    }
    for place in (1..=3).rev() {
        let text = "Waiting for a free machine".to_string();
        cat.set_state(State::Launching { text, place });
        if id == "crash" {
            eprintln!("demo: crashing on purpose");
            std::process::exit(3);
        }
        for _ in 0..5 {
            sleep(Duration::from_millis(100));
            if let Some(d) = stop_asked(rx) {
                cat.set_state(State::Ready(format!("{}: launch cancelled", g.title)));
                d.done();
                return;
            }
        }
    }
    let mut scr = match Screen::take() {
        Ok(s) => s,
        Err(e) => {
            cat.set_state(State::Ready(format!("{}: no screen ({})", g.title, e)));
            return;
        }
    };
    if let Err(e) = scr.show_color(g.color) {
        cat.set_state(State::Ready(format!("{}: no picture ({})", g.title, e)));
        return;
    }
    cat.set_state(State::Playing);
    println!("demo: {} on screen", g.title);
    let stop = loop {
        if let Some(d) = stop_asked(rx) {
            break Some(d);
        }
        if let Some((input::KIND_BUTTON, input::BTN_B, 1)) = scr.event(100_000_000) {
            break None;
        }
    };
    scr.release();
    cat.set_state(State::Ready(String::new()));
    println!("demo: {} ended", g.title);
    if let Some(d) = stop {
        d.done();
    }
}

/// A `Stop` waiting, if any; other requests answered on the way.
fn stop_asked(rx: &Receiver<Request>) -> Option<nxsvc::Done> {
    loop {
        match rx.try_recv() {
            Ok(Request::Stop(d)) => return Some(d),
            Ok(Request::Suspend(d) | Request::Resume(d)) => d.done(),
            Ok(Request::Launch(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
        }
    }
}

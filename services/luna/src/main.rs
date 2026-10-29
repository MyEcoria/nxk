#![feature(nxk_ext)]
//! Amazon Luna as a game catalog of nxk (`abi::proto::provider`): the
//! sign-in by code, the games the account can play and their covers, and
//! playing them: the cloud session, the stream, the gamepad, the sound and
//! the picture (decoded by `nxh264`, shown by the GPU). The Amazon domain of the account is
//! `domain=` in this catalog's `service.txt` (amazon.com by default); `refresh=` (24 to 60)
//! and `max_mbit=` set the picture rate and bit rate cap the stream announces.

mod account;
mod api;
mod audio;
mod clocks;
mod launch;
mod pad;
mod player;
mod present;
mod qr;
mod store;
mod stream;
mod video;

use std::time::Duration;

use nxsvc::{Request, State};

/// `key=` of the `service.txt` beside the program.
fn setting(key: &str) -> Option<String> {
    let exe = std::env::args().next().unwrap_or_default();
    let dir = exe.rsplit_once('/').map_or("", |(d, _)| d).to_string();
    std::fs::read_to_string(format!("{}/service.txt", dir))
        .ok()
        .and_then(|t| {
            t.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
        })
}

fn domain() -> String {
    setting("domain")
        .filter(|d| {
            !d.is_empty()
                && d.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
        .unwrap_or_else(|| "amazon.com".into())
}

/// `WIDTHxHEIGHT`.
fn size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// What the stream announces: `refresh=` (24 to 60 Hz) and `max_mbit=` (2 to 30) of `service.txt`, else 45 Hz
/// and the cap that suits it. Experiments, all off unless set: `notify_resolution=1280x720`,
/// `max_display=1280x720`, `min_quality=N`, `max_quality=N`, `bitrate_kbps=N`, `frame_stats=1`.
fn display_config() -> lunacore::stream::display::DisplayConfig {
    let number = |k: &str| setting(k).and_then(|v| v.parse::<u32>().ok());
    let refresh = number("refresh")
        .filter(|r| (24..=60).contains(r))
        .unwrap_or(45);
    let max = number("max_mbit").filter(|m| (2..=30).contains(m));
    let mut config = lunacore::stream::display::DisplayConfig::new(refresh, max);
    config.tuning = lunacore::stream::display::Tuning {
        notify_resolution: setting("notify_resolution").and_then(|v| size(&v)),
        max_display: setting("max_display").and_then(|v| size(&v)),
        min_quality: number("min_quality"),
        max_quality: number("max_quality"),
        bitrate_kbps: number("bitrate_kbps"),
        frame_stats: number("frame_stats") == Some(1),
    };
    config
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
    let (c2, d, u2) = (cat.clone(), domain(), user.clone());
    std::thread::spawn(move || account::run(c2, d, u2));
    let mut playing: Option<launch::Launch> = None;
    while let Ok(r) = rx.recv() {
        match r {
            Request::Launch(id) => {
                if playing.as_ref().is_some_and(|l| l.running()) {
                    cat.set_state(State::Ready("A game is already starting".into()));
                } else {
                    playing = Some(launch::Launch::start(cat.clone(), id, user.clone()));
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

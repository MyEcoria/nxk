//! Playing a game: any session left open is ended, the game's launch
//! parameters are read, the regions measured, a session job created and
//! followed through Luna's queue, then the stream runs (on a thread of its
//! own) while this one holds the screen and passes the gamepad on. A stop
//! (HOME, sleep) ends the stream and the cloud session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use lunacore::auth::Session;
use lunacore::pages;
use lunacore::session::{
    self as gs, CreateJobResponse, GameSession, GetJobResponse, JobStatus, LaunchParams,
};
use lunacore::stream::input::{
    GamePadConnection, GamePadEvent, GamePadKind, GamePadState, InputEvent,
};
use lunacore::stream::Secret;
use nxk_abi::proto::provider::Stats;
use nxsvc::{Catalog, State};
use serde_json::{json, Value};

use crate::api::{Api, Result};
use crate::{clocks::CpuBoost, pad, present::Presenter, store, stream, video};

const CALL_TIMEOUT: Duration = Duration::from_secs(8);
const END_TIMEOUT: Duration = Duration::from_secs(3);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SIDE: (u32, u32) = (1280, 720);
/// The presenter looks at the stop flag and the figures at least this often.
const PICTURE_WAIT: Duration = Duration::from_millis(20);
/// The gamepad thread looks at its stop flag at least this often.
const PAD_WAIT_NS: u64 = 20_000_000;
/// A dark screen until the first picture is decoded.
const WAIT_COLOR: u32 = 0x101820;

/// A launch running; `stop` ends it.
pub struct Launch {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Launch {
    pub fn start(cat: Catalog, id: String) -> Self {
        let (stop, done) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (s2, d2) = (stop.clone(), done.clone());
        std::thread::spawn(move || {
            let title = cat.game(&id).map(|g| g.title).unwrap_or_else(|| id.clone());
            let why = match play(&cat, &id, &s2) {
                Ok(()) => String::new(),
                Err(e) => {
                    println!("luna: {}: {}", title, e);
                    format!("{}: {}", title, e)
                }
            };
            cat.set_state(State::Ready(why));
            d2.store(true, Ordering::Relaxed);
        });
        Self { stop, done }
    }

    pub fn running(&self) -> bool {
        !self.done.load(Ordering::Relaxed)
    }

    /// Ends it, waiting at most `wait` for the cloud session to be ended.
    pub fn stop(&self, wait: Duration) {
        self.stop.store(true, Ordering::Relaxed);
        let until = Instant::now() + wait;
        while self.running() && Instant::now() < until {
            sleep(Duration::from_millis(20));
        }
    }
}

fn status_text(s: JobStatus) -> &'static str {
    match s {
        JobStatus::ActiveRequested => "Asking for a machine",
        JobStatus::ActiveQueued => "Waiting in line",
        JobStatus::ActiveCreatingSession => "Starting the game",
        JobStatus::ActiveConnecting => "Connecting",
        JobStatus::ActiveTidyingUp => "Tidying up",
        _ => "Starting",
    }
}

fn stopped(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        Err("launch cancelled".into())
    } else {
        Ok(())
    }
}

fn play(cat: &Catalog, id: &str, stop: &AtomicBool) -> Result<()> {
    let st = store::load();
    let s = st.session.ok_or("sign in first")?;
    let mut api = Api::quick(&st.device_serial, CALL_TIMEOUT);
    end_left_open(&mut api, &s);
    stopped(stop)?;
    let page = api.call(
        Some(&s),
        "getPage",
        &pages::request("game_detail", Some(id)),
    )?;
    let launch: LaunchParams = pages::game_session_payload(&page)
        .and_then(|p| serde_json::from_value(p).ok())
        .ok_or("this game cannot be played with this account")?;
    stopped(stop)?;
    let (latencies, preferences) = gs::to_json(&regions());
    let request = gs::create_job_request(&launch, latencies, preferences, SIDE);
    let created: CreateJobResponse =
        serde_json::from_value(api.call(Some(&s), "createGameSessionJob", &request)?)
            .map_err(|_| "createGameSessionJob: unexpected reply")?;
    let job = match (created.error_code, created.game_session_job_id) {
        (0, Some(j)) => j,
        (code, _) => return Err(format!("createGameSessionJob refused (error {})", code)),
    };
    let session = match follow(
        cat,
        &mut api,
        &s,
        &job,
        created.start_polling_after_ms,
        stop,
    ) {
        Ok(g) => g,
        Err(e) => {
            let _ = api.call(Some(&s), "cancelGameSessionJob", &json!({ "jobId": job }));
            end_left_open(&mut Api::quick(&api.serial, END_TIMEOUT), &s);
            return Err(e);
        }
    };
    let result = stream_it(cat, &session, stop);
    // The cloud session is ended whatever happened.
    if let Some(sid) = &session.session_id {
        let end = Api::quick(&api.serial, END_TIMEOUT).call(
            Some(&s),
            "terminateGameSession",
            &json!({"sessionIds": [sid], "terminationReason": "OVERLAY_EXIT"}),
        );
        let mut check = Api::quick(&api.serial, END_TIMEOUT);
        match (end, open_sessions(&mut check, &s)) {
            (Ok(_), Some(0)) => println!("luna: session ended"),
            (Ok(_), n) => println!("luna: session ended, {:?} still listed", n),
            (Err(_), _) => {
                println!("luna: the session could not be ended now: next launch ends it")
            }
        }
    }
    result
}

/// Luna's queue, shown on the launch card, until the session is ready.
fn follow(
    cat: &Catalog,
    api: &mut Api,
    s: &Session,
    job: &str,
    first_ms: u64,
    stop: &AtomicBool,
) -> Result<GameSession> {
    let started = Instant::now();
    sleep(Duration::from_millis(first_ms.min(5_000)));
    loop {
        stopped(stop)?;
        let r: GetJobResponse = serde_json::from_value(api.call(
            Some(s),
            "getGameSessionJob",
            &json!({ "gameSessionJobId": job }),
        )?)
        .map_err(|_| "getGameSessionJob: unexpected reply")?;
        let status = r.status.unwrap_or(JobStatus::Unknown);
        let place = r.position_in_line.max(0) as u32;
        cat.set_state(State::Launching {
            text: status_text(status).into(),
            place,
        });
        match status {
            JobStatus::CompletedSucceeded => {
                return r
                    .game_session
                    .ok_or_else(|| "no session in the reply".into())
            }
            JobStatus::CompletedFailed => {
                return Err(format!("Luna could not start it (error {})", r.error_code))
            }
            _ if started.elapsed() > QUEUE_TIMEOUT => return Err("waited too long".into()),
            _ => {
                let wait = r.retry_after_ms.clamp(500, 5_000);
                let until = Instant::now() + Duration::from_millis(wait);
                while Instant::now() < until {
                    stopped(stop)?;
                    sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// The screen held, the gamepad passed on, while the stream runs.
fn stream_it(cat: &Catalog, g: &GameSession, stop: &AtomicBool) -> Result<()> {
    let info = g
        .certificate_information
        .as_ref()
        .ok_or("no certificate to pin")?;
    let pin = nxdtls::Pin::from_hex(
        info["commonName"].as_str().unwrap_or(""),
        info["sha256"].as_str().unwrap_or(""),
    )
    .ok_or("bad certificate pin")?;
    let target = stream::Target {
        ip: g.public_ip.clone().ok_or("no server address")?,
        port: g.port.ok_or("no server port")?,
        pin,
        session_id: g.session_id.clone().ok_or("no session id")?,
        token: Secret::new(g.auth_token.clone().unwrap_or_default()),
        display: crate::display_config(),
    };
    let mut screen = Presenter::open().map_err(|e| format!("no screen ({})", e))?;
    let _ = screen.show_color(WAIT_COLOR);
    let _boost = CpuBoost::raise();
    let shared = Arc::new(video::Shared::default());
    let video_out = shared.clone();
    cat.set_state(State::Playing);
    let (tx, rx) = channel();
    let ended = Arc::new(AtomicBool::new(false));
    let (e2, stop_flag) = (ended.clone(), Arc::new(AtomicBool::new(false)));
    let s2 = stop_flag.clone();
    let runner = std::thread::spawn(move || {
        let r = stream::run(&target, &rx, &s2, video_out);
        e2.store(true, Ordering::Relaxed);
        r
    });
    // The gamepad is read on a thread of its own: `show` waits for the vertical blank.
    let (events, pad_done) = (screen.events(), Arc::new(AtomicBool::new(false)));
    let (pad_flag, pad_tx) = (pad_done.clone(), tx);
    let gamepad = std::thread::spawn(move || read_pad(&events, &pad_tx, &pad_flag));
    let mut next_stats = Instant::now();
    while !ended.load(Ordering::Relaxed) {
        if Instant::now() >= next_stats {
            cat.set_stats(shared.stats());
            next_stats += Duration::from_secs(1);
        }
        if stop.load(Ordering::Relaxed) {
            stop_flag.store(true, Ordering::Relaxed);
        }
        // The newest picture, at the next vertical blank (this call sets the pace).
        if let Some((arrived, pic)) = shared.wait_latest(PICTURE_WAIT) {
            let t = Instant::now();
            if let Err(e) = screen.show(&pic) {
                println!("luna: showing a picture: {}", e);
            }
            shared.on_screen(arrived, t.elapsed());
        }
    }
    pad_done.store(true, Ordering::Relaxed);
    let _ = gamepad.join();
    cat.set_stats(Stats::default());
    let r = runner.join().map_err(|_| "the stream thread failed")?;
    drop(screen);
    match r? {
        stream::End::Stopped | stream::End::Closed => Ok(()),
    }
}

/// The console's controls as Luna's gamepad: every change goes to the
/// stream thread as it is read, until `done`.
fn read_pad(events: &nxgfx::Events, tx: &Sender<stream::Timed>, done: &AtomicBool) {
    let timed = |ev| (Instant::now(), ev);
    let _ = tx.send(timed(InputEvent::GamePadConnection(GamePadConnection {
        device_id: 0,
        connected: true,
        kind: GamePadKind::Xbox360,
    })));
    let (mut state, mut packet) = (GamePadState::default(), 0u32);
    while !done.load(Ordering::Relaxed) {
        let Some(ev) = events.poll_words(PAD_WAIT_NS) else {
            continue;
        };
        if pad::apply(&mut state, &ev) {
            packet += 1;
            let _ = tx.send(timed(InputEvent::GamePad(GamePadEvent {
                device_id: 0,
                packet_number: packet,
                kind: GamePadKind::Xbox360,
                state,
            })));
        }
    }
}

/// Every region's ping, measured at once.
fn regions() -> Vec<(String, f64)> {
    let handles: Vec<_> = gs::REGIONS
        .iter()
        .map(|r| {
            let r = r.to_string();
            std::thread::spawn(move || {
                Api::ping(&gs::ping_url(&r), Duration::from_secs(2))
                    .map(|d| (r, d.as_secs_f64() * 1000.0))
            })
        })
        .collect();
    let mut out: Vec<(String, f64)> = handles
        .into_iter()
        .filter_map(|h| h.join().ok().flatten())
        .collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

/// How many sessions Luna lists as open for the account.
fn open_sessions(api: &mut Api, s: &Session) -> Option<usize> {
    let reply = api
        .call(Some(s), "getPlayingGameSession", &json!({}))
        .ok()?;
    let mut ids = Vec::new();
    collect_ids(&reply, &mut ids);
    Some(ids.len())
}

/// A session a crash or a kill left open is ended (one stream per account).
fn end_left_open(api: &mut Api, s: &Session) {
    let Ok(reply) = api.call(Some(s), "getPlayingGameSession", &json!({})) else {
        return;
    };
    let mut ids = Vec::new();
    collect_ids(&reply, &mut ids);
    if !ids.is_empty() {
        println!("luna: ending {} session(s) left open", ids.len());
        let _ = api.call(
            Some(s),
            "terminateGameSession",
            &json!({"sessionIds": ids, "terminationReason": "OVERLAY_EXIT"}),
        );
    }
}

fn collect_ids(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(id) = m.get("sessionId").and_then(Value::as_str) {
                if !out.iter().any(|o| o == id) {
                    out.push(id.to_string());
                }
            }
            m.values().for_each(|c| collect_ids(c, out));
        }
        Value::Array(a) => a.iter().for_each(|c| collect_ids(c, out)),
        _ => {}
    }
}

//! display: Tegra X1 DC-A screen; window A on a shared image, drawn as
//! the Switch-style touch UI of `ui`: home row of games and app tiles,
//! quick settings drawer, settings, system, network, USB-C terminal and
//! logs. Redrawn every second and on input. Counts the time played in
//! the games it starts.
//! Panel init is left to the bootloader; only the window is changed. A
//! portrait panel (Switch) is drawn turned.

#![no_std]
#![no_main]

extern crate alloc;

mod art;
mod backlight;
mod config;
mod controllers;
mod font;
mod games;
mod home;
mod pages;
mod playtime;
mod quick;
mod screen;
mod settings;
mod sheets;
mod stats;
mod status;
mod term;
mod theme;
mod ui;
mod widgets;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, EIO, ENODEV};
use libnx::abi::proto::gpu as gpuproto;
use libnx::abi::proto::{
    audio, bt, console as con, cpufreq, display as proto, fan, gpu, input, wifi,
};
use libnx::abi::rights::{DMA, DUP, GRANT, MAP, WRITE};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::abi::slots;
use libnx::drv::{self, Mmio};
use libnx::ipc::{self, Msg};
use libnx::mem::SharedMemory;
use libnx::ns::Service;
use libnx::sync::{Condvar, Mutex};
use libnx::{println, sys, thread, time};

use screen::Screen;
use ui::{Action, ScanNet, Ui};

const DC: u64 = 0x5420_0000;
/// Statistics refresh period.
const REFRESH_NS: u64 = 1_000_000_000;
/// Console read period while the TERMINAL or LOGS page is shown, and
/// else: a quiet core stays quiet (ktest's tickless test), the ring
/// holds 16 KiB.
const TAIL_NS: u64 = 200_000_000;
const TAIL_IDLE_NS: u64 = 5_000_000_000;
/// Cursor blink half-period.
const BLINK_NS: u64 = 500_000_000;
/// Longest wait on the console.
const CALL_NS: u64 = 20_000_000;
/// Log lines kept for the LOGS page: the console's logs (services, the
/// kernel) and text written to the screen.
const MSG_LINES: usize = 256;

/// Notification bits: touches queued, an action finished.
const EV_TOUCH: u64 = 1;
const EV_DONE: u64 = 2;
/// A game the library started has ended.
const EV_EXIT: u64 = 4;
/// The game list is read again this often (and every 3 s while empty).
const SCAN_NS: u64 = 8_000_000_000;
/// A game that took the screen by itself: its process checked this often.
const LIVE_NS: u64 = 1_000_000_000;

// 32-bit word register indices.
const CMD_DISPLAY_COMMAND: usize = 0x32;
const CMD_STATE_CONTROL: usize = 0x41;
const CMD_DISPLAY_WINDOW_HEADER: usize = 0x42;
const DISP_DISP_ACTIVE: usize = 0x409;
const DISP_BLEND_BACKGROUND_COLOR: usize = 0x4E4;
const WIN_OPTIONS: usize = 0x700;
const WIN_COLOR_DEPTH: usize = 0x703;
const WIN_POSITION: usize = 0x704;
const WIN_SIZE: usize = 0x705;
const WIN_PRESCALED_SIZE: usize = 0x706;
const WIN_H_INITIAL_DDA: usize = 0x707;
const WIN_V_INITIAL_DDA: usize = 0x708;
const WIN_DDA_INCREMENT: usize = 0x709;
const WIN_LINE_STRIDE: usize = 0x70A;
const WINBUF_START_ADDR: usize = 0x800;
const WINBUF_ADDR_H_OFFSET: usize = 0x806;
const WINBUF_ADDR_V_OFFSET: usize = 0x808;
const WINBUF_SURFACE_KIND: usize = 0x80B;
/// `WINBUF_SURFACE_KIND`: block-linear, block height in bits 6-4.
const KIND_BLOCK: u32 = 2;
/// Block height of the scanout images (log2 of GOBs): 16 GOBs.
const SCANOUT_BLOCK_LOG2: u32 = 4;
/// Events kept for a game that does not read them.
const GAME_EVENTS: usize = 256;

const R8G8B8A8: u32 = 13;

static INPUT: Service = Service::new("input", 0);
static WIFI: Service = Service::optional("wifi");
static POWER: Service = Service::optional("power");
static FAN: Service = Service::optional("fan");
static CPUFREQ: Service = Service::optional("cpufreq");
static GPU: Service = Service::optional("gpu");
static AUDIO: Service = Service::optional("audio");
static BT: Service = Service::optional("bt");

fn wr(dc: &Mmio, reg: usize, v: u32) {
    dc.w32(reg * 4, v);
}

/// Log lines, the latest last.
pub struct Messages {
    lines: VecDeque<String>,
    /// Unfinished last line, bytes pending (UTF-8).
    partial: Vec<u8>,
}

impl Messages {
    /// Returns the number of lines completed.
    fn write(&mut self, bytes: &[u8]) -> usize {
        let mut done = 0;
        for &b in bytes {
            match b {
                b'\n' => {
                    let l = String::from_utf8_lossy(&self.partial).into_owned();
                    self.partial.clear();
                    if self.lines.len() == MSG_LINES {
                        self.lines.pop_front();
                    }
                    self.lines.push_back(l);
                    done += 1;
                }
                b'\r' => {}
                b if self.partial.len() < 1024 => self.partial.push(b),
                _ => {}
            }
        }
        done
    }

    pub fn lines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lines.iter().cloned().collect();
        if !self.partial.is_empty() {
            v.push(String::from_utf8_lossy(&self.partial).into_owned());
        }
        v
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.partial.is_empty()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
    }
}

/// An event as `input` reports it: kind, then for a touch the code (down,
/// move, up), finger, raw x, y; for a button its code and 1 or 0.
/// kind, code, value, x, y, source (`input::READ`).
type Event = (u64, u64, u64, u64, u64, u64);

enum Job {
    Act(Action),
    Save(config::Config),
    /// `playtime.cfg`'s new text.
    SavePlay(String),
    SleepCpu(u64),
}

/// Outcome of an action run by the worker.
enum Done {
    Scan(Result<Vec<ScanNet>, i64>),
    Retry(Result<String, i64>),
    Leave(Result<(), i64>),
    Flight(Result<bool, i64>),
    Power(i64),
    /// The fan's state after a `SET`.
    Fan(Result<stats::Fan, i64>),
    /// The CPU clock after a `SET`.
    Cpu(Result<stats::Cpu, i64>),
    /// A GPU `CLOCK` change (the stats page shows the result).
    Gpu(Result<(), i64>),
    /// An audio volume/melody/tone call (`Some(toast)` when user-triggered playback).
    Audio(Result<Option<&'static str>, i64>),
    /// A Bluetooth call, with the toast it gives when it worked.
    Bt(Result<&'static str, i64>),
}

/// A game that took the screen: the images it draws on (block-linear,
/// scanned out in turn), the input events routed to it and the caller
/// waiting for the next one (reply token, deadline).
struct Game {
    images: Vec<SharedMemory>,
    /// The one on display.
    shown: usize,
    /// The process that took the screen.
    owner: u32,
    /// A copy of it for `SNAPSHOT`: the GPU is drawing on the others, and
    /// a capture takes longer than a frame.
    snap: Option<SharedMemory>,
    pas: Vec<u32>,
    events: VecDeque<Event>,
    waiter: Option<(u64, u64)>,
}

/// The game the library started: its play time counts from `since`.
struct Running {
    handle: u32,
    title: String,
    name: String,
    since: u64,
}

/// Starts games and learns when they end (a thread waits on each).
struct Launcher {
    running: Option<Running>,
    exits: Arc<Mutex<VecDeque<(u32, i64)>>>,
    notif: u32,
}

impl Launcher {
    /// Start game `i`; true if it runs (it is now the last played).
    fn launch(&mut self, ui: &mut Ui, i: usize) -> bool {
        let now = time::now_ns();
        let Some(g) = ui.game(i) else {
            return false;
        };
        let (exe, title, name) = (g.exe.clone(), g.title.clone(), g.name.clone());
        if let Some(r) = &self.running {
            ui.toast(format!("{} is running", r.title), now);
            return false;
        }
        match libnx::process::spawn_args(&[&exe]) {
            Ok(h) => {
                let (q, notif) = (self.exits.clone(), self.notif);
                let spawned = thread::spawn(move || {
                    let code = libnx::process::wait(h);
                    q.lock().push_back((h, code));
                    let _ = sys::notify_signal(notif, EV_EXIT);
                });
                if spawned.is_err() {
                    ui.toast("cannot watch the game".into(), now);
                }
                ui.play.started(&name);
                let games = ui.take_games();
                ui.set_games(games);
                self.running = Some(Running {
                    handle: h,
                    title,
                    name,
                    since: now,
                });
                true
            }
            Err(e) => {
                ui.toast(format!("{}: {}", title, libnx::abi::err::name(e)), now);
                false
            }
        }
    }

    /// End the game: its exit comes back as an `EV_EXIT`.
    fn stop(&mut self) -> bool {
        match &self.running {
            Some(r) => {
                let _ = sys::process_kill(r.handle);
                true
            }
            None => false,
        }
    }
}

struct Worker {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    done: Mutex<VecDeque<Done>>,
}

/// Networks of a `wifi::SCAN` reply.
fn scan() -> Result<Vec<ScanNet>, i64> {
    let mut m = Msg::new(wifi::SCAN, 1, 0);
    m.mr[0] = 0;
    let r = WIFI.call(&m)?;
    let count = ipc::header(&r, 0) as usize;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    let n = ipc::unpack_at(&r, 1, &mut buf);
    let mut nets = Vec::new();
    let mut at = 0;
    for _ in 0..count {
        let Some(rec) = buf[..n].get(at..at + wifi::RECORD) else {
            break;
        };
        let len = rec[0] as usize;
        let Some(ssid) = buf[..n].get(at + wifi::RECORD..at + wifi::RECORD + len) else {
            break;
        };
        nets.push(ScanNet {
            ssid: String::from_utf8_lossy(ssid).into_owned(),
            channel: rec[1],
            signal: rec[3],
            security: rec[2],
        });
        at += wifi::RECORD + len;
    }
    nets.sort_by_key(|n| n.signal);
    Ok(nets)
}

/// Runs the actions that may take seconds (Wi-Fi scan, chip recovery, a
/// fan busy measuring its speed, a GPU running a batch) one at a time,
/// off the drawing loop.
fn work(w: &Worker, notif: u32) -> ! {
    loop {
        let mut q = w.jobs.lock();
        let job = loop {
            match q.pop_front() {
                Some(j) => break j,
                None => q = w.wake.wait(q),
            }
        };
        drop(q);
        let act = match job {
            Job::Act(a) => a,
            Job::Save(cfg) => {
                let _ = cfg.save();
                continue;
            }
            Job::SavePlay(text) => {
                let _ = playtime::save(&text);
                continue;
            }
            Job::SleepCpu(mhz) => {
                let _ = CPUFREQ.call(&Msg::from_words(cpufreq::SET, &[mhz]));
                continue;
            }
        };
        let done = match act {
            Action::Scan => Done::Scan(scan()),
            Action::Retry => Done::Retry(WIFI.call(&Msg::new(wifi::RETRY, 0, 0)).map(|r| {
                let mut buf = [0u8; 256];
                let n = ipc::unpack(&r, &mut buf);
                String::from_utf8_lossy(&buf[..n]).into_owned()
            })),
            Action::Leave => Done::Leave(WIFI.call(&Msg::new(wifi::DISCONNECT, 0, 0)).map(|_| ())),
            Action::Flight(off) => Done::Flight(
                WIFI.call(&Msg::from_words(wifi::RADIO, &[(!off) as u64]))
                    .map(|_| off),
            ),
            Action::Power(label) => {
                Done::Power(POWER.call(&Msg::new(label, 0, 0)).err().unwrap_or(0))
            }
            Action::Fan(v) => Done::Fan(
                FAN.call(&Msg::from_words(fan::SET, &[v]))
                    .map(|m| stats::Fan::from_reply(&m)),
            ),
            Action::Cpu(v) => Done::Cpu(
                CPUFREQ
                    .call(&Msg::from_words(cpufreq::SET, &[v]))
                    .map(|m| stats::Cpu::from_reply(&m)),
            ),
            Action::Gpu(khz, force) => {
                let f = if force { gpu::CLOCK_FORCE } else { 0 };
                Done::Gpu(
                    GPU.call(&Msg::from_words(gpu::CLOCK, &[khz, f]))
                        .map(|_| ()),
                )
            }
            // Silent on failure: also replayed at boot, and `audio` logs its own errors.
            Action::Volume(pct) => Done::Audio(Ok(AUDIO
                .call(&Msg::from_words(audio::VOLUME, &[pct as u64]))
                .ok()
                .and(None))),
            Action::Music => Done::Audio(
                AUDIO
                    .call(&Msg::new(audio::MUSIC, 0, 0))
                    .map(|_| Some("playing test melody")),
            ),
            Action::Tone => Done::Audio(
                AUDIO
                    .call(&Msg::from_words(audio::TONE, &[440, 2000, 50]))
                    .map(|_| Some("playing 440 Hz tone (2 s)")),
            ),
            Action::BtPair(on) => Done::Bt(
                BT.call(&Msg::from_words(
                    bt::PAIR,
                    &[if on { bt::PAIR_DEFAULT_NS } else { 0 }, 0],
                ))
                .map(|_| {
                    if on {
                        "Bluetooth: hold the controller's sync button"
                    } else {
                        "Bluetooth: pairing stopped"
                    }
                }),
            ),
            Action::BtForget(addr) => Done::Bt(
                BT.call(&Msg::from_words(bt::FORGET, &[addr]))
                    .map(|_| "Bluetooth: controller forgotten"),
            ),
            Action::ClearLogs
            | Action::Launch(_)
            | Action::Brightness(_)
            | Action::Save
            | Action::Sleep => continue,
        };
        w.done.lock().push_back(done);
        let _ = sys::notify_signal(notif, EV_DONE);
    }
}

/// New console output into the TERMINAL page; true if there was some.
fn read_console(term: &mut term::Term, pos: &mut u64, buf: &mut [u8]) -> bool {
    let mut any = false;
    for _ in 0..8 {
        let mut m = Msg::new(con::TAIL, 2, 0);
        m.mr[0] = *pos;
        m.mr[1] = 0;
        let Ok(r) = ipc::call_timeout(slots::TERMINAL, &m, CALL_NS) else {
            break;
        };
        if r.label() != 0 {
            break;
        }
        let n = ipc::unpack_at(&r, 1, buf);
        *pos = ipc::header(&r, 0);
        if n == 0 {
            break;
        }
        term.feed(&buf[..n]);
        any = true;
    }
    any
}

/// Raw touch coordinates to screen pixels.
fn touch_xy(x: u64, y: u64, sw: usize, sh: usize) -> (usize, usize) {
    let e = input::TOUCH_EDGE;
    let x = (x.clamp(e, input::TOUCH_X_MAX) - e) * sw as u64 / (input::TOUCH_X_MAX - e);
    let y = (y.clamp(e, input::TOUCH_Y_MAX) - e) * sh as u64 / (input::TOUCH_Y_MAX - e);
    (x as usize, y as usize)
}

/// The image on display, copied (cached memory, allocated on the first
/// capture).
fn still(g: &mut Game) -> Option<&SharedMemory> {
    let n = g.images[g.shown].len();
    if g.snap.is_none() {
        g.snap = Some(SharedMemory::new(n, 0).ok()?);
    }
    let snap = g.snap.as_ref()?;
    snap.as_mut_slice()[..n].copy_from_slice(&g.images[g.shown].as_slice()[..n]);
    Some(snap)
}

/// The images of a game, black, the first on display, block-linear:
/// handles queued in `sent` for the reply.
fn take_screen(dc: &Mmio, w: usize, h: usize, sent: &mut Vec<u32>) -> Result<Game, i64> {
    let bytes = gpuproto::tiled_bytes(w as u32, h as u32, SCANOUT_BLOCK_LOG2);
    let mut images = Vec::new();
    let mut pas = Vec::new();
    for _ in 0..proto::SCANOUT_BUFFERS {
        let m = SharedMemory::new(bytes, CONTIGUOUS | DMA32 | UNCACHED)?;
        pas.push(m.paddr(0)? as u32);
        images.push(m);
    }
    for (i, m) in images.iter().enumerate() {
        let h2 = sys::handle_dup(m.handle(), MAP | DMA | GRANT | DUP, 0)?;
        ipc::set_send_handle(i, h2);
        sent.push(h2);
    }
    // Block-linear window, pitch the width in bytes.
    wr(
        dc,
        WINBUF_SURFACE_KIND,
        KIND_BLOCK | SCANOUT_BLOCK_LOG2 << 4,
    );
    wr(dc, WIN_LINE_STRIDE, w as u32 * 4);
    flip(dc, pas[0]);
    let _ = h;
    Ok(Game {
        images,
        shown: 0,
        owner: 0,
        snap: None,
        pas,
        events: VecDeque::new(),
        waiter: None,
    })
}

/// Window A reads `pa` from the next vertical blank; returns once it does.
fn flip(dc: &Mmio, pa: u32) {
    wr(dc, WINBUF_START_ADDR, pa);
    wr(dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(dc, CMD_STATE_CONTROL, 1 | 2);
    // The request bits clear when the controller takes the update.
    for _ in 0..400 {
        if dc.r32(CMD_STATE_CONTROL * 4) & 3 == 0 {
            return;
        }
        time::sleep_us(100);
    }
}

/// The UI's image again, pitch-linear.
fn give_back(dc: &Mmio, w: usize, pa: u32) {
    wr(dc, WINBUF_SURFACE_KIND, 0);
    wr(dc, WIN_LINE_STRIDE, w as u32 * 4);
    flip(dc, pa);
}

libnx::entry!(main);

fn main() -> i32 {
    let dc = Mmio::map(drv::dev_slot(0), DC, 0x40000).expect("display: registers");
    let bl = backlight::Backlight::open();
    let active = dc.r32(DISP_DISP_ACTIVE * 4);
    let (mut w, mut h) = (
        (active & 0x1FFF) as usize,
        ((active >> 16) & 0x1FFF) as usize,
    );
    if w < 16 || h < 16 {
        (w, h) = (1280, 720); // display unconfigured: default mode
        wr(&dc, DISP_DISP_ACTIVE, (h as u32) << 16 | w as u32);
    }
    let fb = SharedMemory::new(w * h * 4, CONTIGUOUS | DMA32 | UNCACHED).expect("display: image");
    let pa = fb.paddr(0).expect("display: image address") as u32;
    let mut scr = Screen::new(fb, w, h);
    let (sw, sh) = scr.size();

    wr(&dc, CMD_DISPLAY_WINDOW_HEADER, 1 << 4); // window A
    wr(&dc, WIN_COLOR_DEPTH, R8G8B8A8);
    wr(&dc, WIN_POSITION, 0);
    wr(&dc, WIN_SIZE, (h as u32) << 16 | w as u32);
    wr(&dc, WIN_PRESCALED_SIZE, (h as u32) << 16 | (w as u32 * 4));
    wr(&dc, WIN_H_INITIAL_DDA, 0);
    wr(&dc, WIN_V_INITIAL_DDA, 0);
    wr(&dc, WIN_DDA_INCREMENT, 0x1000_1000);
    wr(&dc, WIN_LINE_STRIDE, w as u32 * 4);
    wr(&dc, WINBUF_START_ADDR, pa);
    wr(&dc, WINBUF_ADDR_H_OFFSET, 0);
    wr(&dc, WINBUF_ADDR_V_OFFSET, 0);
    wr(&dc, WIN_OPTIONS, 1 << 30);
    wr(&dc, DISP_BLEND_BACKGROUND_COLOR, 0);
    wr(&dc, CMD_DISPLAY_COMMAND, 1 << 5); // continuous display
    wr(&dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(&dc, CMD_STATE_CONTROL, 1 | 2);

    let mut msgs = Messages {
        lines: VecDeque::new(),
        partial: Vec::new(),
    };
    let turned = if scr.turned() {
        ", turned landscape"
    } else {
        ""
    };
    msgs.write(format!("display: {}x{} panel{}\n", w, h, turned).as_bytes());
    println!(
        "[display] screen {}x{} (window A, image at {:#x})",
        w, h, pa
    );

    // Touches and finished actions arrive as notifications on the endpoint.
    let notif = sys::notify_create().expect("display: notification");
    sys::notify_bind(notif).expect("display: notification bind");
    let touches: Arc<Mutex<VecDeque<Event>>> = Arc::new(Mutex::new(VecDeque::new()));
    {
        let q = touches.clone();
        thread::spawn(move || loop {
            let mut m = Msg::new(input::READ, 2, 0);
            m.mr[0] = 0;
            m.mr[1] = 1 << input::KIND_TOUCH | 1 << input::KIND_BUTTON | 1 << input::KIND_STICK;
            match INPUT.call(&m) {
                Ok(r) => {
                    q.lock()
                        .push_back((r.mr[0], r.mr[1], r.mr[2], r.mr[3], r.mr[4], r.mr[5]));
                    let _ = sys::notify_signal(notif, EV_TOUCH);
                }
                Err(_) => time::sleep_ms(1000),
            }
        })
        .expect("display: touch thread");
    }
    let worker = Arc::new(Worker {
        jobs: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        done: Mutex::new(VecDeque::new()),
    });
    {
        let w2 = worker.clone();
        thread::spawn(move || work(&w2, notif)).expect("display: worker thread");
    }

    let ep = drv::publish("display").expect("display: publish");
    let mut data = stats::Data::new();
    let mut ui = Ui::new();
    ui.has_backlight = bl.is_some();
    if let Some(b) = &bl {
        ui.cfg.brightness = b.percent();
    }
    let mut cfg_loaded = false;
    let mut play_loaded = false;
    let mut save_at: u64 = 0;
    let mut sleeping = false;
    let mut pre_sleep_cpu: Option<u64> = None;
    let mut last_input = time::now_ns();

    let mut term = term::Term::new();
    let (mut tail, mut tail_buf) = (0u64, alloc::vec![0u8; ipc::MAX_BYTES]);
    // Finger followed (the first one down) and where it last was.
    let mut finger: Option<(u64, usize, usize)> = None;
    let (mut next_refresh, mut next_tail, mut blink) = (0, 0, 0);
    let mut dirty = true;
    // A client drawing on the image itself (OPEN) stops the UI.
    let mut owned = false;
    // A game drawing with the GPU (SCANOUT) stops it too, and gets the input.
    let mut game: Option<Game> = None;
    // Handles sent with the previous reply: closed afterwards.
    let mut sent: Vec<u32> = Vec::new();
    let mut launcher = Launcher {
        running: None,
        exits: Arc::new(Mutex::new(VecDeque::new())),
        notif,
    };
    let (mut next_scan, mut next_live) = (0u64, 0u64);
    // The game goes: the UI comes back, the GPU forgets its buffers.
    macro_rules! release_game {
        () => {{
            game = None;
            // Nothing runs on the GPU any more: it goes off (the clock asked
            // for is kept for the next game).
            let _ = GPU.call(&Msg::new(gpuproto::RESET, 0, 0));
            let _ = GPU.call(&Msg::from_words(gpuproto::POWER, &[0]));
            give_back(&dc, w, pa);
            scr.forget();
            dirty = true;
            next_refresh = 0;
            last_input = time::now_ns();
        }};
    }
    // Latest volume wins in the worker's queue; the config is saved a bit later.
    macro_rules! set_volume {
        ($pct:expr, $now:expr) => {{
            save_at = $now + 800_000_000;
            let mut q = worker.jobs.lock();
            q.retain(|j| !matches!(j, Job::Act(Action::Volume(_))));
            q.push_back(Job::Act(Action::Volume($pct)));
            drop(q);
            worker.wake.notify_one();
        }};
    }
    macro_rules! enter_sleep {
        () => {{
            if !sleeping {
                sleeping = true;
                if let Some(b) = &bl {
                    b.off();
                }
                pre_sleep_cpu = match &data.cpu {
                    stats::Seen::Value(c) => Some(c.step),
                    _ => None,
                };
                worker
                    .jobs
                    .lock()
                    .push_back(Job::SleepCpu(cpufreq::STEPS_MHZ[0] as u64));
                worker.wake.notify_one();
            }
        }};
    }
    macro_rules! wake_sleep {
        ($now:expr) => {{
            if sleeping {
                sleeping = false;
                if let Some(b) = &bl {
                    b.set(ui.cfg.brightness);
                }
                if let Some(mhz) = pre_sleep_cpu.take() {
                    worker.jobs.lock().push_back(Job::SleepCpu(mhz));
                    worker.wake.notify_one();
                }
                scr.forget();
                dirty = true;
                next_refresh = 0;
                last_input = $now;
            }
        }};
    }

    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let now = time::now_ns();
        let mut wake = u64::MAX;

        if save_at != 0 && now >= save_at {
            save_at = 0;
            worker.jobs.lock().push_back(Job::Save(ui.cfg));
            worker.wake.notify_one();
        }
        if save_at != 0 {
            wake = wake.min(save_at);
        }

        if !owned && game.is_none() && !sleeping {
            let sleep_mins = ui.sleep_minutes() as u64;
            if sleep_mins > 0 {
                let sleep_at = last_input + sleep_mins * 60_000_000_000;
                if now >= sleep_at {
                    enter_sleep!();
                } else {
                    wake = wake.min(sleep_at);
                }
            }
        }

        if !owned && game.is_none() && !sleeping {
            if !cfg_loaded && now >= next_scan && libnx::fs::read_dir("/").is_ok() {
                cfg_loaded = true;
                if let Some(mut cfg) = config::Config::load() {
                    if cfg.brightness > 0 {
                        if let Some(b) = &bl {
                            b.set(cfg.brightness);
                        }
                    } else {
                        cfg.brightness = ui.cfg.brightness;
                    }
                    {
                        let mut q = worker.jobs.lock();
                        if cfg.flight {
                            q.push_back(Job::Act(Action::Flight(true)));
                        }
                        if cfg.volume != audio::DEFAULT_VOL as u8 {
                            q.push_back(Job::Act(Action::Volume(cfg.volume)));
                        }
                    }
                    worker.wake.notify_one();
                    ui.apply_config(cfg);
                    dirty = true;
                }
            }
            // Read once the card is: saving before would lose its lines.
            if !play_loaded && now >= next_scan {
                if let Some(pt) = playtime::Playtime::load() {
                    ui.play = pt;
                    play_loaded = true;
                }
            }
            if ui.page == ui::LIBRARY && now >= next_scan {
                let found = games::scan(ui.take_games());
                next_scan = now
                    + if found.is_empty() {
                        3_000_000_000
                    } else {
                        SCAN_NS
                    };
                ui.set_games(found);
                dirty = true;
            }
            if ui.animating() {
                dirty = true;
            }
            if now >= next_refresh {
                data.refresh();
                next_refresh = now + REFRESH_NS;
                dirty = true;
            }
            let terminal = ui.page == ui::TERMINAL;
            if now >= next_tail {
                dirty |= read_console(&mut term, &mut tail, &mut tail_buf);
                let logs = term.take_logs();
                if !logs.is_empty() {
                    let lines = msgs.write(&logs);
                    if ui.page != ui::LOGS {
                        ui.unread += lines;
                    }
                    dirty = true;
                }
                // Idle reads half a refresh off the statistics' calls.
                next_tail = if terminal || ui.page == ui::LOGS {
                    now + TAIL_NS
                } else {
                    next_refresh + TAIL_IDLE_NS - REFRESH_NS / 2
                };
            }
            if terminal && now / BLINK_NS != blink {
                blink = now / BLINK_NS;
                dirty = true;
            }
            if ui.deadline().is_some_and(|d| now >= d) {
                dirty = true;
            }
            // A held stick repeats its direction, as a pressed key.
            if let Some(b) = ui.stick_repeat(now) {
                if !owned {
                    touches
                        .lock()
                        .push_back((input::KIND_BUTTON, b, 1, 0, 0, input::SRC_NONE));
                    let _ = sys::notify_signal(notif, EV_TOUCH);
                }
            }
            if dirty {
                ui.draw(&mut scr, &data, &msgs, &term, time::now_ns());
                scr.present();
                dirty = false;
            }
            wake = wake.min(next_refresh.min(next_tail));
            if terminal {
                wake = wake.min((blink + 1) * BLINK_NS);
            }
            if let Some(d) = ui.deadline() {
                wake = wake.min(d);
            }
            if let Some(d) = ui.stick_due() {
                wake = wake.min(d);
            }
            if ui.page == ui::LIBRARY {
                wake = wake.min(next_scan);
            }
            if ui.animating() {
                wake = wake.min(now + 16_000_000);
            }
        }
        if let Some(g) = &game {
            wake = wake.min(next_live);
            if now >= next_live {
                next_live = now + LIVE_NS;
                if !libnx::process::live_pids().contains(&g.owner) {
                    release_game!();
                    wake = 0;
                }
            }
        }
        if let Some((_, d)) = game.as_ref().and_then(|g| g.waiter).filter(|w| w.1 != 0) {
            wake = wake.min(d);
        }
        let wait = if wake == u64::MAX {
            0
        } else {
            wake.saturating_sub(time::now_ns()).max(1)
        };
        let Ok(rx) = ipc::recv_timeout(ep, wait) else {
            if let Some(g) = game.as_mut() {
                if let Some((tok, d)) = g.waiter {
                    if d != 0 && time::now_ns() >= d {
                        g.waiter = None;
                        let _ = ipc::reply_to(tok, &Msg::error(-libnx::abi::err::ETIMEDOUT));
                    }
                }
            }
            continue;
        };
        if rx.msg.is_notify() {
            let ev = rx.msg.mr[0];
            if ev & EV_TOUCH != 0 {
                let now = time::now_ns();
                let mut batch: Vec<Event> = touches.lock().drain(..).collect();
                if sleeping {
                    let wake_ev = batch.iter().any(|&(kind, code, id, _, _, _)| {
                        (kind == input::KIND_TOUCH && code == input::TOUCH_DOWN)
                            || (kind == input::KIND_BUTTON && id == 1)
                    });
                    if wake_ev {
                        wake_sleep!(now);
                        finger = None;
                    }
                    continue;
                }
                last_input = now;
                // HOME ends the game (a press; the game never sees it).
                let home = |e: &Event| e.0 == input::KIND_BUTTON && e.1 == input::BTN_HOME;
                if game.is_some() && batch.iter().any(home) {
                    let pressed = batch.iter().any(|e| home(e) && e.2 == 1);
                    batch.retain(|e| !home(e));
                    if pressed && !launcher.stop() {
                        release_game!();
                    }
                }
                // The volume buttons are the console's: the game never sees them.
                let vol = |e: &Event| {
                    e.0 == input::KIND_BUTTON
                        && e.2 == 1
                        && matches!(e.1, input::BTN_VOL_UP | input::BTN_VOL_DOWN)
                };
                let vol_keys: Vec<bool> = batch
                    .iter()
                    .filter(|e| vol(e))
                    .map(|e| e.1 == input::BTN_VOL_UP)
                    .collect();
                batch.retain(|e| {
                    !(e.0 == input::KIND_BUTTON
                        && matches!(e.1, input::BTN_VOL_UP | input::BTN_VOL_DOWN))
                });
                for up in vol_keys {
                    if let Some(Action::Volume(pct)) = ui.volume_step(up, now) {
                        set_volume!(pct, now);
                    }
                    dirty = true;
                }
                // The Power button is the console's too: a short press
                // sleeps, a long one opens the Power sheet. A game or a
                // program owning the screen keeps the screen to itself.
                let power: Vec<u64> = batch
                    .iter()
                    .filter(|e| {
                        e.0 == input::KIND_BUTTON
                            && e.2 == 1
                            && matches!(e.1, input::BTN_POWER | input::BTN_POWER_LONG)
                    })
                    .map(|e| e.1)
                    .collect();
                batch.retain(|e| {
                    !(e.0 == input::KIND_BUTTON
                        && matches!(e.1, input::BTN_POWER | input::BTN_POWER_LONG))
                });
                if game.is_none() && !owned {
                    for code in power {
                        if code == input::BTN_POWER_LONG {
                            ui.power_menu();
                        } else {
                            enter_sleep!();
                        }
                        dirty = true;
                    }
                }
                if let Some(g) = game.as_mut() {
                    for e in batch {
                        let m = Msg::from_words(0, &[e.0, e.1, e.2, e.3, e.4, e.5]);
                        match g.waiter.take() {
                            Some((tok, _)) => {
                                let _ = ipc::reply_to(tok, &m);
                            }
                            None => {
                                if g.events.len() == GAME_EVENTS {
                                    g.events.pop_front();
                                }
                                g.events.push_back(e);
                            }
                        }
                    }
                    continue;
                }
                for (kind, code, id, x, y, src) in batch {
                    // Each controller's state, for the Test tab.
                    if ui.track(kind, code, id, x, y, src) {
                        dirty = true;
                    }
                    // The left stick moves like the D-pad (games get it raw).
                    let (kind, code, id) = if kind == input::KIND_STICK {
                        match ui.stick_nav(code, x, y, now) {
                            Some(b) if !owned => (input::KIND_BUTTON, b, 1),
                            _ => continue,
                        }
                    } else {
                        (kind, code, id)
                    };
                    // Testing, the buttons only light up (HOME leaves).
                    if kind == input::KIND_BUTTON && ui.testing() && code != input::BTN_HOME {
                        continue;
                    }
                    let action = if kind == input::KIND_BUTTON {
                        if owned || id != 1 {
                            continue;
                        }
                        let page = ui.page;
                        let a = ui.pad(code);
                        if ui.page != page {
                            next_tail = 0;
                        }
                        a
                    } else {
                        // One finger at a time; its release where it last was.
                        let (x, y) = match (code, finger) {
                            (input::TOUCH_DOWN, None) => touch_xy(x, y, sw, sh),
                            (_, Some((f, fx, fy))) if f == id => {
                                if code == input::TOUCH_UP {
                                    (fx, fy)
                                } else {
                                    touch_xy(x, y, sw, sh)
                                }
                            }
                            _ => continue,
                        };
                        finger = (code != input::TOUCH_UP).then_some((id, x, y));
                        if owned {
                            continue;
                        }
                        let page = ui.page;
                        let a = ui.touch(code, x, y);
                        if ui.page != page {
                            next_tail = 0; // TERMINAL and LOGS show up to date
                        }
                        a
                    };
                    match action {
                        Some(Action::ClearLogs) => msgs.clear(),
                        Some(Action::Launch(i)) => {
                            if launcher.launch(&mut ui, i) && play_loaded {
                                worker.jobs.lock().push_back(Job::SavePlay(ui.play.text()));
                                worker.wake.notify_one();
                            }
                        }
                        Some(Action::Brightness(pct)) => {
                            if let Some(b) = &bl {
                                b.set(pct);
                            }
                            save_at = now + 800_000_000;
                        }
                        Some(Action::Volume(pct)) => {
                            set_volume!(pct, now);
                        }
                        Some(Action::Save) => {
                            save_at = 0;
                            worker.jobs.lock().push_back(Job::Save(ui.cfg));
                            worker.wake.notify_one();
                        }
                        Some(Action::Flight(off)) => {
                            save_at = 0;
                            let mut q = worker.jobs.lock();
                            q.push_back(Job::Save(ui.cfg));
                            q.push_back(Job::Act(Action::Flight(off)));
                            drop(q);
                            worker.wake.notify_one();
                        }
                        Some(Action::Sleep) => {
                            enter_sleep!();
                        }
                        Some(a) => {
                            worker.jobs.lock().push_back(Job::Act(a));
                            worker.wake.notify_one();
                        }
                        None => {}
                    }
                    dirty = true;
                }
            }
            if ev & EV_EXIT != 0 {
                let ended: Vec<(u32, i64)> = launcher.exits.lock().drain(..).collect();
                for (h, code) in ended {
                    let run = launcher.running.take_if(|r| r.handle == h);
                    if let Some(r) = &run {
                        let secs = (now - r.since) / 1_000_000_000;
                        ui.play.ended(&r.name, secs, time::realtime_ns());
                        if play_loaded {
                            worker.jobs.lock().push_back(Job::SavePlay(ui.play.text()));
                            worker.wake.notify_one();
                        }
                    }
                    let title = run.map(|r| r.title);
                    let _ = sys::handle_close(h);
                    dirty = true;
                    if game.is_some() {
                        release_game!();
                    }
                    // 137: ended by HOME.
                    if let Some(t) = title.filter(|_| code != 0 && code != 137) {
                        ui.toast(format!("{} stopped (code {})", t, code), now);
                    }
                }
            }
            if ev & EV_DONE != 0 {
                let batch: Vec<Done> = worker.done.lock().drain(..).collect();
                let now = time::now_ns();
                for d in batch {
                    if !matches!(
                        d,
                        Done::Fan(_)
                            | Done::Cpu(_)
                            | Done::Gpu(_)
                            | Done::Flight(_)
                            | Done::Audio(_)
                    ) {
                        ui.busy = false;
                    }
                    match d {
                        Done::Fan(r) => {
                            ui.fan_done();
                            match r {
                                Ok(f) => data.fan = stats::Seen::Value(f),
                                Err(e) => {
                                    ui.toast(format!("fan: {}", libnx::abi::err::name(e)), now)
                                }
                            }
                        }
                        Done::Cpu(r) => {
                            ui.cpu_done();
                            match r {
                                Ok(c) => data.cpu = stats::Seen::Value(c),
                                Err(e) => ui
                                    .toast(format!("cpu clock: {}", libnx::abi::err::name(e)), now),
                            }
                        }
                        Done::Gpu(r) => {
                            ui.gpu_done();
                            match r {
                                Ok(()) => {}
                                Err(e) if e == -EIO => ui.toast(
                                    "gpu clock: measured off its step, back to 76.8 MHz".into(),
                                    now,
                                ),
                                Err(e) => ui
                                    .toast(format!("gpu clock: {}", libnx::abi::err::name(e)), now),
                            }
                        }
                        Done::Bt(Ok(msg)) => ui.toast(msg.into(), now),
                        Done::Bt(Err(e)) => {
                            ui.toast(format!("Bluetooth: {}", libnx::abi::err::name(e)), now)
                        }
                        Done::Audio(r) => match r {
                            Ok(Some(msg)) => ui.toast(msg.into(), now),
                            Ok(None) => {}
                            Err(e) => ui.toast(format!("audio: {}", libnx::abi::err::name(e)), now),
                        },
                        Done::Flight(r) => match r {
                            Ok(true) => ui.toast("flight mode on (radio off)".into(), now),
                            Ok(false) => ui.toast("flight mode off".into(), now),
                            Err(e) => ui.toast(format!("wifi: {}", libnx::abi::err::name(e)), now),
                        },
                        Done::Scan(Ok(nets)) => ui.scan = Some(nets),
                        Done::Retry(Ok(t)) => ui.toast(format!("wlan: {}", t), now),
                        Done::Leave(Ok(())) => ui.toast("wlan: left the network".into(), now),
                        Done::Power(e) => {
                            ui.toast(format!("power: {}", libnx::abi::err::name(e)), now)
                        }
                        Done::Scan(Err(e)) | Done::Retry(Err(e)) | Done::Leave(Err(e)) => {
                            ui.toast(format!("wifi: {}", libnx::abi::err::name(e)), now)
                        }
                    }
                    data.soon();
                }
                next_refresh = 0;
                dirty = true;
            }
            continue;
        }
        for h in sent.drain(..) {
            let _ = sys::handle_close(h);
        }
        let reply = match rx.msg.label() {
            proto::OPEN => match sys::handle_dup(scr.fb.handle(), MAP | WRITE | GRANT | DUP, 0) {
                Ok(h2) => {
                    wake_sleep!(now);
                    ipc::set_send_handle(0, h2);
                    sent.push(h2);
                    owned = true;
                    scr.forget();
                    Msg::from_words(
                        0,
                        &[w as u64, h as u64, w as u64 * 4, proto::FORMAT_RGBA8888],
                    )
                    .with_handles(1)
                }
                Err(e) => Msg::error(e),
            },
            // The image as it is, read-only: the UI goes on.
            proto::SNAPSHOT => {
                // What is on display: the UI's image, or a game's.
                let (mem, layout, block) = match game.as_mut() {
                    Some(g) => match still(g) {
                        Some(m) => (m, 1, SCANOUT_BLOCK_LOG2 as u64),
                        None => (&scr.fb, 0, 0),
                    },
                    None => (&scr.fb, 0, 0),
                };
                match sys::handle_dup(mem.handle(), MAP | GRANT | DUP, 0) {
                    Ok(h2) => {
                        ipc::set_send_handle(0, h2);
                        sent.push(h2);
                        Msg::from_words(
                            0,
                            &[
                                w as u64,
                                h as u64,
                                w as u64 * 4,
                                proto::FORMAT_RGBA8888,
                                layout,
                                block,
                            ],
                        )
                        .with_handles(1)
                    }
                    Err(e) => Msg::error(e),
                }
            }
            proto::SCANOUT if game.is_none() && !owned => {
                wake_sleep!(now);
                match take_screen(&dc, w, h, &mut sent) {
                    Ok(mut g) => {
                        g.owner = rx.pid;
                        next_live = time::now_ns() + LIVE_NS;
                        game = Some(g);
                        Msg::from_words(
                            0,
                            &[
                                w as u64,
                                h as u64,
                                proto::FORMAT_RGBA8888,
                                SCANOUT_BLOCK_LOG2 as u64,
                                gpuproto::tiled_bytes(w as u32, h as u32, SCANOUT_BLOCK_LOG2)
                                    as u64,
                            ],
                        )
                        .with_handles(proto::SCANOUT_BUFFERS)
                    }
                    Err(e) => Msg::error(e),
                }
            }
            proto::SCANOUT => Msg::error(-ENODEV),
            proto::FLIP => match game.as_mut() {
                Some(g) if (rx.msg.mr[0] as usize) < g.pas.len() => {
                    g.shown = rx.msg.mr[0] as usize;
                    flip(&dc, g.pas[g.shown]);
                    Msg::new(0, 0, 0)
                }
                _ => Msg::error(-EINVAL),
            },
            proto::RELEASE if game.is_some() => {
                release_game!();
                Msg::new(0, 0, 0)
            }
            proto::EVENT => match game.as_mut() {
                None => Msg::error(-ENODEV),
                Some(g) => match g.events.pop_front() {
                    Some(e) => Msg::from_words(0, &[e.0, e.1, e.2, e.3, e.4, e.5]),
                    None => {
                        let t = rx.msg.mr[0];
                        let deadline = if t == 0 { 0 } else { time::now_ns() + t };
                        g.waiter = Some((rx.token, deadline));
                        continue;
                    }
                },
            },
            proto::WRITE => {
                let n = ipc::unpack(&rx.msg, &mut buf);
                let lines = msgs.write(&buf[..n]);
                if ui.page != ui::LOGS {
                    ui.unread += lines;
                }
                dirty = true;
                Msg::new(0, 0, 0)
            }
            _ => Msg::error(-EINVAL),
        };
        let _ = ipc::reply(&reply);
    }
}

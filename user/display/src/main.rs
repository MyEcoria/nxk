//! display: Tegra X1 DC-A screen; window A on a shared image, drawn as
//! the Switch-style touch UI of `ui`: home row of games and app tiles,
//! quick settings drawer, settings, system, network, USB-C terminal and
//! logs. Redrawn every second and on input. Counts the time played in
//! the games it starts. Over a game (window A on its images), window D
//! (B if D does not fetch) blends the volume indicator, toasts and the
//! drawer (`+`) on top; Sleep holds the game in its next `FLIP`.
//! Panel init is left to the bootloader; only the window is changed. A
//! portrait panel (Switch) is drawn turned.

#![no_std]
#![no_main]

extern crate alloc;

mod art;
mod avatar;
mod backlight;
mod config;
mod controllers;
mod flow;
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
mod users;
mod userspage;
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
use libnx::abi::rights::{DMA, DUP, GRANT, MAP, SEND, WRITE};
use libnx::abi::shm::{CONTIGUOUS, DMA32, UNCACHED};
use libnx::abi::slots;
use libnx::drv::{self, Mmio};
use libnx::fs;
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
const WIN_BUFFER_CONTROL: usize = 0x702;
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
const WIN_BLEND_LAYER_CONTROL: usize = 0x716;
const WIN_BLEND_MATCH_SELECT: usize = 0x717;
const WIN_BLEND_NOMATCH_SELECT: usize = 0x718;
/// Underflows of the selected window (a count).
const WINBUF_UFLOW_STATUS: usize = 0x80A;
const CMD_REG_ACT_CONTROL: usize = 0x43;
const COM_CRC_CONTROL: usize = 0x300;
const COM_CRC_CHECKSUM_LATCHED: usize = 0x329;
/// `CMD_DISPLAY_WINDOW_HEADER`: window A's registers addressed.
const SELECT_A: u32 = 1 << 4;
/// `WIN_BLEND_LAYER_CONTROL`: blending off, the bottom layer.
const BLEND_BYPASS: u32 = 1 << 24;
/// The overlay's layer on top (depth 0) with K1 = K2 = 255, per-pixel
/// premultiplied alpha: color = src + dst * (1 - K1 * src alpha).
const BLEND_TOP: u32 = 255 << 16 | 255 << 8;
const BLEND_PREMULT: u32 = 2 | 6 << 4 | 2 << 8;
/// `WINBUF_SURFACE_KIND`: block-linear, block height in bits 6-4.
const KIND_BLOCK: u32 = 2;
/// Block height of the link images (log2 of GOBs): 16 GOBs.
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
static JOYCON: Service = Service::optional("joycon");

/// What each service powers down asleep, in the order it goes; the wake
/// brings them back in reverse, after the picture and the game.
static SLEEPERS: [&Service; 6] = [&JOYCON, &FAN, &INPUT, &AUDIO, &BT, &WIFI];
/// A service's SUSPEND or RESUME that takes longer is left behind.
const SLEEP_CALL_NS: u64 = 2_000_000_000;

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
    /// A user's `playtime.cfg`: its path and new text.
    SavePlay(String, String),
    /// A user's `profile.cfg`.
    Profile(users::Profile),
    /// A user's directory, or a game's saves, to erase.
    RemoveTree(String),
    /// The console goes to sleep (true) or wakes (false) at the time
    /// given, the brightness to come back to: `sleep_steps`.
    Sleep(bool, u64, u8),
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
    /// The process that took the screen.
    owner: u32,
    /// A copy of it for `SNAPSHOT`: the GPU is drawing on the others, and
    /// a capture takes longer than a frame.
    snap: Option<SharedMemory>,
}

/// The overlay's redraws while the drawer is open over a game (count,
/// total, longest, split); the game's flips are counted by `Link`.
struct Frames {
    since: u64,
    flips: u64,
    /// Underflows of window A and the overlay's when it opened.
    uf: [u32; 2],
    draws: u64,
    draw_ns: u64,
    draw_max: u64,
    split: [u64; 3],
}

impl Frames {
    fn new(now: u64, flips: u64, uf: [u32; 2]) -> Self {
        Self {
            since: now,
            flips,
            uf,
            draws: 0,
            draw_ns: 0,
            draw_max: 0,
            split: [0; 3],
        }
    }

    fn drawn(&mut self, t: [u64; 4]) {
        let ns = t[3] - t[0];
        (self.draws, self.draw_ns, self.draw_max) =
            (self.draws + 1, self.draw_ns + ns, self.draw_max.max(ns));
        for k in 0..3 {
            self.split[k] += t[k + 1] - t[k];
        }
    }

    /// One log line; `scan`'s flips since, `uf`: window A's and the
    /// overlay's underflows now.
    fn report(&self, now: u64, scan: &Link, uf: [u32; 2]) {
        let ms = ((now - self.since) / 1_000_000).max(1);
        let n = self.draws.max(1);
        let flips = scan.flips - self.flips;
        println!(
            "[display] drawer over the game {} ms: {} flips ({}/s), longest gap {} ms, {} late; {} redraws, {} us mean (draw {} present {} ink {}), {} us longest; underflows A {} overlay {}",
            ms,
            flips,
            flips * 1000 / ms,
            scan.gap / 1_000_000,
            scan.late,
            self.draws,
            self.draw_ns / n / 1000,
            self.split[0] / n / 1000,
            self.split[1] / n / 1000,
            self.split[2] / n / 1000,
            self.draw_max / 1000,
            uf[0].wrapping_sub(self.uf[0]),
            uf[1].wrapping_sub(self.uf[1])
        );
    }
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
        let Some(user) = ui.users.active() else {
            ui.toast("Nobody is playing: set up a user first".into(), now);
            return false;
        };
        // The game keeps its data in the user's own directory.
        let dir = users::save_dir(&user.id, &name);
        users::mkdirs(&dir);
        let mut env = libnx::process::current_env();
        env.push(format!("NXK_SAVE_DIR={}", dir));
        env.push(format!("NXK_USER={}", user.name));
        env.push(format!("NXK_USER_ID={}", user.id));
        match libnx::process::spawn_with(&libnx::process::Strings::new(&[&exe], &env)) {
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
/// The clocks set before the sleep: CPU step (MHz, `cpufreq::BOOT`: the
/// loader's), GPU step (kHz) if it was on.
struct Saved {
    cpu: Option<u64>,
    gpu: Option<u64>,
}

/// DSI A (hekate bdk/display/di.h): host packets inside the video stream,
/// the packet's header word; MIPI DCS commands.
const DSI_BASE: u64 = 0x5430_0000;
const DSI_WR_DATA: usize = 0x0A;
const DSI_VIDEO_MODE_CONTROL: usize = 0x4E;
const DSI_CMD_PKT_VID_ENABLE: u32 = 1;
const DCS_SHORT_WRITE: u32 = 0x05;
const DCS_SLEEP_IN: u32 = 0x10;
const DCS_SLEEP_OUT: u32 = 0x11;
const DCS_DISPLAY_OFF: u32 = 0x28;
const DCS_DISPLAY_ON: u32 = 0x29;
/// A frame and the packet's transmission (hekate `display_end`).
const DCS_FRAME_US: u64 = 16_666 + 230;
/// Sleep out to display on: 180 ms, the longest of hekate's panels.
const SLEEP_OUT_MS: u64 = 180;

/// A DCS short write while the panel takes video: host packets on for
/// one frame, sent in its blanking as hekate's `display_end` does.
fn dcs(dsi: &Mmio, cmd: u32) {
    dsi.w32(DSI_VIDEO_MODE_CONTROL * 4, DSI_CMD_PKT_VID_ENABLE);
    dsi.w32(DSI_WR_DATA * 4, cmd << 8 | DCS_SHORT_WRITE);
    time::sleep_us(DCS_FRAME_US);
    dsi.w32(DSI_VIDEO_MODE_CONTROL * 4, 0);
}

/// Window A's fetch on or off (the panel shows the background), under
/// the link lock like every register sequence.
fn window_a(dc: &Mmio, link: &Mutex<Link>, on: bool) {
    let _l = link.lock();
    wr(dc, CMD_DISPLAY_WINDOW_HEADER, SELECT_A);
    wr(dc, WIN_OPTIONS, if on { 1 << 30 } else { 0 });
    wr(dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(dc, CMD_STATE_CONTROL, 1 | 2);
}

/// A GPU step the power source allows, `khz` or the highest below it.
fn gpu_clock_back(khz: u64) {
    for &k in gpuproto::STEPS_KHZ.iter().rev().filter(|&&k| k <= khz) {
        if GPU.call(&Msg::from_words(gpuproto::CLOCK, &[k, 0])).is_ok() {
            return;
        }
    }
}

/// Every sleeper's SUSPEND or RESUME, in turn.
fn tell_sleepers(label: u32, order: impl Iterator<Item = &'static &'static Service>) {
    for s in order {
        let _ = s.call_timeout(&Msg::new(label, 0, 0), SLEEP_CALL_NS);
    }
}

/// Sleep's steps, off the UI loop. Asleep: the services' hardware off,
/// windows A and the overlay's (`ov`; the UI loop programs it again
/// awake) stop fetching, CPU and GPU at their lowest step (an idle GPU
/// at 768 MHz drew 1.8 W, at 76.8 MHz 0.3 W). Awake: the picture and the
/// game's held FLIP first, the services after.
fn sleep_steps(
    on: bool,
    at: u64,
    pct: u8,
    saved: &mut Option<Saved>,
    hw: &Screens,
    link: &Mutex<Link>,
    awake: &Condvar,
) {
    let dc = &hw.dc;
    if on {
        if saved.is_some() {
            return;
        }
        tell_sleepers(libnx::abi::proto::SUSPEND, SLEEPERS.iter());
        window_a(dc, link, false);
        if let Some(v) = hw.ov {
            let _l = link.lock();
            overlay(dc, v, 0, 0, None);
        }
        // The backlight is off already: the panel blanks, then sleeps.
        if let Some(d) = &hw.dsi {
            dcs(d, DCS_DISPLAY_OFF);
            dcs(d, DCS_SLEEP_IN);
        }
        let cpu = CPUFREQ
            .call(&Msg::new(cpufreq::STATUS, 0, 0))
            .ok()
            .map(|r| r.mr[5]);
        let _ = CPUFREQ.call(&Msg::from_words(cpufreq::SET, &[cpufreq::STEPS_MHZ[0]]));
        let gpu_on = GPU
            .call(&Msg::new(gpuproto::STATUS, 0, 0))
            .is_ok_and(|r| r.mr[0] & 0xFF == gpuproto::ON);
        let gpu = gpu_on
            .then(|| GPU.call(&Msg::from_words(gpuproto::CLOCK, &[0, 0])).ok())
            .flatten()
            .map(|r| r.mr[0]);
        if gpu.is_some() {
            let _ = GPU.call(&Msg::from_words(
                gpuproto::CLOCK,
                &[gpuproto::STEPS_KHZ[0], 0],
            ));
        }
        *saved = Some(Saved { cpu, gpu });
        return;
    }
    // The clocks come back while the panel leaves its sleep.
    let woke = time::now_ns();
    if let Some(d) = &hw.dsi {
        dcs(d, DCS_SLEEP_OUT);
    }
    if let Some(sv) = saved.take() {
        if let Some(khz) = sv.gpu {
            gpu_clock_back(khz);
        }
        if let Some(mhz) = sv.cpu {
            let _ = CPUFREQ.call(&Msg::from_words(cpufreq::SET, &[mhz]));
        }
    }
    window_a(dc, link, true);
    if let Some(d) = &hw.dsi {
        let left = (SLEEP_OUT_MS * 1_000_000).saturating_sub(time::now_ns() - woke);
        time::sleep_us(left / 1000);
        dcs(d, DCS_DISPLAY_ON);
        if let Some(b) = &hw.bl {
            b.set(pct);
        }
    }
    {
        let mut l = link.lock();
        l.asleep = false;
        l.slept += time::now_ns().saturating_sub(l.asleep_since);
    }
    awake.notify_all();
    let back = time::now_ns();
    tell_sleepers(libnx::abi::proto::RESUME, SLEEPERS.iter().rev());
    println!(
        "[display] awake: picture in {} ms, services in {} ms",
        back.saturating_sub(at) / 1_000_000,
        time::now_ns().saturating_sub(at) / 1_000_000
    );
}

/// What the sleep steps drive: the display controller, the overlay
/// window, DSI A (`None`: no grant, the panel stays on) and the backlight.
#[derive(Clone, Copy)]
struct Screens {
    dc: Mmio,
    ov: Option<Win>,
    dsi: Option<Mmio>,
    bl: Option<backlight::Backlight>,
}

fn work(w: &Worker, notif: u32, hw: Screens, link: &Mutex<Link>, awake: &Condvar) -> ! {
    let mut saved: Option<Saved> = None;
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
            Job::SavePlay(path, text) => {
                let _ = fs::write(&path, text.as_bytes());
                continue;
            }
            Job::Profile(p) => {
                let _ = users::write_profile(&p);
                continue;
            }
            Job::RemoveTree(path) => {
                let _ = users::remove_tree(&path);
                continue;
            }
            Job::Sleep(on, at, pct) => {
                sleep_steps(on, at, pct, &mut saved, &hw, link, awake);
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
            | Action::Sleep
            | Action::UserSave(..)
            | Action::UserActivate(_)
            | Action::UserDelete(_)
            | Action::EraseSave(..) => continue,
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
fn still(g: &mut Game, shown: usize) -> Option<&SharedMemory> {
    let n = g.images[shown].len();
    if g.snap.is_none() {
        g.snap = Some(SharedMemory::new(n, 0).ok()?);
    }
    let snap = g.snap.as_ref()?;
    snap.as_mut_slice()[..n].copy_from_slice(&g.images[shown].as_slice()[..n]);
    Some(snap)
}

/// The images of a game, black, the first on display, block-linear:
/// handles queued in `sent` for the reply; their addresses.
fn take_screen(
    dc: &Mmio,
    w: usize,
    h: usize,
    sent: &mut Vec<u32>,
) -> Result<(Game, Vec<u32>), i64> {
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
    let g = Game {
        images,
        owner: 0,
        snap: None,
    };
    Ok((g, pas))
}

/// Window A reads `pa` from the next vertical blank; returns once it does.
fn flip(dc: &Mmio, pa: u32) {
    show(dc, pa);
    wait_shown(dc);
}

/// A window of the display controller: A (0) to D (3).
#[derive(Clone, Copy, PartialEq)]
struct Win(u32);

/// The overlay's candidates, the first that fetches its image: D (hekate
/// draws its log on it), then B. A window activated mid-frame stopped
/// fetching for good on the Erista (B, then C, until a reboot).
const OVERLAY_WINS: [Win; 2] = [Win(3), Win(1)];

impl Win {
    /// `CMD_DISPLAY_WINDOW_HEADER`: its registers addressed.
    fn select(self) -> u32 {
        1 << (4 + self.0)
    }

    /// `CMD_STATE_CONTROL`: its update, then its activation request.
    fn update(self) -> u32 {
        1 << (9 + self.0)
    }

    fn act(self) -> u32 {
        1 << (1 + self.0)
    }

    /// `CMD_REG_ACT_CONTROL`: taken at the next line, not the next frame.
    fn hcntr(self) -> u32 {
        1 << [2, 4, 6, 10][self.0 as usize]
    }

    fn name(self) -> char {
        (b'A' + self.0 as u8) as char
    }

    /// Its underflows so far.
    fn underflows(self, dc: &Mmio) -> u32 {
        wr(dc, CMD_DISPLAY_WINDOW_HEADER, self.select());
        let n = dc.r32(WINBUF_UFLOW_STATUS * 4);
        wr(dc, CMD_DISPLAY_WINDOW_HEADER, SELECT_A);
        n
    }
}

/// The display's output CRC over a frame, taken twice; `None` if the two
/// differ.
fn out_crc(dc: &Mmio) -> Option<u32> {
    wr(dc, COM_CRC_CONTROL, 0);
    wr(dc, COM_CRC_CONTROL, (1 << 3) | (1 << 2) | 1); // always, active data, on
    time::sleep_ms(60);
    let a = dc.r32(COM_CRC_CHECKSUM_LATCHED * 4);
    time::sleep_ms(40);
    let b = dc.r32(COM_CRC_CHECKSUM_LATCHED * 4);
    wr(dc, COM_CRC_CONTROL, 0);
    (a == b).then_some(b)
}

/// Whether `win` fetches: a 16x4 white then black patch in the corner
/// (opaque, `buf` rows 0-3 and 4-7, 64 bytes each) give two output CRCs.
fn fetches(dc: &Mmio, win: Win, buf: u32) -> bool {
    let patch = |dc: &Mmio, row: Option<u32>| {
        wr(dc, CMD_DISPLAY_WINDOW_HEADER, win.select());
        if let Some(r) = row {
            wr(dc, WIN_BUFFER_CONTROL, 0);
            wr(dc, WIN_COLOR_DEPTH, R8G8B8A8);
            wr(dc, WIN_POSITION, 0);
            wr(dc, WIN_SIZE, 4 << 16 | 16);
            wr(dc, WIN_PRESCALED_SIZE, 4 << 16 | 64);
            wr(dc, WIN_H_INITIAL_DDA, 0);
            wr(dc, WIN_V_INITIAL_DDA, 0);
            wr(dc, WIN_DDA_INCREMENT, 0x1000_1000);
            wr(dc, WIN_LINE_STRIDE, 64);
            wr(dc, WINBUF_START_ADDR, buf);
            wr(dc, WINBUF_ADDR_H_OFFSET, 0);
            wr(dc, WINBUF_ADDR_V_OFFSET, r);
            wr(dc, WINBUF_SURFACE_KIND, 0);
            wr(dc, WIN_BLEND_LAYER_CONTROL, BLEND_BYPASS);
            wr(dc, WIN_OPTIONS, 1 << 30);
        } else {
            wr(dc, WIN_OPTIONS, 0);
        }
        wr(dc, CMD_DISPLAY_WINDOW_HEADER, SELECT_A);
        wr(dc, CMD_STATE_CONTROL, (1 << 8) | win.update());
        wr(dc, CMD_STATE_CONTROL, 1 | win.act());
        time::sleep_ms(40);
        out_crc(dc)
    };
    let white = patch(dc, Some(0));
    let black = patch(dc, Some(4));
    let off = patch(dc, None);
    println!(
        "[display] window {}: output CRC {:x?} white, {:x?} black, {:x?} off",
        win.name(),
        white,
        black,
        off
    );
    white.is_some() && white != black
}

/// Window A asked to read `pa` from the next vertical blank.
fn show(dc: &Mmio, pa: u32) {
    wr(dc, CMD_DISPLAY_WINDOW_HEADER, SELECT_A);
    wr(dc, WINBUF_START_ADDR, pa);
    wr(dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(dc, CMD_STATE_CONTROL, 1 | 2);
}

/// Until window A took its update (at the vertical blank). Its own request
/// bit only: the overlay's asked meanwhile sets the general one again.
fn wait_shown(dc: &Mmio) {
    for _ in 0..400 {
        if dc.r32(CMD_STATE_CONTROL * 4) & 2 == 0 {
            return;
        }
        time::sleep_us(100);
    }
}

/// What the game's thread shares with the UI's loop, locked around every
/// register sequence too: the game's images, the one on display, its
/// endpoint's badge, the input routed to it and its waiting `EVENT`
/// (reply token, deadline), the flips, the longest gap and the gaps over
/// 25 ms (a frame missed).
struct Link {
    pas: Vec<u32>,
    shown: usize,
    badge: u64,
    events: VecDeque<Event>,
    waiter: Option<(u64, u64)>,
    flips: u64,
    last: u64,
    gap: u64,
    late: u64,
    /// The console sleeps: the game's next `FLIP` waits for the wake.
    asleep: bool,
    /// Since when, and the time slept while this game had the screen
    /// (`FLIP`'s reply: the game's clock leaves it out).
    asleep_since: u64,
    slept: u64,
}

/// Above the UI's threads: a frame asked while the UI draws is not late.
const GAME_PRIO: u8 = 8;

/// The game's `FLIP` and `EVENT`, on their own endpoint (`SCANOUT`'s
/// handle 3): never behind a redraw of the overlay.
fn game_thread(dc: Mmio, ep: u32, link: &Mutex<Link>, awake: &Condvar) -> ! {
    let _ = thread::set_current_priority(GAME_PRIO);
    loop {
        let wait = match link.lock().waiter.map_or(0, |w| w.1) {
            0 => 0,
            d => d.saturating_sub(time::now_ns()).max(1),
        };
        let Ok(rx) = ipc::recv_timeout(ep, wait) else {
            let mut s = link.lock();
            if let Some((tok, _)) = s.waiter.filter(|w| w.1 != 0 && time::now_ns() >= w.1) {
                s.waiter = None;
                let _ = ipc::reply_to(tok, &Msg::error(-libnx::abi::err::ETIMEDOUT));
            }
            continue;
        };
        if rx.msg.label() == proto::EVENT {
            let mut s = link.lock();
            let reply = if s.badge != rx.badge {
                Msg::error(-ENODEV)
            } else if let Some(e) = s.events.pop_front() {
                Msg::from_words(0, &[e.0, e.1, e.2, e.3, e.4, e.5])
            } else {
                let t = rx.msg.mr[0];
                let d = if t == 0 { 0 } else { time::now_ns() + t };
                s.waiter = Some((rx.token, d));
                continue;
            };
            drop(s);
            let _ = ipc::reply(&reply);
            continue;
        }
        let k = rx.msg.mr[0] as usize;
        let ok = rx.msg.label() == proto::FLIP && {
            let mut s = link.lock();
            // Asleep, the game waits in its FLIP: its GPU and CPU idle.
            while s.asleep && s.badge == rx.badge {
                s = awake.wait(s);
            }
            let pa = s.pas.get(k).copied().filter(|_| s.badge == rx.badge);
            pa.inspect(|&pa| show(&dc, pa)).is_some()
        };
        let reply = if ok {
            wait_shown(&dc);
            let t = time::now_ns();
            let mut s = link.lock();
            if s.badge == rx.badge {
                s.shown = k;
            }
            if s.flips > 0 {
                s.gap = s.gap.max(t - s.last);
                s.late += (t - s.last > 25_000_000) as u64;
            }
            (s.flips, s.last) = (s.flips + 1, t);
            Msg::from_words(0, &[s.slept])
        } else {
            Msg::error(-EINVAL)
        };
        let _ = ipc::reply(&reply);
    }
}

/// The overlay window `win`: the part `rect` (x, y, w, h, image pixels)
/// of the UI's image `pa` (`w` wide, premultiplied alpha) over the game,
/// or off. Taken at the next vertical blank; window A addressed after.
fn overlay(dc: &Mmio, win: Win, pa: u32, w: usize, rect: Option<(usize, usize, usize, usize)>) {
    wr(dc, CMD_DISPLAY_WINDOW_HEADER, win.select());
    match rect {
        Some((x, y, rw, rh)) => {
            let (x, y, rw, rh) = (x as u32, y as u32, rw as u32, rh as u32);
            wr(dc, WIN_BUFFER_CONTROL, 0); // the host's memory (not VI or SB2D)
            wr(dc, WIN_COLOR_DEPTH, R8G8B8A8);
            wr(dc, WIN_POSITION, y << 16 | x);
            wr(dc, WIN_SIZE, rh << 16 | rw);
            wr(dc, WIN_PRESCALED_SIZE, (rh << 16) | (rw * 4));
            wr(dc, WIN_H_INITIAL_DDA, 0);
            wr(dc, WIN_V_INITIAL_DDA, 0);
            wr(dc, WIN_DDA_INCREMENT, 0x1000_1000);
            wr(dc, WIN_LINE_STRIDE, w as u32 * 4);
            wr(dc, WINBUF_START_ADDR, pa);
            wr(dc, WINBUF_ADDR_H_OFFSET, x * 4);
            wr(dc, WINBUF_ADDR_V_OFFSET, y);
            wr(dc, WINBUF_SURFACE_KIND, 0);
            wr(dc, WIN_BLEND_LAYER_CONTROL, BLEND_TOP);
            // No color key: the "no match" factors apply; both set alike.
            wr(dc, WIN_BLEND_MATCH_SELECT, BLEND_PREMULT);
            wr(dc, WIN_BLEND_NOMATCH_SELECT, BLEND_PREMULT);
            wr(dc, WIN_OPTIONS, 1 << 30);
        }
        None => wr(dc, WIN_OPTIONS, 0),
    }
    wr(dc, CMD_DISPLAY_WINDOW_HEADER, SELECT_A);
    wr(dc, CMD_STATE_CONTROL, (1 << 8) | win.update());
    wr(dc, CMD_STATE_CONTROL, 1 | win.act());
}

/// Underflows so far of window A and the overlay window.
fn underflows(dc: &Mmio, ov: Option<Win>) -> [u32; 2] {
    [Win(0).underflows(dc), ov.map_or(0, |v| v.underflows(dc))]
}

/// The overlay window: the first candidate that fetches, left off
/// (a display started again finds it as left).
fn pick_overlay(dc: &Mmio) -> Option<Win> {
    let buf = SharedMemory::new(4096, CONTIGUOUS | DMA32 | UNCACHED).ok()?;
    let px = unsafe { core::slice::from_raw_parts_mut(buf.as_ptr() as *mut u32, 128) };
    px[..64].fill(0xFFFF_FFFF);
    px[64..].fill(0xFF00_0000);
    let pa = buf.paddr(0).ok()? as u32;
    let win = OVERLAY_WINS.into_iter().find(|&v| fetches(dc, v, pa));
    match win {
        Some(v) => println!("[display] overlay on window {}", v.name()),
        None => println!("[display] no window fetches (or no output CRC): no overlay over games"),
    }
    win
}

/// The overlay's drawn area widened to 16-pixel columns (64-byte reads).
fn ov_rect(
    r: Option<(usize, usize, usize, usize)>,
    w: usize,
) -> Option<(usize, usize, usize, usize)> {
    r.map(|(x, y, rw, rh)| {
        let (x0, x1) = (x & !15, ((x + rw + 15) & !15).min(w));
        (x0, y, x1 - x0, rh)
    })
}

/// The overlay as shown (`rect` of the UI's image) blended over a game's
/// block-linear copy, as the overlay window does: for `SNAPSHOT`.
fn blend_over(snap: &SharedMemory, scr: &Screen, rect: (usize, usize, usize, usize)) {
    let (iw, _) = scr.image_size();
    let (x0, y0, rw, rh) = rect;
    let px = snap.as_mut_slice();
    for y in y0..y0 + rh {
        for x in x0..x0 + rw {
            let o = scr.shown_at(x, y);
            let a = o >> 24;
            if a == 0 {
                continue;
            }
            let at = gpuproto::tiled_offset(iw as u32, x as u32, y as u32, 4, SCANOUT_BLOCK_LOG2);
            for c in 0..3 {
                let s = (o >> (8 * c) & 0xFF) + (px[at + c] as u32 * (255 - a) + 127) / 255;
                px[at + c] = s.min(255) as u8;
            }
        }
    }
}

/// The UI's image again, pitch-linear.
fn give_back(dc: &Mmio, w: usize, pa: u32) {
    wr(dc, WINBUF_SURFACE_KIND, 0);
    wr(dc, WIN_LINE_STRIDE, w as u32 * 4);
    flip(dc, pa);
}

/// The play time file of the user playing.
fn play_path(ui: &Ui) -> Option<String> {
    ui.users.active().map(|p| users::playtime_path(&p.id))
}

/// User `i` plays: their theme and play time, the library in their order.
/// True if the play time was read (it may be saved).
fn activate(ui: &mut Ui, i: usize) -> bool {
    let Some(p) = ui.users.list.get(i) else {
        return false;
    };
    let (id, light) = (p.id.clone(), p.light);
    ui.users.current = Some(i);
    ui.apply_theme(light);
    let _ = users::write_last(&id);
    let loaded = playtime::Playtime::load(&users::playtime_path(&id));
    let ok = loaded.is_some();
    ui.play = loaded.unwrap_or_default();
    let games = ui.take_games();
    ui.set_games(games);
    ui.usel = i;
    ui.want_saves();
    ok
}

libnx::entry!(main);

fn main() -> i32 {
    let dc = Mmio::map(drv::dev_slot(0), DC, 0x40000).expect("display: registers");
    let bl = backlight::Backlight::open();
    // Granted by a newer init only: without it the panel stays on asleep.
    let dsi_regs = Mmio::map(drv::dev_slot(2), DSI_BASE, 0x1000).ok();
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
    // Window A is the bottom layer, opaque whatever its alpha (a game's).
    wr(&dc, WIN_BLEND_LAYER_CONTROL, BLEND_BYPASS | 255);
    wr(&dc, DISP_BLEND_BACKGROUND_COLOR, 0);
    wr(&dc, CMD_DISPLAY_COMMAND, 1 << 5); // continuous display

    // Window A and the overlay's candidates take their registers at the
    // vertical blank (hekate leaves them at the next line).
    let hcntr = OVERLAY_WINS
        .iter()
        .fold(Win(0).hcntr(), |m, v| m | v.hcntr());
    let act = dc.r32(CMD_REG_ACT_CONTROL * 4);
    wr(&dc, CMD_REG_ACT_CONTROL, act & !hcntr);
    wr(&dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(&dc, CMD_STATE_CONTROL, 1 | 2);
    let ov_win = pick_overlay(&dc);

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
    let link = Arc::new(Mutex::new(Link {
        pas: Vec::new(),
        shown: 0,
        badge: 0,
        events: VecDeque::new(),
        waiter: None,
        flips: 0,
        last: 0,
        gap: 0,
        late: 0,
        asleep: false,
        asleep_since: 0,
        slept: 0,
    }));
    let awake = Arc::new(Condvar::new());
    let worker = Arc::new(Worker {
        jobs: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        done: Mutex::new(VecDeque::new()),
    });
    {
        let (w2, l2, a2) = (worker.clone(), link.clone(), awake.clone());
        let hw = Screens {
            dc,
            ov: ov_win,
            dsi: dsi_regs,
            bl,
        };
        thread::spawn(move || work(&w2, notif, hw, &l2, &a2)).expect("display: worker thread");
    }
    let game_ep = sys::endpoint_create().expect("display: game endpoint");
    {
        let (s2, a2) = (link.clone(), awake.clone());
        thread::spawn(move || game_thread(dc, game_ep, &s2, &a2)).expect("display: game thread");
    }
    // Each game's endpoint has its own badge: an old one is refused.
    let mut link_gen = 0u64;

    let ep = drv::publish("display").expect("display: publish");
    let mut data = stats::Data::new();
    let mut ui = Ui::new();
    ui.has_backlight = bl.is_some();
    if let Some(b) = &bl {
        ui.cfg.brightness = b.percent();
    }
    let mut cfg_loaded = false;
    let mut users_loaded = false;
    let mut play_loaded = false;
    let mut save_at: u64 = 0;
    let mut sleeping = false;
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
    // Over a game: the overlay window's area of the UI's image (None: off), the
    // finger the overlay follows, and the frames counted while the drawer
    // is open (since, flips, longest gap, last flip, longest redraw).
    let mut ov_shown: Option<(usize, usize, usize, usize)> = None;
    let mut ov_finger: Option<u64> = None;
    let mut ov_stats: Option<Frames> = None;
    // The game goes: the UI comes back, the GPU forgets its buffers.
    macro_rules! release_game {
        () => {{
            game = None;
            {
                let mut s = link.lock();
                (s.pas, s.badge, s.waiter) = (Vec::new(), 0, None);
                s.events.clear();
                awake.notify_all();
                if let Some(v) = ov_win {
                    overlay(&dc, v, pa, w, None);
                }
            }
            ov_shown = None;
            ov_finger = None;
            ov_stats = None;
            ui.in_game = false;
            ui.quick = false;
            scr.set_overlay(false);
            // Nothing runs on the GPU any more: it goes off (the clock asked
            // for is kept for the next game).
            let _ = GPU.call(&Msg::new(gpuproto::RESET, 0, 0));
            let _ = GPU.call(&Msg::from_words(gpuproto::POWER, &[0]));
            // No game thread write after: the game's images are gone.
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
                {
                    let mut l = link.lock();
                    (l.asleep, l.asleep_since) = (true, time::now_ns());
                }
                if let Some(b) = &bl {
                    b.off();
                }
                println!("[display] sleep");
                worker
                    .jobs
                    .lock()
                    .push_back(Job::Sleep(true, time::now_ns(), ui.cfg.brightness));
                worker.wake.notify_one();
            }
        }};
    }
    macro_rules! wake_sleep {
        ($now:expr) => {{
            if sleeping {
                sleeping = false;
                let since = link.lock().asleep_since;
                println!(
                    "[display] wake after {} s",
                    $now.saturating_sub(since) / 1_000_000_000
                );
                let pct = ui.cfg.brightness;
                worker.jobs.lock().push_back(Job::Sleep(false, $now, pct));
                ov_shown = None;
                worker.wake.notify_one();
                // With DSI the worker lights it once the panel is on.
                if let (Some(b), None) = (&bl, dsi_regs) {
                    b.set(pct);
                }
                scr.forget();
                dirty = true;
                next_refresh = 0;
                last_input = $now;
            }
        }};
    }

    // What an action asks of the main loop.
    macro_rules! run_action {
        ($action:expr, $now:expr) => {{
            let now = $now;
            match $action {
                Some(Action::ClearLogs) => msgs.clear(),
                Some(Action::Launch(i)) => {
                    if launcher.launch(&mut ui, i) && play_loaded {
                        if let Some(path) = play_path(&ui) {
                            worker
                                .jobs
                                .lock()
                                .push_back(Job::SavePlay(path, ui.play.text()));
                            worker.wake.notify_one();
                        }
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
                    let mut q = worker.jobs.lock();
                    q.push_back(Job::Save(ui.cfg));
                    if let Some(p) = ui.users.take_dirty().and_then(|i| ui.users.list.get(i)) {
                        q.push_back(Job::Profile(p.clone()));
                    }
                    drop(q);
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
                Some(Action::UserSave(i, play)) => {
                    if let Some(p) = ui.users.list.get(i).cloned() {
                        if let Err(e) = users::write_profile(&p) {
                            ui.toast(format!("users: {}", libnx::abi::err::name(e)), now);
                        }
                        if play {
                            // The first user takes over the play time counted before users.
                            if ui.users.list.len() == 1 {
                                if let Ok(old) = fs::read(playtime::LEGACY) {
                                    let _ = fs::write(&users::playtime_path(&p.id), &old);
                                    let _ = fs::remove_file(playtime::LEGACY);
                                }
                            }
                            play_loaded = activate(&mut ui, i);
                        }
                    }
                }
                Some(Action::UserActivate(i)) => {
                    if launcher.running.is_some() {
                        ui.toast("Close the game first".into(), now);
                    } else {
                        play_loaded = activate(&mut ui, i);
                        if ui.page == ui::USERS {
                            ui.go(ui::LIBRARY);
                        }
                    }
                }
                Some(Action::UserDelete(i)) => {
                    if launcher.running.is_some() {
                        ui.toast("Close the game first".into(), now);
                    } else if let Some(dir) = ui.users.list.get(i).map(|p| p.dir()) {
                        worker.jobs.lock().push_back(Job::RemoveTree(dir));
                        worker.wake.notify_one();
                        if let Some(n) = ui.remove_user(i) {
                            play_loaded = activate(&mut ui, n);
                        }
                    }
                }
                Some(Action::EraseSave(u, k)) => {
                    let path = ui
                        .users
                        .list
                        .get(u)
                        .zip(ui.saves.get(k))
                        .map(|(p, (game, _))| users::save_dir(&p.id, game));
                    if let Some(path) = path {
                        worker.jobs.lock().push_back(Job::RemoveTree(path));
                        worker.wake.notify_one();
                        ui.saves.remove(k);
                    }
                }
                Some(a) => {
                    worker.jobs.lock().push_back(Job::Act(a));
                    worker.wake.notify_one();
                }
                None => {}
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
            // The users come with the card; their play time is read when
            // one of them starts playing (saving before would lose its lines).
            if cfg_loaded && !users_loaded {
                if let Some((list, last)) = users::load() {
                    users_loaded = true;
                    ui.users = users::Users::new(list);
                    let pick = last
                        .and_then(|id| ui.users.list.iter().position(|p| p.id == id))
                        .unwrap_or(0);
                    match ui.users.list.get(pick) {
                        None => ui.start_setup(),
                        Some(p) if p.pin.is_some() => ui.start_verify(pick, true),
                        Some(_) => play_loaded = activate(&mut ui, pick),
                    }
                    dirty = true;
                }
            }
            if let Some(id) = ui.take_saves_request() {
                ui.set_saves(&id, users::saves(&id));
                dirty = true;
            }
            if let Some(a) = ui.tick(now) {
                run_action!(Some(a), now);
                dirty = true;
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
                wake = wake.min(now + ui.frame_ns());
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
        // Over a game, only the overlay is drawn: on a change, and when
        // a toast or the volume indicator ends.
        if game.is_some() && !sleeping {
            if ui.deadline().is_some_and(|d| now >= d) {
                dirty = true;
            }
            if ui.quick {
                if let Some(b) = ui.stick_repeat(now) {
                    touches
                        .lock()
                        .push_back((input::KIND_BUTTON, b, 1, 0, 0, input::SRC_NONE));
                    let _ = sys::notify_signal(notif, EV_TOUCH);
                }
                if let Some(d) = ui.stick_due() {
                    wake = wake.min(d);
                }
            }
            if dirty {
                dirty = false;
                let t0 = time::now_ns();
                ui.draw_over(&mut scr, &data, t0);
                let t1 = time::now_ns();
                scr.present();
                let t2 = time::now_ns();
                let rect = ov_rect(scr.ink(), w).filter(|_| ov_win.is_some());
                if let Some(v) = ov_win.filter(|_| rect != ov_shown) {
                    let _s = link.lock();
                    overlay(&dc, v, pa, w, rect);
                    ov_shown = rect;
                }
                if let Some(st) = ov_stats.as_mut() {
                    st.drawn([t0, t1, t2, time::now_ns()]);
                }
            }
            if let Some(d) = ui.deadline() {
                wake = wake.min(d);
            }
        }
        let wait = if wake == u64::MAX {
            0
        } else {
            wake.saturating_sub(time::now_ns()).max(1)
        };
        let Ok(rx) = ipc::recv_timeout(ep, wait) else {
            continue;
        };
        if rx.msg.is_notify() {
            let ev = rx.msg.mr[0];
            if ev & EV_TOUCH != 0 {
                let now = time::now_ns();
                let mut batch: Vec<Event> = touches.lock().drain(..).collect();
                if sleeping {
                    // Only the Power button wakes: a volume key in a bag must not.
                    let wake_ev = batch.iter().any(|&(kind, code, id, _, _, _)| {
                        kind == input::KIND_BUTTON
                            && id == 1
                            && matches!(code, input::BTN_POWER | input::BTN_POWER_LONG)
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
                // sleeps (a game too, held in its FLIP), a long one opens
                // the Power sheet. A program owning the screen keeps it.
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
                if !owned {
                    for code in power {
                        if code != input::BTN_POWER_LONG {
                            ui.quick = false;
                            enter_sleep!();
                        } else if game.is_none() {
                            ui.power_menu();
                        }
                        dirty = true;
                    }
                }
                if game.is_some() {
                    // PLUS is the console's in a game: it opens or closes
                    // the drawer; the game never sees it.
                    let plus = |e: &Event| e.0 == input::KIND_BUTTON && e.1 == input::BTN_PLUS;
                    let mut to_game: Vec<Event> = Vec::new();
                    if batch.iter().any(|e| plus(e) && e.2 == 1) {
                        ui.toggle_quick();
                        dirty = true;
                        if ui.quick {
                            // Nothing stays held in the game: sticks centered.
                            for st in [input::STICK_L, input::STICK_R] {
                                to_game.push((input::KIND_STICK, st, 0, 0, 0, input::SRC_NONE));
                            }
                            data.refresh();
                            let mut s = link.lock();
                            (s.gap, s.late) = (0, 0);
                            let uf = underflows(&dc, ov_win);
                            ov_stats = Some(Frames::new(now, s.flips, uf));
                        }
                    }
                    batch.retain(|e| !plus(e));
                    for e in batch {
                        let (kind, code, id, x, y, _) = e;
                        // A finger belongs to what it went down on: the
                        // overlay (the drawer takes them all) or the game.
                        let action = if kind == input::KIND_TOUCH {
                            let own = match code {
                                input::TOUCH_DOWN if ov_finger.is_none() => {
                                    let (tx, ty) = touch_xy(x, y, sw, sh);
                                    ui.over_hit(tx, ty)
                                }
                                _ => ov_finger == Some(id),
                            };
                            if !own {
                                to_game.push(e);
                                continue;
                            }
                            let (tx, ty) = match (code, finger) {
                                (input::TOUCH_UP, Some((f, fx, fy))) if f == id => (fx, fy),
                                _ => touch_xy(x, y, sw, sh),
                            };
                            let up = code == input::TOUCH_UP;
                            ov_finger = (!up).then_some(id);
                            finger = (!up).then_some((id, tx, ty));
                            ui.touch(code, tx, ty)
                        } else if !ui.quick {
                            to_game.push(e);
                            continue;
                        } else if kind == input::KIND_BUTTON && id == 0 {
                            // Releases still reach the game (its presses did).
                            to_game.push(e);
                            continue;
                        } else if kind == input::KIND_BUTTON {
                            ui.pad(code)
                        } else if kind == input::KIND_STICK {
                            match ui.stick_nav(code, x, y, now) {
                                Some(b) => ui.pad(b),
                                None => continue,
                            }
                        } else {
                            continue;
                        };
                        run_action!(action, now);
                        dirty = true;
                    }
                    if !ui.quick {
                        if let Some(st) = ov_stats.take() {
                            let s = link.lock();
                            st.report(now, &s, underflows(&dc, ov_win));
                        }
                    }
                    let mut s = link.lock();
                    for e in to_game {
                        let m = Msg::from_words(0, &[e.0, e.1, e.2, e.3, e.4, e.5]);
                        match s.waiter.take() {
                            Some((tok, _)) => {
                                let _ = ipc::reply_to(tok, &m);
                            }
                            None => {
                                if s.events.len() == GAME_EVENTS {
                                    s.events.pop_front();
                                }
                                s.events.push_back(e);
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
                    run_action!(action, now);
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
                            if let Some(path) = play_path(&ui) {
                                worker
                                    .jobs
                                    .lock()
                                    .push_back(Job::SavePlay(path, ui.play.text()));
                                worker.wake.notify_one();
                            }
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
                // What is on display: the UI's image, or a game's, copied
                // with no flip meanwhile (the game draws on it two later).
                let held = link.lock();
                let (mem, layout, block) = match game.as_mut() {
                    Some(g) => match still(g, held.shown) {
                        Some(m) => {
                            // With the overlay window adds on top.
                            if let Some(r) = ov_shown {
                                blend_over(m, &scr, r);
                            }
                            (m, 1, SCANOUT_BLOCK_LOG2 as u64)
                        }
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
                let mut s = link.lock();
                link_gen += 1;
                let taken = take_screen(&dc, w, h, &mut sent).and_then(|t| {
                    let fh = sys::handle_dup(game_ep, SEND | GRANT, link_gen)?;
                    ipc::set_send_handle(proto::SCANOUT_BUFFERS, fh);
                    sent.push(fh);
                    Ok(t)
                });
                match taken {
                    Ok((mut g, pas)) => {
                        (s.pas, s.shown, s.badge) = (pas, 0, link_gen);
                        (s.waiter, s.events) = (None, VecDeque::new());
                        s.slept = 0;
                        drop(s);
                        g.owner = rx.pid;
                        next_live = time::now_ns() + LIVE_NS;
                        game = Some(g);
                        ui.in_game = true;
                        ui.quick = false;
                        scr.set_overlay(true);
                        dirty = true;
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
                        .with_handles(proto::SCANOUT_BUFFERS + 1)
                    }
                    Err(e) => Msg::error(e),
                }
            }
            proto::SCANOUT => Msg::error(-ENODEV),
            proto::RELEASE if game.is_some() => {
                release_game!();
                Msg::new(0, 0, 0)
            }
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

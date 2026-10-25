//! display: Tegra X1 DC-A screen; window A on a shared image, drawn as
//! the Switch-style touch UI of `ui`: home row of games and app tiles,
//! quick settings drawer, settings, system, network, USB-C terminal and
//! logs. Redrawn every second and on input. Counts the time played in
//! the games it starts. Over a game (window A on its images), window D
//! (B if D does not fetch) blends the volume indicator, toasts and the
//! drawer (`+`) on top; Sleep holds the game in its next `FLIP`.
//! The panel is the bootloader's at start (adopted as it left it, its id
//! read); asleep the whole display path goes off (`panel`: panel, DSI,
//! the controller, PLLD, the rails) and comes back from reset at the wake,
//! the windows replayed (`dcgate`). A portrait panel (Switch) is drawn
//! turned.

#![no_std]
#![no_main]

extern crate alloc;

mod art;
mod avatar;
mod backlight;
mod cache;
mod catalog;
mod config;
mod controllers;
mod dcgate;
mod flow;
mod font;
mod games;
mod home;
mod keyboard;
mod known;
mod pages;
mod panel;
mod perf;
mod playtime;
mod quick;
mod screen;
mod settings;
mod sheets;
mod speed;
mod speedsheet;
mod stats;
mod status;
mod term;
mod theme;
mod ui;
mod users;
mod userspage;
mod widgets;
mod wifisheet;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::take;
use core::sync::atomic::{AtomicU32, Ordering};

use libnx::abi::err::{EINVAL, EIO, ENODEV, EPERM};
use libnx::abi::proto::gpu as gpuproto;
use libnx::abi::proto::provider as provproto;
use libnx::abi::proto::{
    audio, bt, console as con, cpufreq, display as proto, fan, gpu, input, power, wifi,
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
/// A catalog's covers it had not are asked again this often.
const ABSENT_RETRY_NS: u64 = 10_000_000_000;

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
static EMC: Service = Service::optional("emc");
static GPU: Service = Service::optional("gpu");
static AUDIO: Service = Service::optional("audio");
static BT: Service = Service::optional("bt");
static JOYCON: Service = Service::optional("joycon");

/// What each service powers down asleep, in the order it goes; the wake
/// brings them back in reverse, after the picture and the game.
static SLEEPERS: [&Service; 6] = [&JOYCON, &FAN, &INPUT, &AUDIO, &BT, &WIFI];
/// A service's SUSPEND or RESUME that takes longer is left behind.
const SLEEP_CALL_NS: u64 = 2_000_000_000;

/// Controller register access: behind `dcgate` (a stopped controller must
/// not be touched; its windows are replayed at the wake).
fn wr(dc: &Mmio, reg: usize, v: u32) {
    dcgate::wr(dc, reg, v);
}

fn rd(dc: &Mmio, reg: usize) -> u32 {
    dcgate::rd(dc, reg)
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
    /// A Wi-Fi join ended: the network's name, how, and whether it was
    /// the boot-time one (a failure is then silent).
    Join {
        name: String,
        r: Result<(), i64>,
        auto: bool,
    },
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
        match libnx::process::spawn_with(
            &libnx::process::Strings::new(&[&exe], &env).kill_on_close(),
        ) {
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

/// How long a join may take before the sheet gives up on `wlan`.
const JOIN_NS: u64 = 30_000_000_000;

/// Join network `raw` (its passphrase `psk`, empty if open); `wlan`
/// replies once associated.
fn join(raw: &[u8], psk: &[u8]) -> Result<(), i64> {
    let mut data = raw.to_vec();
    data.push(0);
    data.extend_from_slice(psk);
    // A scan, then the join (15 s at most): a `wlan` that never answers
    // must not leave "Joining..." on the screen.
    WIFI.call_timeout(&ipc::pack_at(wifi::CONNECT, &[0], &data), JOIN_NS)
        .map(|_| ())
}

/// A join in a thread of its own: `wlan` is held for seconds by it, and
/// the worker has sleep steps to run. A network that joined is kept for
/// the next boot.
fn start_join(w: &Arc<Worker>, notif: u32, raw: Vec<u8>, psk: Vec<u8>, auto: bool) {
    let name = String::from_utf8_lossy(&raw).into_owned();
    let (w2, name2) = (w.clone(), name.clone());
    let spawned = thread::spawn(move || {
        let r = join(&raw, &psk);
        if r.is_ok() && !auto {
            let _ = known::Known { ssid: raw, psk }.save();
        }
        w2.done.lock().push_back(Done::Join {
            name: name2,
            r,
            auto,
        });
        let _ = sys::notify_signal(notif, EV_DONE);
    });
    if spawned.is_err() {
        w.done.lock().push_back(Done::Join {
            name,
            r: Err(-libnx::abi::err::EAGAIN),
            auto,
        });
        let _ = sys::notify_signal(notif, EV_DONE);
    }
}

/// The network joined last, tried again at boot until it answers (3
/// times, then the user's to pick).
struct Autojoin {
    net: known::Known,
    tries: u32,
    at: u64,
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
            raw: ssid.to_vec(),
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
    /// The GPU was suspended (`proto::SUSPEND`): `RESUME` brings it back
    /// at its own clock, `gpu` is then `None`.
    gpu_off: bool,
    /// The output CRC of the picture before the sleep, when it is checked
    /// against the one after the wake (the first sleeps with `panel`).
    crc: Option<Option<u32>>,
}

/// Sleeps whose picture is compared before and after (a CRC of the output
/// costs 100 ms at each end).
static CRC_CHECKS: AtomicU32 = AtomicU32::new(3);

/// CPU cores on asleep (core 0 only) and awake (all four).
const CORES_ASLEEP: u64 = 0b0001;
const CORES_AWAKE: u64 = 0b1111;

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

/// How long the GPU's suspend waits for a game to stop at its held
/// `FLIP`: a frame still in flight would wake the GPU again.
const PARK_WAIT_NS: u64 = 50_000_000;
/// The GPU's `RESUME` runs a full bring-up (secure boot, golden context).
const GPU_RESUME_NS: u64 = 5_000_000_000;

/// The GPU powered off with its state kept, once the game (if any) waits
/// in its `FLIP`. False: the service is older than the call, or absent.
fn gpu_suspend(link: &Mutex<Link>) -> bool {
    let t0 = time::now_ns();
    while {
        let l = link.lock();
        l.badge != 0 && !l.parked
    } && time::now_ns() - t0 < PARK_WAIT_NS
    {
        time::sleep_us(1000);
    }
    GPU.call_timeout(&Msg::new(libnx::abi::proto::SUSPEND, 0, 0), SLEEP_CALL_NS)
        .is_ok()
}

/// A GPU step the power source allows, `khz` or the highest below it.
fn gpu_clock_back(khz: u64) {
    for &k in gpuproto::STEPS_KHZ.iter().rev().filter(|&&k| k <= khz) {
        if GPU.call(&Msg::from_words(gpuproto::CLOCK, &[k, 0])).is_ok() {
            return;
        }
    }
}

/// The DRAM clock's sleep drop (`on`) or its restore.
fn emc_sleep(on: bool) {
    let _ = EMC.call_timeout(
        &Msg::from_words(libnx::abi::proto::emc::SLEEP, &[on as u64]),
        SLEEP_CALL_NS,
    );
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
    hw: &mut Screens,
    link: &Mutex<Link>,
    awake: &Condvar,
) {
    let dc = &hw.dc;
    if on {
        if saved.is_some() {
            return;
        }
        // A catalog ends what needs the network (a cloud session) before
        // Wi-Fi goes: as long as a STOP may take; the screen is dark already.
        catalog::tell_sleeper(libnx::abi::proto::SUSPEND, provproto::STOP_NS);
        tell_sleepers(libnx::abi::proto::SUSPEND, SLEEPERS.iter());
        if let Some(v) = hw.ov {
            let _l = link.lock();
            overlay(dc, v, 0, 0, None);
        }
        let check = hw.panel.is_some()
            && CRC_CHECKS
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok();
        let crc = check.then(|| out_crc(dc));
        window_a(dc, link, false);
        // The backlight is off already: the panel blanks, then sleeps; with
        // `panel` the whole display path goes off.
        if let Some(p) = hw.panel.as_mut() {
            p.power_down();
        } else if let Some(d) = &hw.dsi {
            dcs(d, DCS_DISPLAY_OFF);
            dcs(d, DCS_SLEEP_IN);
        }
        let cpu = CPUFREQ
            .call(&Msg::new(cpufreq::STATUS, 0, 0))
            .ok()
            .map(|r| r.mr[5]);
        let _ = CPUFREQ.call(&Msg::from_words(cpufreq::SET, &[cpufreq::STEPS_MHZ[0]]));
        // The GPU off (its state kept); an older service lowers its clock.
        let gpu_off = gpu_suspend(link);
        let gpu_on = !gpu_off
            && GPU
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
        // Cores 1-3 off (PSCI CPU_OFF); no monitor: ENOSYS, they stay.
        let _ = POWER.call(&Msg::from_words(power::CORES, &[CORES_ASLEEP]));
        // Last: the DRAM at its lowest trained rate (a no-op until the
        // `emc` service's sleep mode is on; it restores the rate on wake).
        emc_sleep(true);
        *saved = Some(Saved {
            cpu,
            gpu,
            gpu_off,
            crc,
        });
        return;
    }
    // The clocks come back while the panel leaves its sleep.
    let woke = time::now_ns();
    // First: nothing runs at the sleep rate once awake.
    emc_sleep(false);
    let ready = match hw.panel.as_mut() {
        Some(p) => Some(p.power_up_1()),
        None => {
            if let Some(d) = &hw.dsi {
                dcs(d, DCS_SLEEP_OUT);
            }
            None
        }
    };
    let _ = POWER.call(&Msg::from_words(power::CORES, &[CORES_AWAKE]));
    let sv = saved.take();
    if let Some(sv) = &sv {
        if let Some(mhz) = sv.cpu {
            let _ = CPUFREQ.call(&Msg::from_words(cpufreq::SET, &[mhz]));
        }
        // The GPU's bring-up fits in the panel's sleep-out wait below.
        if sv.gpu_off {
            let t = time::now_ns();
            let r = GPU.call_timeout(&Msg::new(libnx::abi::proto::RESUME, 0, 0), GPU_RESUME_NS);
            println!(
                "[display] gpu resumed in {} ms ({})",
                (time::now_ns() - t) / 1_000_000,
                if r.is_ok() { "ok" } else { "failed" }
            );
        }
        if let Some(khz) = sv.gpu {
            gpu_clock_back(khz);
        }
    }
    if let (Some(p), Some(ready)) = (hw.panel.as_mut(), ready) {
        // The panel's out of sleep: video back, the windows replayed.
        p.power_up_2(ready);
        window_a(dc, link, true);
        if let Some(Some(before)) = sv.as_ref().and_then(|s| s.crc) {
            let after = out_crc(dc);
            println!(
                "[display] output CRC before sleep {:#x}, after wake {:x?}: {}",
                before,
                after,
                if after == Some(before) { "same" } else { "DIFFERENT" }
            );
        } else if sv.as_ref().is_some_and(|s| s.crc.is_some()) {
            println!("[display] output CRC before sleep: none stable, not compared");
        }
        if let Some(b) = &hw.bl {
            b.set(pct);
        }
    } else {
        window_a(dc, link, true);
        if let Some(d) = &hw.dsi {
            let left = (SLEEP_OUT_MS * 1_000_000).saturating_sub(time::now_ns() - woke);
            time::sleep_us(left / 1000);
            dcs(d, DCS_DISPLAY_ON);
            if let Some(b) = &hw.bl {
                b.set(pct);
            }
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
    catalog::tell_sleeper(libnx::abi::proto::RESUME, SLEEP_CALL_NS);
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
    /// The display path's own bring-up (`None`: the loader's, left on).
    panel: Option<panel::Panel>,
}

fn work(w: &Worker, notif: u32, mut hw: Screens, link: &Mutex<Link>, awake: &Condvar) -> ! {
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
                sleep_steps(on, at, pct, &mut saved, &mut hw, link, awake);
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
            Action::Leave => Done::Leave(
                WIFI.call(&Msg::new(wifi::DISCONNECT, 0, 0))
                    .map(|_| known::forget()),
            ),
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
            Action::Volume(pct) => {
                // At boot `audio` may not be published yet: the saved level
                // must still land, its default is loud.
                let m = Msg::from_words(audio::VOLUME, &[pct as u64]);
                let mut r = AUDIO.call(&m);
                for _ in 0..40 {
                    if r.is_ok() {
                        break;
                    }
                    time::sleep_ms(250);
                    r = AUDIO.call(&m);
                }
                Done::Audio(Ok(r.ok().and(None)))
            }
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
            | Action::EraseSave(..)
            | Action::Service(_)
            | Action::Join
            | Action::SpeedStart
            | Action::SpeedStop
            | Action::Cancel => continue,
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
        let h2 = sys::handle_dup(m.handle(), MAP | WRITE | DMA | GRANT | DUP, 0)?;
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
        let n = rd(dc, WINBUF_UFLOW_STATUS);
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
    let a = rd(dc, COM_CRC_CHECKSUM_LATCHED);
    time::sleep_ms(40);
    let b = rd(dc, COM_CRC_CHECKSUM_LATCHED);
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
        if rd(dc, CMD_STATE_CONTROL) & 2 == 0 {
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
    /// The same since the overlay's last second (`perf`).
    hud_gap: u64,
    hud_late: u64,
    /// `slept` as the last flip saw it: a flip held by a sleep is no gap.
    hud_slept: u64,
    /// The console sleeps: the game's next `FLIP` waits for the wake.
    asleep: bool,
    /// Since when, and the time slept while this game had the screen
    /// (`FLIP`'s reply: the game's clock leaves it out).
    asleep_since: u64,
    slept: u64,
    /// The game's `FLIP` waits for the wake (`asleep`): it has stopped
    /// using the GPU.
    parked: bool,
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
                s.parked = true;
                s = awake.wait(s);
            }
            s.parked = false;
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
                let late = t - s.last > perf::LATE_NS;
                s.gap = s.gap.max(t - s.last);
                s.late += late as u64;
                if s.slept == s.hud_slept {
                    s.hud_gap = s.hud_gap.max(t - s.last);
                    s.hud_late += late as u64;
                }
            }
            s.hud_slept = s.slept;
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
    let (id, light, service) = (p.id.clone(), p.light, p.service.clone());
    ui.users.current = Some(i);
    ui.show_service(service);
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
    // Before the first read of the controller: a stopped display path is
    // brought up here (its picture is then dark until the backlight).
    let panel = dsi_regs.and_then(|d| panel::Panel::open(dc, d));
    if let (Some(p), Some(b)) = (&panel, &bl) {
        if p.cold_started() {
            b.set(100);
        }
    }
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
    let act = rd(&dc, CMD_REG_ACT_CONTROL);
    wr(&dc, CMD_REG_ACT_CONTROL, act & !hcntr);
    wr(&dc, CMD_STATE_CONTROL, (1 << 8) | (1 << 9));
    wr(&dc, CMD_STATE_CONTROL, 1 | 2);
    let ov_win = pick_overlay(&dc);
    // What the loader left in the windows is part of the state replayed at a wake.
    dcgate::adopt(&dc);

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
        hud_gap: 0,
        hud_late: 0,
        hud_slept: 0,
        asleep: false,
        asleep_since: 0,
        slept: 0,
        parked: false,
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
            panel,
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
    let mut autojoin: Option<Autojoin> = None;
    let mut wifi_gone = false;
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
    // The catalogs on the card, the one running (for the user and the
    // catalog shown), when it may start again after a failure (and how many
    // failed in a row), its game's play time key and start, and when a
    // STOP it got unanswered ends it.
    let mut services: Vec<catalog::ServiceInfo> = Vec::new();
    let mut prov: Option<catalog::Provider> = None;
    // The speed test program, alive while its sheet is open.
    let mut speed: Option<speed::Speed> = None;
    // What the card keeps of each catalog for each user (`cache.rs`), the
    // catalogs refreshed since the boot, and the covers a refresh of a
    // catalog nobody looks at still waits for.
    let cache = cache::Cache::start(notif);
    let mut kept: BTreeMap<(String, String), cache::Kept> = BTreeMap::new();
    let mut refreshed: Vec<(String, String)> = Vec::new();
    let mut hidden_need: Vec<String> = Vec::new();
    let (mut hidden_list, mut hidden_retry) = (0u64, 0u64);
    let (mut prov_retry_at, mut prov_fails) = (0u64, 0u32);
    let mut prov_key: Option<String> = None;
    // The version of the launch's own ST_LAUNCHING (LAUNCH's reply).
    let mut launch_version: Option<u64> = None;
    let mut prov_play: Option<u64> = None;
    let mut stop_at: Option<u64> = None;
    let mut shown_wait: Option<u64> = None;
    let mut absent_retry = 0u64;
    // Over a game: the overlay window's area of the UI's image (None: off), the
    // finger the overlay follows, and the frames counted while the drawer
    // is open (since, flips, longest gap, last flip, longest redraw).
    let mut ov_shown: Option<(usize, usize, usize, usize)> = None;
    let mut ov_finger: Option<u64> = None;
    let mut ov_stats: Option<Frames> = None;
    // The performance overlay: its second's clock and when the next
    // sample is due; whether the catalog game running streams from a server.
    let mut perf_clock: Option<perf::Clock> = None;
    let mut next_perf = 0u64;
    let mut prov_cloud = false;
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
            (perf_clock, ui.perf, next_perf) = (None, None, 0);
            if let Some(p) = &prov {
                p.want_stats(false);
            }
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
    // A catalog game is over: its play time counted and saved.
    macro_rules! catalog_game_over {
        ($now:expr) => {{
            if let (Some(since), Some(key)) = (prov_play.take(), prov_key.as_ref()) {
                let secs = ($now - since) / 1_000_000_000;
                ui.play.ended(key, secs, time::realtime_ns());
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
            prov_key = None;
            prov_cloud = false;
            launch_version = None;
            ui.starting = None;
            stop_at = None;
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
                // The overlay's second spans the sleep: start it again.
                perf_clock = None;
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
                Some(Action::Launch(i)) if !ui.service.is_empty() => {
                    let ready = prov.as_ref().filter(|p| p.pid().is_some());
                    match (ready, ui.game(i)) {
                        _ if ui.users.active().is_none() => {
                            ui.toast("Nobody is playing: set up a user first".into(), now)
                        }
                        _ if launcher.running.is_some() || ui.catalog_busy() => {
                            ui.toast("A game is running".into(), now)
                        }
                        (Some(p), Some(g)) => {
                            p.launch(&g.id);
                            let (key, title, cover, cloud) =
                                (g.name.clone(), g.title.clone(), g.cover.clone(), g.cloud);
                            ui.starting = Some((title, cover, now));
                            ui.play.started(&key);
                            let games = ui.take_games();
                            ui.set_games(games);
                            (prov_key, launch_version, prov_play) = (Some(key), None, None);
                            prov_cloud = cloud;
                            if let Some(path) = play_path(&ui).filter(|_| play_loaded) {
                                worker
                                    .jobs
                                    .lock()
                                    .push_back(Job::SavePlay(path, ui.play.text()));
                                worker.wake.notify_one();
                            }
                        }
                        _ => ui.toast("The service is not ready".into(), now),
                    }
                }
                Some(Action::Service(k)) => {
                    if !ui.catalog_busy() {
                        ui.pick_service(k);

                        if let Some(c) = ui.users.current {
                            if let Some(p) = ui.users.list.get_mut(c) {
                                p.service = ui.service.clone();
                                worker.jobs.lock().push_back(Job::Profile(p.clone()));
                                worker.wake.notify_one();
                            }
                        }
                        next_scan = 0;
                    }
                }
                Some(Action::Cancel) => {
                    if let Some(p) = &prov {
                        p.stop();
                        stop_at.get_or_insert(now + provproto::STOP_NS);
                    } else {
                        ui.starting = None;
                    }
                }
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
                Some(Action::SpeedStart) => {
                    if speed.is_none() {
                        match speed::Speed::start(notif) {
                            Ok(s) => speed = Some(s),
                            Err(e) => ui.speed_gone(format!(
                                "cannot start the test: {}",
                                libnx::abi::err::name(e)
                            )),
                        }
                    }
                    if let Some(s) = &speed {
                        s.begin();
                    }
                }
                Some(Action::SpeedStop) => {
                    if let Some(s) = &speed {
                        s.end();
                    }
                }
                Some(Action::Join) => {
                    if let Some((raw, psk)) = ui.take_join() {
                        autojoin = None;
                        start_join(&worker, notif, raw, psk, false);
                    }
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
                    if launcher.running.is_some() || ui.catalog_busy() {
                        ui.toast("Close the game first".into(), now);
                    } else {
                        play_loaded = activate(&mut ui, i);
                        if ui.page == ui::USERS {
                            ui.go(ui::LIBRARY);
                        }
                    }
                }
                Some(Action::UserDelete(i)) => {
                    if launcher.running.is_some() || ui.catalog_busy() {
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
                autojoin = known::Known::load().map(|net| Autojoin {
                    net,
                    tries: 0,
                    at: 0,
                });
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
                services = catalog::scan();
                ui.set_services(services.clone());
                // A catalog picked but gone from the card stays picked.
                if !ui.service.is_empty() && !services.iter().any(|s| s.name == ui.service) {
                    ui.prov = Some(catalog::View::local(
                        provproto::ST_ERROR,
                        format!("/services/{} is not on the card", ui.service),
                    ));
                }
                if ui.service.is_empty() {
                    let found = games::scan(ui.take_games());
                    next_scan = now
                        + if found.is_empty() {
                            3_000_000_000
                        } else {
                            SCAN_NS
                        };
                    ui.set_games(found);
                } else {
                    next_scan = now + SCAN_NS;
                }
                dirty = true;
            }
            // What the card kept of each catalog, for the user playing.
            let uid = ui.users.active().map(|u| u.id.clone());
            if let Some(uid) = &uid {
                for s in services.iter() {
                    let k = kept.entry((uid.clone(), s.name.clone())).or_default();
                    if !k.asked {
                        k.asked = true;
                        cache.post(cache::Job::LoadList(uid.clone(), s.name.clone()));
                    }
                }
                // Its list stands in for the catalog's until it answers.
                if !ui.service.is_empty() && ui.prov_list == 0 && ui.games.is_empty() {
                    let games = kept
                        .get(&(uid.clone(), ui.service.clone()))
                        .and_then(|k| k.games.as_ref())
                        .filter(|g| !g.is_empty());
                    if let Some(g) = games {
                        let name = ui.service.clone();
                        ui.show_cached(&name, g);
                        dirty = true;
                    }
                }
            }
            // The catalog shown runs for the user playing; another one goes.
            // With none shown, one not looked at since the boot runs to keep
            // its list and covers up to date, once the first frames are out.
            let visible = (!ui.service.is_empty())
                .then(|| services.iter().find(|s| s.name == ui.service))
                .flatten();
            // A refresh nobody looks at asks again for the covers its catalog
            // had not yet (it reads them off the card after it is up).
            if let Some(p) = prov
                .as_ref()
                .filter(|p| p.pid().is_some() && p.name != ui.service)
            {
                if hidden_list != 0 && !hidden_need.is_empty() && now >= hidden_retry {
                    p.covers(hidden_list, hidden_need.clone());
                    hidden_retry = now + 2_000_000_000;
                }
                if hidden_list != 0 {
                    wake = wake.min(hidden_retry);
                }
            }
            // A refresh nobody looks at gives up after a minute.
            if let Some(p) = prov.as_ref().filter(|p| p.name != ui.service) {
                if now - p.since > 60_000_000_000 {
                    println!("[display] refresh done: timeout");
                    refreshed.push((p.user.clone(), p.name.clone()));
                }
            }
            let hidden = visible
                .is_none()
                .then(|| {
                    let uid = uid.as_ref()?;
                    let idle = launcher.running.is_none() && game.is_none() && !sleeping;
                    services.iter().find(|s| {
                        let key = (uid.clone(), s.name.clone());
                        idle && now > 6_000_000_000
                            // Only a catalog that has been used: one with no
                            // account would ask Amazon for a sign-in code at
                            // every boot.
                            && kept.get(&key).is_some_and(|k| k.games.is_some())
                            && !refreshed.contains(&key)
                    })
                })
                .flatten();
            let want = visible.or(hidden).zip(ui.users.active());
            let same = match (&prov, want) {
                (Some(p), Some((s, u))) => p.name == s.name && p.user == u.id,
                (None, None) => true,
                _ => false,
            };
            if !same && !ui.catalog_busy() {
                if prov.take().is_some() {
                    ui.prov = None;
                }
                if let Some((s, u)) = want.filter(|_| now >= prov_retry_at) {
                    if visible.is_none() {
                        println!("[display] {}: refreshing its list and covers", s.name);
                    }
                    (hidden_need, hidden_list) = (Vec::new(), 0);
                    match catalog::Provider::start(s, u, notif) {
                        Ok(p) => prov = Some(p),
                        Err(e) => {
                            ui.prov = Some(catalog::View::local(
                                provproto::ST_ERROR,
                                format!("cannot start it: {}", libnx::abi::err::name(e)),
                            ));
                            prov_retry_at = now + 5_000_000_000;
                        }
                    }
                    dirty = true;
                }
            }
            let live = prov
                .as_ref()
                .is_some_and(|p| p.pid().is_some() && p.name == ui.service)
                && ui.prov_list != 0
                && ui.prov_list != ui::CACHED_LIST;
            if live {
                // Covers the catalog had not are asked again now and then
                // too: a cover set while its ENOENT was on its way is not missed.
                if !ui.covers_absent.is_empty() {
                    if now >= absent_retry {
                        ui.covers_absent.clear();
                        absent_retry = now + ABSENT_RETRY_NS;
                    }
                    wake = wake.min(absent_retry);
                }
            }
            if let Some(uid) = uid.as_ref().filter(|_| ui.prov_list != 0) {
                if let Some(ids) = ui.want_covers(live) {
                    // The card first; what it has not goes to the catalog.
                    let (fresh, absent): (Vec<String>, Vec<String>) = ids
                        .into_iter()
                        .partition(|id| !ui.cache_absent.contains(id));
                    if !fresh.is_empty() {
                        let job = cache::Job::LoadCovers(
                            uid.clone(),
                            ui.service.clone(),
                            ui.prov_list,
                            fresh,
                        );
                        cache.post(job);
                    }
                    if let (true, Some(p)) = (live && !absent.is_empty(), &prov) {
                        p.covers(ui.prov_list, absent);
                    }
                }
            }
            if prov.is_none() && !ui.service.is_empty() && prov_retry_at > now {
                wake = wake.min(prov_retry_at);
            }
            if ui.animating() {
                dirty = true;
            }
            if now >= next_refresh {
                data.refresh();
                next_refresh = now + REFRESH_NS;
                dirty = true;
                // `net` keeps its order of links in memory: say it again
                // when it differs (a restart, a change from the page).
                if let stats::Now::Value(n) = data.net {
                    let want = if ui.cfg.wired {
                        libnx::abi::proto::net::PREF_WIRED
                    } else {
                        libnx::abi::proto::net::PREF_WIFI
                    };
                    if n.pref != 0 && n.pref != want {
                        stats::prefer_links(ui.cfg.wired);
                        next_refresh = now + 300_000_000;
                    }
                }
            }
            if speed.is_some() && !ui.speed_open_now() {
                speed = None;
            }
            let idle = matches!(data.wifi, stats::Seen::Value(w) if w.state == wifi::ST_IDLE);
            // The Wi-Fi driver went and came back (a crash, a reload): the
            // network kept is joined again, as at the boot.
            let gone = matches!(data.wifi, stats::Seen::Missing)
                || matches!(data.wifi, stats::Seen::Value(w) if w.state == wifi::ST_DOWN)
                || matches!(data.wifi, stats::Seen::Value(w) if w.state == wifi::ST_CONNECTED);
            if gone {
                // Connected, or the driver is out: if it is idle next, the
                // link was lost, not left (Leave forgets the network).
                wifi_gone = true;
            } else if idle && wifi_gone {
                wifi_gone = false;
                println!("[display] Wi-Fi idle after a lost link: joining the saved network again");
                if autojoin.is_none() && !ui.cfg.flight {
                    autojoin = known::Known::load().map(|net| Autojoin {
                        net,
                        tries: 0,
                        at: now + 2_000_000_000,
                    });
                }
            }
            if let Some(a) = autojoin.as_mut().filter(|a| now >= a.at) {
                if idle && !ui.cfg.flight && ui.wifi.joining.is_none() && !sleeping {
                    a.tries += 1;
                    a.at = now + 30_000_000_000;
                    ui.wifi.joining = Some(String::from_utf8_lossy(&a.net.ssid).into_owned());
                    let (ssid, psk) = (a.net.ssid.clone(), a.net.psk.clone());
                    start_join(&worker, notif, ssid, psk, true);
                    if a.tries >= 3 {
                        autojoin = None;
                    }
                }
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
            // The launch card counts the seconds waited.
            let waited = ui.starting.as_ref().map(|s| (now - s.2) / 1_000_000_000);
            if waited.is_some() && waited != shown_wait {
                shown_wait = waited;
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
            if let (Some(st), Some(n)) = (&ui.starting, shown_wait) {
                wake = wake.min(st.2 + (n + 1) * 1_000_000_000);
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
        if let Some(at) = stop_at {
            if now >= at {
                stop_at = None;
                if let Some(p) = &prov {
                    println!("[display] {}: no answer to STOP, ended", p.title);
                    p.kill();
                }
            } else {
                wake = wake.min(at);
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
            if ui.cfg.perf && ov_win.is_some() {
                wake = wake.min(next_perf);
                if now >= next_perf {
                    next_perf = now + perf::EVERY_NS;
                    let (flips, hud) = {
                        let mut s = link.lock();
                        (s.flips, (take(&mut s.hud_gap), take(&mut s.hud_late)))
                    };
                    let stream = prov.as_ref().filter(|_| prov_cloud).and_then(|p| p.stats());
                    ui.perf = Some(match perf_clock.as_mut() {
                        Some(c) => c.sample(now, flips, hud, &mut data, prov_cloud, stream),
                        None => {
                            perf_clock = Some(perf::Clock::new(now, flips));
                            perf::Sample {
                                cloud: prov_cloud,
                                ..Default::default()
                            }
                        }
                    });
                    dirty = true;
                }
                if let Some(p) = &prov {
                    p.want_stats(prov_cloud);
                }
            } else if perf_clock.is_some() || ui.perf.is_some() {
                (perf_clock, ui.perf, next_perf) = (None, None, 0);
                if let Some(p) = &prov {
                    p.want_stats(false);
                }
                dirty = true;
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
                    let owner = game.as_ref().map(|g| g.owner);
                    let catalog_game = prov
                        .as_ref()
                        .filter(|p| p.pid().is_some() && p.pid() == owner);
                    if let (true, Some(p)) = (pressed, catalog_game) {
                        if stop_at.is_none() {
                            p.stop();
                            stop_at = Some(now + provproto::STOP_NS);
                            let title = prov_key
                                .as_ref()
                                .and_then(|k| ui.games.iter().find(|g| g.name == *k))
                                .map_or(p.title.clone(), |g| g.title.clone());
                            ui.toast(format!("Closing {}", title), now);
                            dirty = true;
                        }
                    } else if pressed && !launcher.stop() {
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
            if ev & speed::EV_SPEED != 0 {
                let up = speed.as_ref().map(|s| s.take()).unwrap_or_default();
                if let Some(r) = up.report {
                    ui.speed_report(r);
                    dirty = true;
                }
                if let Some(e) = up.start_failed {
                    let why = if e == -libnx::abi::err::ENOENT {
                        "speedtest is not on the card (/bin/speedtest)".into()
                    } else {
                        format!("cannot start the test: {}", libnx::abi::err::name(e))
                    };
                    ui.speed_gone(why);
                    speed = None;
                    dirty = true;
                } else if let Some(e) = up.refused {
                    ui.speed_gone(format!(
                        "the test was refused: {}",
                        libnx::abi::err::name(e)
                    ));
                    dirty = true;
                } else if up.exit.is_some() {
                    ui.speed_gone("the test program ended".into());
                    speed = None;
                    dirty = true;
                }
            }
            if ev & cache::EV_CACHE != 0 {
                let me = ui.users.active().map(|u| u.id.clone());
                for o in cache.take() {
                    match o {
                        cache::Out::List(u, s, list, have) => {
                            println!(
                                "[display] cache {}: {} games, {} covers",
                                s,
                                list.as_ref().map_or(0, |l| l.len()),
                                have.len()
                            );
                            let k = kept.entry((u, s)).or_default();
                            (k.asked, k.loaded, k.games, k.have) = (true, true, list, have);
                        }
                        cache::Out::Cover(u, s, list, id, c) => {
                            if me.as_deref() == Some(u.as_str()) && s == ui.service {
                                ui.set_cover(list, &id, catalog::cover(c));
                                dirty = true;
                            }
                        }
                        cache::Out::Absent(u, s, list, ids) => {
                            if me.as_deref() == Some(u.as_str()) && s == ui.service {
                                ui.cache_missed(list, ids);
                                dirty = true;
                            }
                        }
                    }
                }
            }
            if ev & catalog::EV_PROV != 0 {
                let now = time::now_ns();
                let up = prov.as_ref().map(|p| p.take()).unwrap_or_default();
                // What it says is for the screen only if it is the catalog
                // shown; a refresh nobody looks at only feeds the cache.
                let shown = prov.as_ref().is_some_and(|p| p.name == ui.service);
                let key = prov.as_ref().map(|p| (p.user.clone(), p.name.clone()));
                dirty |= shown && !up.is_quiet(ui.prov.as_ref());
                if up.launched.is_some() && prov_key.is_some() {
                    launch_version = up.launched;
                }
                if let Some(v) = up.view {
                    // No account: what the card kept of its games goes.
                    if let (true, Some(k)) = (v.state == provproto::ST_LOGIN, &key) {
                        cache.post(cache::Job::Forget(k.0.clone(), k.1.clone()));
                        if let Some(e) = kept.get_mut(k) {
                            (e.games, e.have) = (None, Vec::new());
                        }
                        if shown {
                            ui.drop_cached();
                        }
                    }
                    if shown {
                        if ui.prov.as_ref().is_some_and(|o| o.covers != v.covers) {
                            ui.covers_absent.clear();
                        }
                        match (&prov, v.state == provproto::ST_LOGIN) {
                            (Some(p), true) => p.login_art(),
                            _ => ui.login_art = None,
                        }
                        ui.prov = Some(v);
                    } else if let Some(k) = &key {
                        if matches!(v.state, provproto::ST_LOGIN | provproto::ST_ERROR)
                            && !refreshed.contains(k)
                        {
                            println!("[display] refresh done: state");
                            refreshed.push(k.clone());
                        }
                    }
                }
                if let Some(art) = up.login_art.filter(|_| shown) {
                    ui.login_art = art.map(catalog::cover);
                }
                // The launch's own state, or a newer one: playing, or over
                // (READY with why, LOGIN, ERROR) even if LAUNCHING was missed.
                let since_launch = ui
                    .prov
                    .as_ref()
                    .zip(launch_version)
                    .filter(|(v, lv)| v.version >= *lv)
                    .map(|(v, lv)| (v.state, v.version > lv));
                match since_launch {
                    Some((provproto::ST_PLAYING, _)) => {
                        ui.starting = None;
                        prov_play.get_or_insert(now);
                    }
                    Some((provproto::ST_LAUNCHING, _)) | Some((_, false)) | None => {}
                    Some(_) => {
                        // Why it did not start, or how it ended.
                        let why = ui.prov.as_ref().map_or(String::new(), |v| v.text.clone());
                        if !why.is_empty() {
                            ui.toast(why, now);
                        }
                        catalog_game_over!(now);
                    }
                }
                if up.stopped {
                    stop_at = None;
                }
                for (list, id, absent) in up.covers_missed {
                    if shown {
                        ui.cover_missed(list, &id, absent);
                    } else if !absent {
                        // Not there yet (`absent`) is asked again below.
                        hidden_need.retain(|i| *i != id);
                    }
                }
                // An empty list is a catalog still loading, not proof that the
                // games are gone: what the card keeps stays, and a refresh
                // nobody looks at waits for a real one.
                let up_games = up
                    .games
                    .filter(|(_, g)| !g.is_empty() || (shown && ui.prov_list != ui::CACHED_LIST));
                if let (Some((list, g)), Some(p)) = (up_games, &prov) {
                    let key = (p.user.clone(), p.name.clone());
                    let entries: Vec<cache::Entry> = g
                        .iter()
                        .map(|x| cache::Entry {
                            id: x.id.clone(),
                            title: x.title.clone(),
                            publisher: x.publisher.clone(),
                            version: x.version.clone(),
                            desc: x.desc.clone(),
                            color: x.color,
                            cloud: x.cloud,
                        })
                        .collect();
                    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
                    if !entries.is_empty() {
                        cache.post(cache::Job::SaveList(
                            key.0.clone(),
                            key.1.clone(),
                            entries.clone(),
                        ));
                        cache.post(cache::Job::Prune(key.0.clone(), key.1.clone(), ids.clone()));
                    }
                    let k = kept.entry(key.clone()).or_default();
                    k.have.retain(|h| ids.contains(h));
                    let missing: Vec<String> = ids
                        .iter()
                        .filter(|i| !k.have.contains(i))
                        .cloned()
                        .collect();
                    if !entries.is_empty() {
                        (k.asked, k.loaded, k.games) = (true, true, Some(entries));
                    }
                    if shown {
                        let mut old = ui.take_games();
                        let merged = catalog::merge(&p.name, g, &mut old);
                        ui.prov_list = list;
                        ui.covers_asked.clear();
                        ui.cache_absent.clear();
                        ui.set_games(merged);
                        // The list is up to date; the covers the card lacks
                        // are fetched once nobody looks (below), so the catalog
                        // is not asked for them behind the selection's.
                        if missing.is_empty() && !refreshed.contains(&key) {
                            println!("[display] refresh done: all covers kept");
                            refreshed.push(key);
                        }
                    } else {
                        // Nobody looks: the covers the card lacks are read now.
                        if missing.is_empty() {
                            println!("[display] refresh done: no cover missing");
                            refreshed.push(key);
                        } else {
                            p.covers(list, missing.clone());
                            (hidden_need, hidden_list) = (missing, list);
                            hidden_retry = now + 2_000_000_000;
                        }
                    }
                }
                for (list, id, c) in up.covers {
                    if let Some(k) = &key {
                        cache.post(cache::Job::SaveCover(
                            k.0.clone(),
                            k.1.clone(),
                            id.clone(),
                            c.clone(),
                        ));
                        let e = kept.entry(k.clone()).or_default();
                        if !e.have.contains(&id) {
                            e.have.push(id.clone());
                        }
                    }
                    if shown {
                        ui.set_cover(list, &id, catalog::cover(c));
                    } else {
                        hidden_need.retain(|i| *i != id);
                    }
                }
                if hidden_list != 0 && hidden_need.is_empty() {
                    hidden_list = 0;
                    if let Some(k) = key.clone().filter(|_| !shown) {
                        println!("[display] refresh done: all covers read");
                        refreshed.push(k);
                    }
                }
                for n in up.notes {
                    ui.starting = None;
                    (prov_key, launch_version) = (None, None);
                    ui.toast(n, now);
                }
                if let Some(e) = up.start_failed {
                    if let Some(k) = key.clone().filter(|_| !shown) {
                        println!("[display] refresh done: start failed");
                        refreshed.push(k);
                    }
                    prov = None;
                    ui.prov = Some(catalog::View::local(
                        provproto::ST_ERROR,
                        format!("cannot start it: {}", libnx::abi::err::name(e)),
                    ));
                    prov_retry_at = now + 5_000_000_000;
                }
                if up.stop_failed {
                    if let Some(p) = &prov {
                        println!("[display] {}: STOP failed, ended", p.title);
                        p.kill();
                    }
                }
                if let Some((code, p)) = up.exit.and_then(|c| prov.take().map(|p| (c, p))) {
                    let owned_by = game.as_ref().map(|g| g.owner);
                    if p.pid().is_some() && p.pid() == owned_by {
                        release_game!();
                    }
                    catalog_game_over!(now);
                    println!("[display] {} ended (code {})", p.title, code);
                    if p.name != ui.service {
                        println!("[display] refresh done: exited");
                        refreshed.push((p.user.clone(), p.name.clone()));
                    }
                    // 137: ended by us (STOP unanswered, or another catalog picked).
                    if code != 0 && code != 137 {
                        ui.toast(format!("{} stopped (code {})", p.title, code), now);
                    }
                    // Back after 2 s, then twice longer each time it fails
                    // again within a minute (64 s at most).
                    prov_fails = if now - p.since > 60_000_000_000 {
                        1
                    } else {
                        prov_fails + 1
                    };
                    prov_retry_at = now + (2_000_000_000u64 << (prov_fails - 1).min(5));
                    ui.prov = Some(catalog::View::local(
                        provproto::ST_ERROR,
                        format!("{} stopped (code {}): starting it again", p.title, code),
                    ));
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
                            | Done::Join { .. }
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
                            Ok(false) => {
                                ui.toast("flight mode off".into(), now);
                                if let Some(a) = autojoin.as_mut() {
                                    (a.tries, a.at) = (0, now + 2_000_000_000);
                                }
                            }
                            Err(e) => ui.toast(format!("wifi: {}", libnx::abi::err::name(e)), now),
                        },
                        Done::Scan(r) => {
                            let joined = status::wifi_connected(&data);
                            ui.wifi_scanned(r, joined.as_ref().map(|w| w.raw()));
                        }
                        Done::Join { name, r, auto } => {
                            if let Err(e) = &r {
                                println!(
                                    "[display] wifi: join {}: {}",
                                    name,
                                    libnx::abi::err::name(*e)
                                );
                            }
                            if let Some(t) = ui.wifi_joined(&name, &r, auto) {
                                ui.toast(t, now);
                            }
                            if r.is_ok() {
                                autojoin = None;
                            }
                        }
                        Done::Retry(Ok(t)) => ui.toast(format!("wlan: {}", t), now),
                        Done::Leave(Ok(())) => {
                            autojoin = None;
                            ui.toast("wlan: left the network".into(), now)
                        }
                        Done::Power(e) => {
                            ui.toast(format!("power: {}", libnx::abi::err::name(e)), now)
                        }
                        Done::Retry(Err(e)) | Done::Leave(Err(e)) => {
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
                        (s.slept, s.hud_slept) = (0, 0);
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
            proto::HELPER => {
                let h = if rx.msg.handles() > 0 {
                    ipc::received_handle(0)
                } else {
                    0
                };
                let ok = speed
                    .as_mut()
                    .is_some_and(|s| s.hello(rx.msg.mr[0], rx.pid, h));
                if ok {
                    Msg::new(0, 0, 0)
                } else {
                    if h != 0 {
                        let _ = sys::handle_close(h);
                    }
                    Msg::error(-EPERM)
                }
            }
            proto::PROVIDER => {
                let h = if rx.msg.handles() > 0 {
                    ipc::received_handle(0)
                } else {
                    0
                };
                let said = prov.as_mut().filter(|p| p.pid().is_none()).map(|p| {
                    let ok = p.hello(rx.msg.mr[0], rx.pid, h);
                    if ok {
                        println!(
                            "[display] {}: catalog up (pid {}) {} ms after its pick",
                            p.title,
                            rx.pid,
                            time::now_ns().saturating_sub(p.since) / 1_000_000
                        );
                    }
                    ok
                });
                match said {
                    Some(true) => Msg::new(0, 0, 0),
                    _ => {
                        if h != 0 {
                            let _ = sys::handle_close(h);
                        }
                        Msg::error(-EPERM)
                    }
                }
            }
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

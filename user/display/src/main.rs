//! display: Tegra X1 DC-A screen; window A on a shared image, drawn as
//! the touch UI of `ui` (docs/ui/home.html): header, home tiles, system,
//! network, the USB-C terminal and the lines written to the screen
//! (driver failures, `fbecho`). Redrawn every second and on touches.
//! Panel init is left to the bootloader; only the window is changed. A
//! portrait panel (Switch) is drawn turned.

#![no_std]
#![no_main]

extern crate alloc;

mod screen;
mod stats;
mod term;
mod ui;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use libnx::abi::err::EINVAL;
use libnx::abi::proto::{console as con, cpufreq, display as proto, fan, input, wifi};
use libnx::abi::rights::{DUP, GRANT, MAP, WRITE};
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

// Touch panel range (hekate bdk/input/touch.c): landscape, edges cut.
const TOUCH_EDGE: u64 = 15;
const TOUCH_X_MAX: u64 = 1264;
const TOUCH_Y_MAX: u64 = 704;

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

const R8G8B8A8: u32 = 13;

static INPUT: Service = Service::new("input", 0);
static WIFI: Service = Service::optional("wifi");
static POWER: Service = Service::optional("power");
static FAN: Service = Service::optional("fan");
static CPUFREQ: Service = Service::optional("cpufreq");

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

/// A touch as `input` reports it: code (down, move, up), finger, raw x, y.
type Touch = (u64, u64, u64, u64);

/// Outcome of an action run by the worker.
enum Done {
    Scan(Result<Vec<ScanNet>, i64>),
    Retry(Result<String, i64>),
    Leave(Result<(), i64>),
    Power(i64),
    /// The fan's state after a `SET`.
    Fan(Result<stats::Fan, i64>),
    /// The CPU clock after a `SET`.
    Cpu(Result<stats::Cpu, i64>),
}

struct Worker {
    jobs: Mutex<VecDeque<Action>>,
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
/// fan busy measuring its speed) one at a time, off the drawing loop.
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
        let done = match job {
            Action::Scan => Done::Scan(scan()),
            Action::Retry => Done::Retry(WIFI.call(&Msg::new(wifi::RETRY, 0, 0)).map(|r| {
                let mut buf = [0u8; 256];
                let n = ipc::unpack(&r, &mut buf);
                String::from_utf8_lossy(&buf[..n]).into_owned()
            })),
            Action::Leave => Done::Leave(WIFI.call(&Msg::new(wifi::DISCONNECT, 0, 0)).map(|_| ())),
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
            Action::ClearLogs => continue,
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
    let e = TOUCH_EDGE;
    let x = (x.clamp(e, TOUCH_X_MAX) - e) * sw as u64 / (TOUCH_X_MAX - e);
    let y = (y.clamp(e, TOUCH_Y_MAX) - e) * sh as u64 / (TOUCH_Y_MAX - e);
    (x as usize, y as usize)
}

libnx::entry!(main);

fn main() -> i32 {
    let dc = Mmio::map(drv::dev_slot(0), DC, 0x40000).expect("display: registers");
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
    let touches: Arc<Mutex<VecDeque<Touch>>> = Arc::new(Mutex::new(VecDeque::new()));
    {
        let q = touches.clone();
        thread::spawn(move || loop {
            let mut m = Msg::new(input::READ, 2, 0);
            m.mr[0] = 0;
            m.mr[1] = 1 << input::KIND_TOUCH;
            match INPUT.call(&m) {
                Ok(r) => {
                    q.lock().push_back((r.mr[1], r.mr[2], r.mr[3], r.mr[4]));
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
    let mut term = term::Term::new();
    let (mut tail, mut tail_buf) = (0u64, alloc::vec![0u8; ipc::MAX_BYTES]);
    // Finger followed (the first one down) and where it last was.
    let mut finger: Option<(u64, usize, usize)> = None;
    let (mut next_refresh, mut next_tail, mut blink) = (0, 0, 0);
    let mut dirty = true;
    // A client drawing on the image itself (OPEN) stops the UI.
    let mut owned = false;
    // Handle sent with the previous reply: closed afterwards.
    let mut sent = 0u32;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    loop {
        let now = time::now_ns();
        let mut wake = u64::MAX;
        if !owned {
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
            if dirty {
                ui.draw(&mut scr, &data, &msgs, &term, time::now_ns());
                scr.present();
                dirty = false;
            }
            wake = next_refresh.min(next_tail);
            if terminal {
                wake = wake.min((blink + 1) * BLINK_NS);
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
                let batch: Vec<Touch> = touches.lock().drain(..).collect();
                for (code, id, x, y) in batch {
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
                    let action = ui.touch(code, x, y);
                    if ui.page != page {
                        next_tail = 0; // TERMINAL and LOGS show up to date
                    }
                    match action {
                        Some(Action::ClearLogs) => msgs.clear(),
                        Some(a) => {
                            worker.jobs.lock().push_back(a);
                            worker.wake.notify_one();
                        }
                        None => {}
                    }
                    dirty = true;
                }
            }
            if ev & EV_DONE != 0 {
                let batch: Vec<Done> = worker.done.lock().drain(..).collect();
                let now = time::now_ns();
                for d in batch {
                    if !matches!(d, Done::Fan(_) | Done::Cpu(_)) {
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
        if sent != 0 {
            let _ = sys::handle_close(core::mem::take(&mut sent));
        }
        let reply = match rx.msg.label() {
            proto::OPEN => match sys::handle_dup(scr.fb.handle(), MAP | WRITE | GRANT | DUP, 0) {
                Ok(h2) => {
                    ipc::set_send_handle(0, h2);
                    sent = h2;
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

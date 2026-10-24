//! The speed test: `/bin/speedtest`, a program `display` runs when asked
//! (the Network page's sheet) and talks to (`abi::proto::speed`). It runs
//! as a program of its own because it needs TLS (rustls, std); this side
//! only starts it, sends START and STOP, and keeps the last report. The
//! spawn, the long poll of its status and the commands each have a thread:
//! none of them may hold the UI loop.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use libnx::abi::err::ETIMEDOUT;
use libnx::abi::proto::speed as proto;
use libnx::ipc::{self, Msg};
use libnx::sync::{Condvar, Mutex};
use libnx::{sys, thread, time};

pub use proto::Report;

/// The test's notification bit for `display` (a report, its end).
pub const EV_SPEED: u64 = 16;
const PROGRAM: &str = "/bin/speedtest";
/// How long `STATUS` waits for news, and our call around it.
const POLL_NS: u64 = 1_000_000_000;
const CALL_NS: u64 = 3_000_000_000;

/// What changed since the last `take`.
#[derive(Default)]
pub struct Update {
    pub report: Option<Report>,
    /// The program could not be started (errno), or ended (its code).
    pub start_failed: Option<i64>,
    pub exit: Option<i64>,
    /// `START` was refused (errno).
    pub refused: Option<i64>,
}

enum Cmd {
    Start,
    Stop,
}

struct Shared {
    /// Its endpoint (0 until its hello).
    ep: u32,
    quit: bool,
    cmds: VecDeque<Cmd>,
    up: Update,
}

struct Link {
    sh: Mutex<Shared>,
    wake: Condvar,
    notif: u32,
    /// The process, 0 until `launch` has it.
    proc: AtomicU32,
}

impl Link {
    fn post(&self, f: impl FnOnce(&mut Update)) {
        f(&mut self.sh.lock().up);
        let _ = sys::notify_signal(self.notif, EV_SPEED);
    }

    fn quit(&self) -> bool {
        self.sh.lock().quit
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let ep = self.sh.lock().ep;
        if ep != 0 {
            let _ = sys::handle_close(ep);
        }
        let h = self.proc.load(Ordering::Acquire);
        if h != 0 {
            let _ = sys::handle_close(h);
        }
    }
}

/// The program running.
pub struct Speed {
    key: u64,
    pid: Option<u32>,
    link: Arc<Link>,
}

impl Speed {
    /// Start the program; returns at once (init reads it from the card, a
    /// thread of ours waits for that). Its failure comes back in `Update`.
    pub fn start(notif: u32) -> Result<Self, i64> {
        let mut k = [0u8; 8];
        sys::getrandom(&mut k);
        let key = u64::from_le_bytes(k) >> 1;
        let mut env = libnx::process::current_env();
        env.push(format!("{}={}", proto::KEY_ENV, key));
        let link = Arc::new(Link {
            sh: Mutex::new(Shared {
                ep: 0,
                quit: false,
                cmds: VecDeque::new(),
                up: Update::default(),
            }),
            wake: Condvar::new(),
            notif,
            proc: AtomicU32::new(0),
        });
        let threads = [
            thread::spawn({
                let l = link.clone();
                move || watch(&l)
            }),
            thread::spawn({
                let l = link.clone();
                move || run_cmds(&l)
            }),
            thread::spawn({
                let l = link.clone();
                move || launch(&l, &env)
            }),
        ];
        if threads.iter().any(|t| t.is_err()) {
            link.sh.lock().quit = true;
            link.wake.notify_all();
            return Err(-libnx::abi::err::EAGAIN);
        }
        Ok(Self {
            key,
            pid: None,
            link,
        })
    }

    /// Its hello (`display::HELPER`): kept if it has the key and is first.
    pub fn hello(&mut self, key: u64, pid: u32, ep: u32) -> bool {
        if key != self.key || self.pid.is_some() || ep == 0 {
            return false;
        }
        self.pid = Some(pid);
        self.link.sh.lock().ep = ep;
        self.link.wake.notify_all();
        true
    }

    pub fn take(&self) -> Update {
        core::mem::take(&mut self.link.sh.lock().up)
    }

    fn command(&self, c: Cmd) {
        self.link.sh.lock().cmds.push_back(c);
        self.link.wake.notify_all();
    }

    /// Begin a test (the program's default server, connections, time).
    pub fn begin(&self) {
        self.command(Cmd::Start);
    }

    /// End the test running, keeping what it measured.
    pub fn end(&self) {
        self.command(Cmd::Stop);
    }

    /// End the program.
    pub fn kill(&self) {
        self.link.sh.lock().quit = true;
        self.link.wake.notify_all();
        let h = self.link.proc.load(Ordering::Acquire);
        if h != 0 {
            let _ = sys::process_kill(h);
        }
    }
}

impl Drop for Speed {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawn the program, then wait for its end.
fn launch(l: &Arc<Link>, env: &[alloc::string::String]) {
    let spawned = if l.quit() {
        Err(-libnx::abi::err::EINTR)
    } else {
        libnx::process::spawn_with(&libnx::process::Strings::new(&[PROGRAM], env).kill_on_close())
    };
    let h = match spawned {
        Ok(h) => h,
        Err(e) => {
            l.sh.lock().quit = true;
            l.wake.notify_all();
            l.post(|u| u.start_failed = Some(e));
            return;
        }
    };
    l.proc.store(h, Ordering::Release);
    // Ended meanwhile: `kill` did not see the process.
    if l.quit() {
        let _ = sys::process_kill(h);
    }
    let code = libnx::process::wait(h);
    l.sh.lock().quit = true;
    l.wake.notify_all();
    l.post(|u| u.exit = Some(code));
}

/// Its endpoint once it said hello; `None` once it is gone.
fn endpoint(l: &Link) -> Option<u32> {
    let mut s = l.sh.lock();
    loop {
        if s.quit {
            return None;
        }
        if s.ep != 0 {
            return Some(s.ep);
        }
        s = l.wake.wait(s);
    }
}

/// The report, waiting for news.
fn watch(l: &Arc<Link>) {
    let Some(ep) = endpoint(l) else {
        return;
    };
    let mut known = 0u64;
    let mut buf = alloc::vec![0u8; ipc::MAX_BYTES];
    while !l.quit() {
        let m = Msg::from_words(proto::STATUS, &[known, POLL_NS]);
        let r = match ipc::call_timeout(ep, &m, POLL_NS + CALL_NS).and_then(ipc::checked) {
            Ok(r) => r,
            Err(e) if e == -ETIMEDOUT => continue,
            Err(_) => {
                time::sleep_ms(300);
                continue;
            }
        };
        let n = ipc::unpack_at(&r, 1, &mut buf);
        if let Some(rep) = Report::decode(&buf[..n]) {
            if rep.version != known {
                known = rep.version;
                l.post(|u| u.report = Some(rep));
            }
        }
    }
}

fn run_cmds(l: &Arc<Link>) {
    let Some(ep) = endpoint(l) else {
        return;
    };
    loop {
        let cmd = {
            let mut s = l.sh.lock();
            loop {
                if s.quit {
                    return;
                }
                if let Some(c) = s.cmds.pop_front() {
                    break c;
                }
                s = l.wake.wait(s);
            }
        };
        let m = match cmd {
            Cmd::Start => Msg::from_words(proto::START, &[0, 0, 0]),
            Cmd::Stop => Msg::new(proto::STOP, 0, 0),
        };
        if let Err(e) = ipc::call_timeout(ep, &m, CALL_NS).and_then(ipc::checked) {
            l.post(|u| u.refused = Some(e));
        }
    }
}

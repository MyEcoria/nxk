//! Debug channel on the console stream (`abi::proto::dbg`): requests
//! from the PC are taken out of the shell's input, replies are put in
//! the output. A screen capture is encoded lazily, one reply at a time,
//! as the IN endpoint frees up: no image-sized buffer here.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libnx::abi::err::{EINVAL, ENOSYS, EPIPE};
use libnx::abi::prot;
use libnx::abi::proto::{dbg, display, input};
use libnx::ipc::Msg;
use libnx::mem::SharedMemory;
use libnx::ns::Service;

static DISPLAY: Service = Service::optional("display");
static INPUT: Service = Service::optional("input");

/// Longest request, in base64 characters; a longer frame is dropped.
const MAX_FRAME: usize = 64;
/// Capture bytes per reply.
const CHUNK: usize = 1200;
/// Longest literal run, in pixels.
const MAX_LITERAL: usize = 128;
const CALL_NS: u64 = 200_000_000;
/// Landscape size of the touch UI (the touch panel's 1280x720).
const SCREEN_W: u64 = 1280;
const SCREEN_H: u64 = 720;
const ESC: u8 = 0x1B;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Pass,
    /// ESC held back: a frame may follow.
    Esc,
    /// ESC `_` held back.
    Apc,
    Body,
    BodyEsc,
}

/// Screen capture in progress: the image on display, mapped read-only.
struct Capture {
    id: u8,
    img: SharedMemory,
    pixels: usize,
    pos: usize,
    head: [u8; 6],
    head_sent: bool,
}

pub struct Dbg {
    state: State,
    frame: Vec<u8>,
    overflow: bool,
    requests: Vec<Vec<u8>>,
    /// Replies as frames, not yet sent.
    out: VecDeque<u8>,
    job: Option<Capture>,
}

fn b64_value(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

fn b64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let (mut acc, mut bits) = (0u32, 0);
    let mut out = Vec::new();
    for &c in s {
        if c == b'=' {
            break;
        }
        acc = (acc << 6) | b64_value(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn b64_encode(data: &[u8], out: &mut VecDeque<u8>) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for c in data.chunks(3) {
        let v = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push_back(T[(v >> 18) as usize & 63]);
        out.push_back(T[(v >> 12) as usize & 63]);
        out.push_back(if c.len() > 1 {
            T[(v >> 6) as usize & 63]
        } else {
            b'='
        });
        out.push_back(if c.len() > 2 {
            T[v as usize & 63]
        } else {
            b'='
        });
    }
}

impl Capture {
    /// The next pixels as tokens, about `CHUNK` bytes; true at the end.
    fn encode(&mut self, out: &mut Vec<u8>) -> bool {
        if !self.head_sent {
            out.extend_from_slice(&self.head);
            self.head_sent = true;
        }
        let px =
            unsafe { core::slice::from_raw_parts(self.img.as_ptr() as *const u32, self.pixels) };
        while out.len() < CHUNK && self.pos < self.pixels {
            let first = px[self.pos];
            let run = px[self.pos..]
                .iter()
                .take(0x8000)
                .take_while(|&&p| p == first)
                .count();
            if run >= 2 {
                out.extend_from_slice(&((run as u16 - 1) | 0x8000).to_le_bytes());
                out.extend_from_slice(&first.to_le_bytes());
                self.pos += run;
                continue;
            }
            // Literal: up to the next pair of equal pixels.
            let rest = &px[self.pos..];
            let limit = rest.len().min(MAX_LITERAL);
            let n = (1..limit)
                .find(|&i| rest.get(i + 1) == Some(&rest[i]))
                .unwrap_or(limit);
            out.extend_from_slice(&(n as u16 - 1).to_le_bytes());
            for p in &rest[..n] {
                out.extend_from_slice(&p.to_le_bytes());
            }
            self.pos += n;
        }
        self.pos >= self.pixels
    }
}

impl Dbg {
    pub fn new() -> Self {
        Self {
            state: State::Pass,
            frame: Vec::new(),
            overflow: false,
            requests: Vec::new(),
            out: VecDeque::new(),
            job: None,
        }
    }

    /// Bytes from the PC: those that belong to the shell come back, the
    /// frames are kept as requests.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let mut pass = Vec::with_capacity(data.len() + 2);
        for &b in data {
            self.byte(b, &mut pass);
        }
        pass
    }

    fn byte(&mut self, b: u8, pass: &mut Vec<u8>) {
        match self.state {
            State::Pass if b == ESC => self.state = State::Esc,
            State::Pass => pass.push(b),
            State::Esc if b == dbg::FRAME_START[1] => self.state = State::Apc,
            State::Apc if b == dbg::FRAME_START[2] => {
                self.state = State::Body;
                self.frame.clear();
                self.overflow = false;
            }
            State::Esc | State::Apc => {
                // Not ours: what was held goes to the shell, then `b`.
                pass.push(ESC);
                if self.state == State::Apc {
                    pass.push(dbg::FRAME_START[1]);
                }
                self.state = State::Pass;
                self.byte(b, pass);
            }
            State::Body if b == ESC => self.state = State::BodyEsc,
            State::Body => {
                if self.frame.len() < MAX_FRAME {
                    self.frame.push(b);
                } else {
                    self.overflow = true;
                }
            }
            State::BodyEsc if b == dbg::FRAME_END[1] => {
                self.state = State::Pass;
                if !self.overflow {
                    if let Some(req) = b64_decode(&self.frame) {
                        self.requests.push(req);
                    }
                }
            }
            // A broken frame: dropped, `b` read again as plain input.
            State::BodyEsc => {
                self.state = State::Pass;
                self.byte(b, pass);
            }
        }
    }

    /// Frames in flight or to come.
    pub fn pending(&self) -> bool {
        !self.out.is_empty() || self.job.is_some()
    }

    /// The host is gone: nothing more goes out.
    pub fn reset(&mut self) {
        self.out.clear();
        self.job = None;
        self.requests.clear();
        self.state = State::Pass;
    }

    fn reply(&mut self, id: u8, status: u8, data: &[u8]) {
        let mut p = Vec::with_capacity(data.len() + 2);
        p.push(id);
        p.push(status);
        p.extend_from_slice(data);
        self.out.extend(dbg::FRAME_START);
        b64_encode(&p, &mut self.out);
        self.out.extend(dbg::FRAME_END);
    }

    fn error(&mut self, id: u8, errno: i64) {
        self.reply(id, dbg::ERROR, &(errno.unsigned_abs() as u16).to_le_bytes());
    }

    /// Answer the requests received.
    pub fn run(&mut self) {
        for req in core::mem::take(&mut self.requests) {
            let [id, op, args @ ..] = &req[..] else {
                continue;
            };
            let r = match (*op, args) {
                (dbg::PING, _) => {
                    self.reply(*id, dbg::DONE, &[dbg::VERSION]);
                    Ok(())
                }
                (dbg::SCREEN, _) => self.start_capture(*id),
                (dbg::TOUCH, [action, finger, x0, x1, y0, y1, ..]) => touch(
                    *action,
                    *finger,
                    u16::from_le_bytes([*x0, *x1]),
                    u16::from_le_bytes([*y0, *y1]),
                ),
                (dbg::BUTTON, [code, value, ..]) => button(*code, *value),
                _ => Err(-EINVAL),
            };
            match r {
                Ok(()) if *op == dbg::TOUCH || *op == dbg::BUTTON => {
                    self.reply(*id, dbg::DONE, &[])
                }
                Ok(()) => {}
                Err(e) => self.error(*id, e),
            }
        }
    }

    fn start_capture(&mut self, id: u8) -> Result<(), i64> {
        let r = call(&DISPLAY, &Msg::new(display::SNAPSHOT, 0, 0))?;
        let (w, h, stride) = (r.mr[0] as usize, r.mr[1] as usize, r.mr[2] as usize);
        let handle = libnx::ipc::received_handle(0);
        if handle == 0 {
            return Err(-ENOSYS);
        }
        let img = SharedMemory::map(handle, h * stride, prot::READ)?;
        let mut head = [0u8; 6];
        head[..2].copy_from_slice(&(w as u16).to_le_bytes());
        head[2..4].copy_from_slice(&(h as u16).to_le_bytes());
        head[4] = r.mr[3] as u8;
        head[5] = (r.mr[4] as u8 & 0xF) | (r.mr[5] as u8) << 4;
        self.job = Some(Capture {
            id,
            img,
            pixels: w * h,
            pos: 0,
            head,
            head_sent: false,
        });
        Ok(())
    }

    /// Next bytes for the host, at most `buf.len()`.
    pub fn fill(&mut self, buf: &mut [u8]) -> usize {
        let mut n = 0;
        loop {
            while n < buf.len() {
                match self.out.pop_front() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n == buf.len() {
                return n;
            }
            let Some(job) = self.job.as_mut() else {
                return n;
            };
            let mut data = Vec::with_capacity(CHUNK + MAX_LITERAL * 4 + 8);
            let done = job.encode(&mut data);
            let id = job.id;
            if done {
                self.job = None;
            }
            self.reply(id, if done { dbg::DONE } else { dbg::MORE }, &data);
        }
    }
}

/// Landscape pixel to the touch panel's raw coordinate (the middle of
/// the pixel, so that `display` maps it back to the same one).
fn raw(p: u64, size: u64, max: u64) -> u64 {
    let (e, p) = (input::TOUCH_EDGE, p.min(size - 1));
    e + (2 * p + 1) * (max - e) / (2 * size)
}

/// Call a service; once more if it was restarted (its handle is dead).
fn call(svc: &Service, m: &Msg) -> Result<Msg, i64> {
    match svc.call_timeout(m, CALL_NS) {
        Err(e) if e == -EPIPE => svc.call_timeout(m, CALL_NS),
        r => r,
    }
}

fn inject(e: [u64; 5]) -> Result<(), i64> {
    let mut m = Msg::new(input::INJECT, 5, 0);
    m.mr[..5].copy_from_slice(&e);
    call(&INPUT, &m).map(|_| ())
}

fn touch(action: u8, finger: u8, x: u16, y: u16) -> Result<(), i64> {
    if !matches!(
        action as u64,
        input::TOUCH_DOWN | input::TOUCH_MOVE | input::TOUCH_UP
    ) {
        return Err(-EINVAL);
    }
    inject([
        input::KIND_TOUCH,
        action as u64,
        finger as u64,
        raw(x as u64, SCREEN_W, input::TOUCH_X_MAX),
        raw(y as u64, SCREEN_H, input::TOUCH_Y_MAX),
    ])
}

fn button(code: u8, value: u8) -> Result<(), i64> {
    if !(input::BTN_A..=input::BTN_POWER_LONG).contains(&(code as u64)) {
        return Err(-EINVAL);
    }
    inject([input::KIND_BUTTON, code as u64, (value != 0) as u64, 0, 0])
}

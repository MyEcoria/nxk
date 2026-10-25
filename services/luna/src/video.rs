//! The display channel: the capabilities answered, each frame acknowledged,
//! a key frame asked for after a gap or a decoding error. Frames go to a
//! decoding thread through a small queue (the stream thread never waits for
//! it); the newest decoded picture waits in `Shared` for the presenter.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use lunacore::proto::display::ClientMessage;
use lunacore::stream::display::{nal_types, Channel, DisplayConfig, EncodedFrame};
use lunacore::stream::framing::Frame;
use nxh264::{Decoder, Picture};
use nxk_abi::proto::provider::Stats;

/// Frames waiting for the decoder: more than this are dropped, and the
/// next key frame resumes. A run of busy pictures at 60 Hz (a decoder a
/// little slower than the stream) fills it slowly; dropping costs a
/// freeze until the next key frame, waiting only some delay.
const QUEUE: usize = 10;
/// Threads the decoder uses for a picture (slices, then the filter): the
/// four slices of a picture on the four cores, below the other threads.
const DECODER_THREADS: usize = 4;
/// The decoder runs below the stream and audio threads (default 16).
const DECODER_PRIORITY: u8 = 20;

/// A figure kept over a report period: how many, their sum and their largest (µs).
#[derive(Default)]
struct Spans {
    n: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}

impl Spans {
    fn add(&self, d: Duration) {
        let us = d.as_micros() as u64;
        self.n.fetch_add(1, Relaxed);
        self.sum.fetch_add(us, Relaxed);
        self.max.fetch_max(us, Relaxed);
    }

    /// "avg/max ms" since the last call, or "-" when nothing was added.
    fn take(&self) -> String {
        let (n, sum, max) = (
            self.n.swap(0, Relaxed),
            self.sum.swap(0, Relaxed),
            self.max.swap(0, Relaxed),
        );
        if n == 0 {
            return "-".into();
        }
        format!(
            "{:.1}/{:.1}",
            sum as f64 / n as f64 / 1000.0,
            max as f64 / 1000.0
        )
    }
}

/// What the decoding thread hands to the rest.
#[derive(Default)]
pub struct Shared {
    /// The newest picture and when its frame was complete.
    latest: Mutex<Option<(Instant, Arc<Picture>)>>,
    ready: Condvar,
    /// Frames in the decoder's queue now, and the most in the report period.
    queued: AtomicUsize,
    queue_max: AtomicUsize,
    /// Where the time goes between the network and the panel: a frame
    /// complete to its decoded picture, to the picture on the panel, the
    /// show call itself, and a gamepad event read to its datagram sent.
    decode_wait: Spans,
    on_screen: Spans,
    show: Spans,
    input: Spans,
    /// The decoder lost its references: a key frame is needed.
    want_key: AtomicBool,
    decoded: AtomicU64,
    errors: AtomicU64,
    dropped: AtomicU64,
    decode_us: AtomicU64,
    decode_max_us: AtomicU64,
    shown: AtomicU64,
    /// The figures the overlay shows, renewed by the stream thread.
    stats: Mutex<Stats>,
}

impl Shared {
    pub fn publish(&self, s: Stats) {
        if let Ok(mut l) = self.stats.lock() {
            *l = s;
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// The newest picture not shown yet (and when its frame was complete),
    /// waiting at most `wait` for one.
    pub fn wait_latest(&self, wait: Duration) -> Option<(Instant, Arc<Picture>)> {
        let mut l = self.latest.lock().ok()?;
        if l.is_none() {
            l = self.ready.wait_timeout(l, wait).ok()?.0;
        }
        let p = l.take();
        if p.is_some() {
            self.shown.fetch_add(1, Relaxed);
        }
        p
    }

    /// Pictures put on the panel so far.
    pub fn shown(&self) -> u64 {
        self.shown.load(Relaxed)
    }

    /// A picture shown: `arrived` is when its frame was complete, `show`
    /// how long the show call took.
    pub fn on_screen(&self, arrived: Instant, show: Duration) {
        self.on_screen.add(arrived.elapsed());
        self.show.add(show);
    }

    /// A gamepad event sent, `read` after it was read from the screen's input.
    pub fn input_sent(&self, read: Duration) {
        self.input.add(read);
    }
}

fn lower_priority() {
    let _ = std::os::nxk::thread::set_priority(DECODER_PRIORITY);
}

fn decode_loop(rx: std::sync::mpsc::Receiver<(Instant, EncodedFrame)>, shared: Arc<Shared>) {
    lower_priority();
    let mut dec = Decoder::new();
    dec.threads = DECODER_THREADS;
    dec.worker_hook = Some(lower_priority);
    let mut last_error = String::new();
    while let Ok((arrived, f)) = rx.recv() {
        shared.queued.fetch_sub(1, Relaxed);
        let t = Instant::now();
        match dec.decode(&f.data) {
            Ok(Some(p)) => {
                let us = t.elapsed().as_micros() as u64;
                shared.decoded.fetch_add(1, Relaxed);
                shared.decode_us.fetch_add(us, Relaxed);
                shared.decode_max_us.fetch_max(us, Relaxed);
                shared.decode_wait.add(arrived.elapsed());
                if let Ok(mut l) = shared.latest.lock() {
                    *l = Some((arrived, p));
                }
                shared.ready.notify_one();
            }
            Ok(None) => {}
            Err(e) => {
                shared.errors.fetch_add(1, Relaxed);
                shared.want_key.store(true, Relaxed);
                let text = format!("{:?}", e);
                if text != last_error {
                    println!("luna: video decoder: {}", text);
                    last_error = text;
                }
            }
        }
    }
}

pub struct Video {
    channel: Channel,
    tx: Option<SyncSender<(Instant, EncodedFrame)>>,
    shared: Arc<Shared>,
    first: bool,
    shown: u64,
    last_line: (u64, u64),
    /// The stream's picture size, once a frame said it.
    size: Option<(u16, u16)>,
}

impl Video {
    pub fn new(shared: Arc<Shared>, config: DisplayConfig) -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        let s2 = shared.clone();
        let tx = match std::thread::Builder::new()
            .name("h264".into())
            .spawn(move || decode_loop(rx, s2))
        {
            Ok(_) => Some(tx),
            Err(e) => {
                println!("luna: video decoder thread: {}", e);
                None
            }
        };
        Self {
            channel: Channel::new(config, Instant::now()),
            tx,
            shared,
            first: true,
            shown: 0,
            last_line: (0, 0),
            size: None,
        }
    }

    /// A frame of the display channel: what to send back.
    pub fn on_frame(&mut self, frame: Frame) -> Vec<ClientMessage> {
        let step = self.channel.on_frame(frame, Instant::now());
        if let Some(f) = step.video {
            self.on_video(f);
        }
        step.replies
    }

    /// Pictures received and lost so far, and the stream's size.
    pub fn counts(&self) -> (u64, u64, Option<(u16, u16)>) {
        let s = &self.channel.stats;
        (s.frames, s.missing, self.size)
    }

    fn on_video(&mut self, f: EncodedFrame) {
        if let Some(r) = &f.stream_resolution {
            self.size = Some((
                r.width.min(u16::MAX as u32) as u16,
                r.height.min(u16::MAX as u32) as u16,
            ));
        }
        if self.first {
            self.first = false;
            println!(
                "luna: first video frame {} bytes, stream {:?}, NAL types {:?}",
                f.data.len(),
                f.stream_resolution.map(|r| (r.width, r.height)),
                nal_types(&f.data)
            );
        }
        let Some(tx) = &self.tx else {
            return;
        };
        let depth = self.shared.queued.fetch_add(1, Relaxed) + 1;
        self.shared.queue_max.fetch_max(depth, Relaxed);
        match tx.try_send((Instant::now(), f)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.shared.queued.fetch_sub(1, Relaxed);
                // A dropped frame breaks the references: start again from a key frame.
                self.shared.dropped.fetch_add(1, Relaxed);
                self.shared.want_key.store(true, Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.queued.fetch_sub(1, Relaxed);
                self.tx = None;
            }
        }
    }

    /// `ready` when the server stayed silent, and a key frame when the
    /// decoder asked for one.
    pub fn on_tick(&mut self) -> Vec<ClientMessage> {
        let now = Instant::now();
        let mut out = self.channel.on_tick(now);
        if self.shared.want_key.load(Relaxed) {
            if let Some(m) = self.channel.keyframe_request(now) {
                self.shared.want_key.store(false, Relaxed);
                out.push(m);
            }
        }
        out
    }

    /// One line on what came and what became of it, when something changed.
    pub fn report(&mut self) -> Option<String> {
        let s = &self.channel.stats;
        let sh = &self.shared;
        let (decoded, shown) = (sh.decoded.load(Relaxed), sh.shown.load(Relaxed));
        if (s.frames, shown) == self.last_line && s.rejected == 0 {
            return None;
        }
        self.last_line = (s.frames, shown);
        let _ = self.shown;
        let n = decoded.max(1);
        Some(format!(
            "luna: video {} frames {} KB, {} key, {} missing, {} stale, {} rejected, {} key frames asked; \
             decoded {} ({:.1} ms a frame, {:.1} at most), {} errors, {} dropped, {} shown; \
             ms avg/max: complete to decoded {}, to the panel {}, show {}, gamepad to socket {}, queue {} at most",
            s.frames,
            s.bytes / 1024,
            s.key_frames,
            s.missing,
            s.stale,
            s.rejected,
            s.keyframe_requests,
            decoded,
            sh.decode_us.load(Relaxed) as f64 / n as f64 / 1000.0,
            sh.decode_max_us.swap(0, Relaxed) as f64 / 1000.0,
            sh.errors.load(Relaxed),
            sh.dropped.load(Relaxed),
            shown,
            sh.decode_wait.take(),
            sh.on_screen.take(),
            sh.show.take(),
            sh.input.take(),
            sh.queue_max.swap(0, Relaxed)
        ))
    }
}

//! The display channel: the capabilities answered, each frame acknowledged,
//! a key frame asked for after a gap or a decoding error. Frames go to a
//! decoding thread through a small queue (the stream thread never waits for
//! it); the newest decoded picture waits in `Shared` for the presenter.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lunacore::proto::display::ClientMessage;
use lunacore::stream::display::{nal_types, Channel, DisplayConfig, EncodedFrame};
use lunacore::stream::framing::Frame;
use nxh264::{Decoder, Picture};

/// Frames waiting for the decoder: more than this are dropped, and the
/// next key frame resumes.
const QUEUE: usize = 3;
/// Threads the decoder uses for a picture (slices, then the filter).
const DECODER_THREADS: usize = 3;
/// The decoder runs below the stream and audio threads (default 16).
const DECODER_PRIORITY: u8 = 20;

/// What the decoding thread hands to the rest.
#[derive(Default)]
pub struct Shared {
    latest: Mutex<Option<Arc<Picture>>>,
    /// The decoder lost its references: a key frame is needed.
    want_key: AtomicBool,
    decoded: AtomicU64,
    errors: AtomicU64,
    dropped: AtomicU64,
    decode_us: AtomicU64,
    decode_max_us: AtomicU64,
    shown: AtomicU64,
}

impl Shared {
    /// The newest picture not shown yet.
    pub fn take_latest(&self) -> Option<Arc<Picture>> {
        let p = self.latest.lock().ok()?.take();
        if p.is_some() {
            self.shown.fetch_add(1, Relaxed);
        }
        p
    }
}

fn lower_priority() {
    let _ = std::os::nxk::thread::set_priority(DECODER_PRIORITY);
}

fn decode_loop(rx: std::sync::mpsc::Receiver<EncodedFrame>, shared: Arc<Shared>) {
    lower_priority();
    let mut dec = Decoder::new();
    dec.threads = DECODER_THREADS;
    dec.worker_hook = Some(lower_priority);
    let mut last_error = String::new();
    while let Ok(f) = rx.recv() {
        let t = Instant::now();
        match dec.decode(&f.data) {
            Ok(Some(p)) => {
                let us = t.elapsed().as_micros() as u64;
                shared.decoded.fetch_add(1, Relaxed);
                shared.decode_us.fetch_add(us, Relaxed);
                shared.decode_max_us.fetch_max(us, Relaxed);
                if let Ok(mut l) = shared.latest.lock() {
                    *l = Some(p);
                }
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
    tx: Option<SyncSender<EncodedFrame>>,
    shared: Arc<Shared>,
    first: bool,
    shown: u64,
    last_line: (u64, u64),
}

impl Video {
    pub fn new(shared: Arc<Shared>) -> Self {
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
            channel: Channel::new(DisplayConfig::default(), Instant::now()),
            tx,
            shared,
            first: true,
            shown: 0,
            last_line: (0, 0),
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

    fn on_video(&mut self, f: EncodedFrame) {
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
        match tx.try_send(f) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // A dropped frame breaks the references: start again from a key frame.
                self.shared.dropped.fetch_add(1, Relaxed);
                self.shared.want_key.store(true, Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => self.tx = None,
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
             decoded {} ({:.1} ms a frame, {:.1} at most), {} errors, {} dropped, {} shown",
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
            shown
        ))
    }
}

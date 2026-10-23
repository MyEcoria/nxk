//! Plays the audio channel on a thread of its own: packets put back in
//! order (`lunacore::stream::playout`), decoded by `nxopus`, resampled a
//! few hundred ppm off 1 to follow the server's clock, and written to the
//! audio service's ring, kept `TARGET` ahead. A packet still missing when
//! the ring runs low is concealed. Without an audio service, or with one
//! that does not play at 48 kHz (the emulator's), a clock stands in for
//! the speakers, so the rest still runs.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lunacore::stream::playout::{Arrivals, Drift, Jitter, Next, Payload, Resampler};
use nxgfx::audio::Stream;

const RATE: f64 = 48_000.0;
const FRAME: usize = 960;
const RING_FRAMES: usize = 8192;
/// Frames kept in the ring (50 ms).
const TARGET: u32 = 2400;
/// Below this the next packet is given up for lost: the service takes a
/// period of 512 frames at a time, and a short take is an underrun.
const DEADLINE: u32 = 768;
/// Packets kept waiting at most; a burst past it is dropped.
const MAX_QUEUED: usize = 8;
/// Buffered audio this far past the set point (smoothed) is dropped a
/// frame at a time: resampling would take minutes to absorb it.
const MAX_EXCESS: f64 = 1920.0;
const TICK: Duration = Duration::from_millis(3);
/// The speakers' pace is checked once this long after the first write.
const RATE_CHECK: Duration = Duration::from_secs(4);
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// The line is renewed when something goes wrong, else this often: it
/// reaches the LOGS page each time.
const HEARTBEAT: Duration = Duration::from_secs(60);

/// Where the PCM goes.
trait Sink {
    /// Frames written and not yet played.
    fn queued(&mut self) -> u32;
    fn write(&mut self, pcm: &[i16]);
    fn underruns(&self) -> u32;
    fn name(&self) -> &'static str;
}

impl Sink for Stream {
    fn queued(&mut self) -> u32 {
        Stream::queued(self)
    }

    fn write(&mut self, pcm: &[i16]) {
        Stream::write(self, pcm);
    }

    fn underruns(&self) -> u32 {
        Stream::underruns(self)
    }

    fn name(&self) -> &'static str {
        "speakers"
    }
}

/// The speakers' pace by the wall clock.
struct Clock {
    base: Instant,
    written: u64,
    underruns: u32,
}

impl Sink for Clock {
    fn queued(&mut self) -> u32 {
        let played = (self.base.elapsed().as_secs_f64() * RATE) as u64;
        if played > self.written {
            if self.written > 0 {
                self.underruns += 1;
            }
            // Played silence is not owed: restart the count from now.
            self.base = Instant::now() - Duration::from_secs_f64(self.written as f64 / RATE);
            return 0;
        }
        (self.written - played) as u32
    }

    fn write(&mut self, pcm: &[i16]) {
        self.written += (pcm.len() / 2) as u64;
    }

    fn underruns(&self) -> u32 {
        self.underruns
    }

    fn name(&self) -> &'static str {
        "clock"
    }
}

struct In {
    index: Option<u64>,
    payload: Payload,
    at: Instant,
}

/// The playing thread; dropping it stops it.
pub struct Player {
    tx: Option<Sender<In>>,
    report: Arc<Mutex<String>>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    pub fn start() -> Self {
        let (tx, rx) = channel();
        let report = Arc::new(Mutex::new(String::from("luna: audio not started")));
        let r2 = report.clone();
        let thread = std::thread::spawn(move || run(rx, r2));
        Self {
            tx: Some(tx),
            report,
            thread: Some(thread),
        }
    }

    pub fn packet(&self, index: Option<u64>, payload: Payload) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(In {
                index,
                payload,
                at: Instant::now(),
            });
        }
    }

    pub fn report(&self) -> String {
        self.report.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct Counts {
    packets: u64,
    silent: u64,
    errors: u64,
    decode_ns: u64,
    decode_max_ns: u64,
}

struct State {
    jitter: Jitter,
    arrivals: Arrivals,
    drift: Drift,
    resampler: Resampler,
    decoder: nxopus::Decoder,
    pcm: Vec<f32>,
    out: Vec<f32>,
    out16: Vec<i16>,
    counts: Counts,
    started: Instant,
    primed: bool,
    ratio: f64,
    /// Frames written, and when the first was.
    written: u64,
    first_write: Option<Instant>,
}

fn run(rx: Receiver<In>, report: Arc<Mutex<String>>) {
    let mut sink: Box<dyn Sink> = match Stream::open(RING_FRAMES) {
        Ok(s) => Box::new(s),
        Err(e) => {
            println!("luna: no audio service ({:?}): audio played to a clock", e);
            Box::new(Clock {
                base: Instant::now(),
                written: 0,
                underruns: 0,
            })
        }
    };
    let mut st = State {
        jitter: Jitter::default(),
        arrivals: Arrivals::new((FRAME as f64 / RATE * 1e6) as i64),
        drift: Drift::new((TARGET as usize + FRAME / 2) as f64),
        resampler: Resampler::new(),
        decoder: nxopus::Decoder::new(),
        pcm: vec![0.0; 2 * nxopus::MAX_FRAME],
        out: Vec::with_capacity(2 * RING_FRAMES),
        out16: Vec::with_capacity(2 * RING_FRAMES),
        counts: Counts::default(),
        started: Instant::now(),
        primed: false,
        ratio: 1.0,
        written: 0,
        first_write: None,
    };
    let (mut last_tick, mut last_report) = (Instant::now(), Instant::now());
    let (mut shown, mut shown_at) = (None, Instant::now());
    loop {
        match rx.recv_timeout(TICK) {
            Ok(p) => st.take(p),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(p) = rx.try_recv() {
            st.take(p);
        }
        let now = Instant::now();
        if let Some(rate) = st.wrong_rate(sink.as_mut()) {
            println!(
                "luna: the speakers play at {:.0} Hz: audio played to a clock",
                rate
            );
            sink = Box::new(Clock {
                base: Instant::now(),
                written: 0,
                underruns: 0,
            });
        }
        st.feed(sink.as_mut(), (now - last_tick).as_secs_f64());
        last_tick = now;
        if now - last_report >= REPORT_EVERY {
            last_report = now;
            let events = st.events(sink.as_ref());
            if shown != Some(events) || now - shown_at >= HEARTBEAT {
                shown = Some(events);
                shown_at = now;
                if let Ok(mut r) = report.lock() {
                    *r = st.report(sink.as_ref());
                }
            }
        }
    }
}

impl State {
    fn take(&mut self, p: In) {
        if let Some(i) = p.index {
            self.arrivals
                .arrived((p.at - self.started).as_micros() as i64, i);
        }
        match &p.payload {
            Payload::Silent => self.counts.silent += 1,
            Payload::Opus(_) => self.counts.packets += 1,
        }
        self.jitter.push(p.index, p.payload);
        self.jitter.trim(MAX_QUEUED);
    }

    /// Once, `RATE_CHECK` after the first write: the rate the sink took
    /// frames at, if it is not 48 kHz within 10%.
    fn wrong_rate(&mut self, sink: &mut dyn Sink) -> Option<f64> {
        let first = self.first_write?;
        if first.elapsed() < RATE_CHECK {
            return None;
        }
        self.first_write = None;
        let rate = (self.written - sink.queued() as u64) as f64 / first.elapsed().as_secs_f64();
        ((rate - RATE).abs() > 0.1 * RATE).then_some(rate)
    }

    /// Tops the ring up to `TARGET`, decoding or concealing frames as the
    /// resampler runs out.
    fn feed(&mut self, sink: &mut dyn Sink, dt: f64) {
        if !self.primed {
            // Start with two frames in hand, so the first wait is covered.
            if self.jitter.queued() < 2 {
                return;
            }
            self.primed = true;
        }
        let queued = sink.queued();
        let buffered =
            queued as f64 + self.resampler.pending() + (self.jitter.queued() * FRAME) as f64;
        self.ratio = self.drift.update(buffered, dt);
        if self.drift.excess() > MAX_EXCESS && self.jitter.queued() > 0 {
            self.jitter.trim(self.jitter.queued() - 1);
            self.drift.dropped(FRAME as f64);
        }
        let mut queued = queued;
        while queued < TARGET {
            self.out.clear();
            let n = self
                .resampler
                .pull((TARGET - queued) as usize, self.ratio, &mut self.out);
            if n > 0 {
                self.out16.clear();
                self.out16.extend(
                    self.out
                        .iter()
                        .map(|&v| (v * 32768.0).clamp(-32768.0, 32767.0).round() as i16),
                );
                sink.write(&self.out16);
                if self.written == 0 {
                    self.first_write = Some(Instant::now());
                }
                self.written += n as u64;
                queued += n as u32;
                continue;
            }
            match self.jitter.pop(queued < DEADLINE) {
                Next::Opus(b) => self.decode(&b),
                Next::Silent => self.resampler.push(&[0.0; 2 * FRAME]),
                Next::Conceal => {
                    let n = self.decoder.conceal_float(&mut self.pcm);
                    self.resampler.push(&self.pcm[..2 * n]);
                }
                Next::Wait => {
                    if self.jitter.queued() == 0 && queued == 0 {
                        self.primed = false;
                    }
                    break;
                }
            }
        }
    }

    fn decode(&mut self, opus: &[u8]) {
        let t = Instant::now();
        match self.decoder.decode_float(opus, &mut self.pcm) {
            Ok(n) => self.resampler.push(&self.pcm[..2 * n]),
            Err(_) => {
                self.counts.errors += 1;
                let n = self.decoder.conceal_float(&mut self.pcm);
                self.resampler.push(&self.pcm[..2 * n]);
            }
        }
        let ns = t.elapsed().as_nanos() as u64;
        self.counts.decode_ns += ns;
        self.counts.decode_max_ns = self.counts.decode_max_ns.max(ns);
    }

    /// What going wrong counts: the line is renewed when one moves.
    fn events(&self, sink: &dyn Sink) -> [u64; 7] {
        let j = &self.jitter;
        [
            self.counts.errors,
            j.lost,
            j.late,
            j.dropped,
            j.resyncs,
            sink.underruns() as u64,
            (self.counts.packets > 0) as u64 | ((sink.name() == "clock") as u64) << 1,
        ]
    }

    fn report(&self, sink: &dyn Sink) -> String {
        let c = &self.counts;
        let j = &self.jitter;
        let p = self.arrivals.percentiles();
        format!(
            "luna: audio to {}: {} packets, {} silent, {} errors, {} lost, {} late, {} dup, {} dropped, \
             {} resyncs, {} underruns; decode {} us mean {} us max; buffered {:.0} ms at {:+.0} ppm; \
             arrival p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ms",
            sink.name(),
            c.packets,
            c.silent,
            c.errors,
            j.lost,
            j.late,
            j.duplicates,
            j.dropped,
            j.resyncs,
            sink.underruns(),
            c.decode_ns / 1000 / c.packets.max(1),
            c.decode_max_ns / 1000,
            self.drift.smoothed() / RATE * 1000.0,
            (self.ratio - 1.0) * 1e6,
            p[0],
            p[1],
            p[2],
            p[3]
        )
    }
}

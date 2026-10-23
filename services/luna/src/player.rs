//! Plays the audio channel on a thread of its own: packets put back in
//! order and timed by `lunacore::stream::playout::Playout` (a delay that
//! follows the network's lateness, 50 to 300 ms), decoded by `nxopus`,
//! resampled a few hundred ppm off 1 to follow the server's clock, and
//! written to the audio service's ring, kept `RING_TOP` ahead. A packet
//! still missing when the ring runs low is waited for a frame more (the
//! delay grows) or concealed. Without an audio service, or with one
//! that does not play at 48 kHz (the emulator's), a clock stands in for
//! the speakers, so the rest still runs.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lunacore::stream::playout::{Drop as Skip, Next, Payload, Playout, Resampler, FRAME};
use nxgfx::audio::Stream;

const RATE: f64 = 48_000.0;
const RING_FRAMES: usize = 8192;
/// Frames kept in the ring (50 ms); more waits as packets.
const RING_TOP: u32 = 2400;
/// The least delay kept (50 ms), and what a steady network needs: the
/// deadline, a frame and 10 ms.
const FLOOR: f64 = 2400.0;
const BASE: f64 = (DEADLINE as usize + FRAME + 480) as f64;
/// A decoded frame this quiet may be dropped to shed a backlog (-40 dBFS).
const QUIET_RMS: f32 = 0.01;
/// Frames the audio service may hold between the ring and the speakers
/// (about 11 ms in 128-frame periods, instead of its default 85 ms).
const AHEAD: u32 = 512;
/// Below this the next packet is given up for lost: the service takes a
/// period of 128 frames at a time, a short take is an underrun, and the
/// ring is topped up every `TICK` (144 frames).
const DEADLINE: u32 = 384;
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
    playout: Playout,
    resampler: Resampler,
    decoder: nxopus::Decoder,
    pcm: Vec<f32>,
    out: Vec<f32>,
    out16: Vec<i16>,
    counts: Counts,
    started: Instant,
    ratio: f64,
    /// Frames written, and when the first was.
    written: u64,
    first_write: Option<Instant>,
    /// The first seconds, event by event, printed if they went wrong.
    trace: Vec<String>,
    trace_wanted: bool,
}

const TRACE_FOR: Duration = Duration::from_secs(4);

fn run(rx: Receiver<In>, report: Arc<Mutex<String>>) {
    let mut sink: Box<dyn Sink> = match Stream::open_ahead(RING_FRAMES, AHEAD) {
        Ok(s) => {
            let (period, depth) = s.dma();
            println!("luna: audio service plays {depth} periods of {period} frames ahead");
            Box::new(s)
        }
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
        playout: Playout::new(BASE, FLOOR, true),
        resampler: Resampler::new(),
        decoder: nxopus::Decoder::new(),
        pcm: vec![0.0; 2 * nxopus::MAX_FRAME],
        out: Vec::with_capacity(2 * RING_FRAMES),
        out16: Vec::with_capacity(2 * RING_FRAMES),
        counts: Counts::default(),
        started: Instant::now(),
        ratio: 1.0,
        written: 0,
        first_write: None,
        trace: Vec::new(),
        trace_wanted: false,
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
        if !st.trace.is_empty() && st.started.elapsed() >= TRACE_FOR {
            if st.trace_wanted {
                for chunk in st.trace.chunks(24) {
                    println!("luna: audio start {}", chunk.join(" "));
                }
            }
            st.trace = Vec::new();
        }
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

fn rms(pcm: &[f32]) -> f32 {
    (pcm.iter().map(|v| v * v).sum::<f32>() / pcm.len().max(1) as f32).sqrt()
}

impl State {
    fn note(&mut self, what: std::fmt::Arguments) {
        let ms = self.started.elapsed().as_millis();
        if ms < TRACE_FOR.as_millis() && self.trace.len() < 600 {
            self.trace.push(format!("{}@{}", what, ms));
        }
    }

    fn take(&mut self, p: In) {
        self.note(format_args!("a{}", p.index.map_or(-1, |i| i as i64)));
        match &p.payload {
            Payload::Silent => self.counts.silent += 1,
            Payload::Opus(_) => self.counts.packets += 1,
        }
        let us = (p.at - self.started).as_micros() as i64;
        self.playout.arrived(us, p.index, p.payload);
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

    /// Frames held ahead of the speakers: ring, resampler, packets.
    fn buffered(&self, queued: u32) -> f64 {
        queued as f64 + self.resampler.pending() + (self.playout.jitter.queued() * FRAME) as f64
    }

    /// Tops the ring up to `RING_TOP`, decoding, expanding or concealing
    /// frames as the resampler runs out.
    fn feed(&mut self, sink: &mut dyn Sink, dt: f64) {
        let mut queued = sink.queued();
        self.ratio = self.playout.tick(self.buffered(queued), dt);
        let now_us = self.started.elapsed().as_micros() as i64;
        if !self.playout.ready(now_us) {
            return;
        }
        while queued < RING_TOP {
            self.out.clear();
            let n = self
                .resampler
                .pull((RING_TOP - queued) as usize, self.ratio, &mut self.out);
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
            let next = self.playout.next(self.buffered(queued), queued < DEADLINE);
            match &next {
                Next::Opus(_) => self.note(format_args!("d{}", queued)),
                Next::Conceal => {
                    self.note(format_args!("C{}", queued));
                    self.trace_wanted = true;
                }
                Next::Expand => {
                    self.note(format_args!("E{}", queued));
                    self.trace_wanted = true;
                }
                _ => {}
            }
            match next {
                Next::Opus(b) => self.decode(&b),
                Next::Silent => self.resampler.push(&[0.0; 2 * FRAME]),
                Next::Conceal | Next::Expand => {
                    let n = self.decoder.conceal_float(&mut self.pcm);
                    self.resampler.push(&self.pcm[..2 * n]);
                }
                Next::Wait => {
                    if self.playout.jitter.queued() == 0 && queued == 0 {
                        self.playout.restart();
                    }
                    break;
                }
            }
        }
    }

    /// Decodes a packet into the resampler, unless a backlog is being
    /// shed and the frame can go.
    fn decode(&mut self, opus: &[u8]) {
        let t = Instant::now();
        let n = match self.decoder.decode_float(opus, &mut self.pcm) {
            Ok(n) => n,
            Err(_) => {
                self.counts.errors += 1;
                self.decoder.conceal_float(&mut self.pcm)
            }
        };
        let ns = t.elapsed().as_nanos() as u64;
        self.counts.decode_ns += ns;
        self.counts.decode_max_ns = self.counts.decode_max_ns.max(ns);
        let pcm = &self.pcm[..2 * n];
        let skip = match self.playout.drop_next() {
            Skip::Keep => false,
            Skip::IfQuiet => rms(pcm) < QUIET_RMS,
            Skip::Always => true,
        };
        if skip {
            self.playout.frame_dropped();
        } else {
            self.resampler.push(pcm);
        }
    }

    /// What going wrong counts: the line is renewed when one moves.
    fn events(&self, sink: &dyn Sink) -> [u64; 8] {
        let j = &self.playout.jitter;
        [
            self.counts.errors,
            j.lost,
            j.late,
            j.dropped + self.playout.dropped,
            j.resyncs,
            j.expansions,
            sink.underruns() as u64,
            (self.counts.packets > 0) as u64 | ((sink.name() == "clock") as u64) << 1,
        ]
    }

    fn report(&self, sink: &dyn Sink) -> String {
        let c = &self.counts;
        let j = &self.playout.jitter;
        let p = self.playout.arrivals.percentiles();
        format!(
            "luna: audio to {}: {} packets, {} silent, {} errors, {} lost, {} late, {} waited for, \
             {} dup, {} dropped, {} resyncs, {} underruns; decode {} us mean {} us max; delay {:.0} ms \
             (buffered {:.0} ms at {:+.0} ppm); arrival p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} ms",
            sink.name(),
            c.packets,
            c.silent,
            c.errors,
            j.lost,
            j.late,
            j.expansions,
            j.duplicates,
            j.dropped + self.playout.dropped,
            j.resyncs,
            sink.underruns(),
            c.decode_ns / 1000 / c.packets.max(1),
            c.decode_max_ns / 1000,
            self.playout.target() / RATE * 1000.0,
            self.playout.buffered() / RATE * 1000.0,
            (self.ratio - 1.0) * 1e6,
            p[0],
            p[1],
            p[2],
            p[3]
        )
    }
}

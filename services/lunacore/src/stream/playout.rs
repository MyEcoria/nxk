//! Audio playout with no I/O: packets put back in `frame_index` order and
//! taken when the output needs them (one still missing at its deadline is
//! concealed), the arrival jitter measured, and the drift between the
//! server's clock and the speakers' taken up by resampling at a ratio a
//! few hundred ppm from 1 (`Drift`, `Resampler`): inaudible, unlike
//! dropping or repeating frames. `Playout` puts them together with a
//! delay that follows the network: it grows by concealing a frame without
//! giving the late packet up (an expansion), and shrinks slowly.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

/// Frames concealed in a row before giving up and waiting (200 ms).
pub const MAX_CONCEALED: u32 = 10;
/// A jump in `frame_index` past this is a restart, not losses (500 ms).
pub const RESYNC_GAP: u64 = 25;
/// Frames inserted in a row to wait for a late packet (300 ms).
pub const MAX_EXPANDED: u32 = 15;
/// Samples per channel of a packet (20 ms at 48 kHz).
pub const FRAME: usize = 960;

/// A packet's content.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Opus(Vec<u8>),
    /// The server's silence (`strength == -1`).
    Silent,
}

/// What to play for the next frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    Opus(Vec<u8>),
    Silent,
    /// Lost: the decoder fills it.
    Conceal,
    /// Late: the decoder fills a frame and the packet is still awaited
    /// (the delay grows by a frame).
    Expand,
    /// Nothing to play yet.
    Wait,
}

impl From<Payload> for Next {
    fn from(p: Payload) -> Self {
        match p {
            Payload::Opus(b) => Next::Opus(b),
            Payload::Silent => Next::Silent,
        }
    }
}

/// # Packets in `frame_index` order, handed out as the output asks.
///
/// ## Example
/// ```
/// use lunacore::stream::playout::{Jitter, Next, Payload};
/// let mut j = Jitter::default();
/// j.push(Some(1), Payload::Silent);
/// j.push(Some(0), Payload::Opus(vec![0xfc]));
/// assert_eq!(j.pop(false, false), Next::Opus(vec![0xfc]));
/// assert_eq!(j.pop(false, false), Next::Silent);
/// assert_eq!(j.pop(false, false), Next::Wait);
/// assert_eq!(j.pop(true, true), Next::Expand);
/// assert_eq!(j.pop(true, false), Next::Conceal);
/// ```
#[derive(Debug, Default)]
pub struct Jitter {
    pending: BTreeMap<u64, Payload>,
    next: Option<u64>,
    concealed_run: u32,
    expand_run: u32,
    pub lost: u64,
    pub expansions: u64,
    pub late: u64,
    pub duplicates: u64,
    pub resyncs: u64,
    pub dropped: u64,
}

impl Jitter {
    /// Queues a packet; one older than what was already played, or seen
    /// twice, is counted and dropped. Without an index it follows the
    /// newest one.
    pub fn push(&mut self, index: Option<u64>, p: Payload) {
        let index = match index {
            Some(i) => i,
            None => self
                .pending
                .keys()
                .next_back()
                .map(|k| k + 1)
                .or(self.next)
                .unwrap_or(0),
        };
        if self.next.is_some_and(|n| index < n) {
            self.late += 1;
            return;
        }
        match self.pending.entry(index) {
            Entry::Occupied(_) => self.duplicates += 1,
            Entry::Vacant(v) => {
                v.insert(p);
            }
        }
    }

    /// Packets waiting.
    pub fn queued(&self) -> usize {
        self.pending.len()
    }

    /// Drops the oldest packets past `keep` (a burst after a stall would
    /// otherwise stay as latency for good).
    pub fn trim(&mut self, keep: usize) {
        while self.pending.len() > keep {
            if let Some((k, _)) = self.pending.pop_first() {
                self.next = Some(k + 1);
                self.dropped += 1;
            }
        }
    }

    /// The next frame to play; `due`: the output cannot wait any longer,
    /// so a missing packet is waited for a frame more (`grow`) or given
    /// up for lost.
    pub fn pop(&mut self, due: bool, grow: bool) -> Next {
        let first = self.pending.keys().next().copied();
        let next = match (self.next, first) {
            (Some(n), _) => n,
            (None, Some(f)) => f,
            (None, None) => return Next::Wait,
        };
        if let Some(p) = self.pending.remove(&next) {
            self.next = Some(next + 1);
            self.concealed_run = 0;
            self.expand_run = 0;
            return p.into();
        }
        if let Some(f) = first {
            if f - next > RESYNC_GAP || (due && self.concealed_run >= MAX_CONCEALED) {
                self.lost += f - next;
                self.resyncs += 1;
                self.next = Some(f);
                return self.pop(due, grow);
            }
        }
        if !due {
            return Next::Wait;
        }
        if grow && self.expand_run < MAX_EXPANDED {
            self.expand_run += 1;
            self.expansions += 1;
            return Next::Expand;
        }
        if self.concealed_run >= MAX_CONCEALED {
            // Stalled: start again from whatever comes next.
            self.next = None;
            return Next::Wait;
        }
        self.next = Some(next + 1);
        self.lost += 1;
        self.concealed_run += 1;
        Next::Conceal
    }
}

/// # How late packets come compared with a steady clock.
///
/// `arrived(us, index)` with the frame length; `percentiles` gives the
/// lateness in ms over the last packets, against the earliest of them.
#[derive(Debug)]
pub struct Arrivals {
    frame_us: i64,
    offsets: Vec<i64>,
    at: usize,
}

const ARRIVALS_KEPT: usize = 500;

impl Arrivals {
    pub fn new(frame_us: i64) -> Self {
        Self {
            frame_us,
            offsets: Vec::with_capacity(ARRIVALS_KEPT),
            at: 0,
        }
    }

    pub fn arrived(&mut self, us: i64, index: u64) {
        let offset = us - index as i64 * self.frame_us;
        if self.offsets.len() < ARRIVALS_KEPT {
            self.offsets.push(offset);
        } else {
            self.offsets[self.at] = offset;
            self.at = (self.at + 1) % ARRIVALS_KEPT;
        }
    }

    /// Lateness in microseconds at quantile `q` over the last `n` packets,
    /// against the earliest of them.
    pub fn recent(&self, n: usize, q: f64) -> i64 {
        let len = self.offsets.len();
        let n = n.min(len);
        if n == 0 {
            return 0;
        }
        let mut last: Vec<i64> = if len < ARRIVALS_KEPT {
            self.offsets[len - n..].to_vec()
        } else {
            (0..n)
                .map(|k| self.offsets[(self.at + ARRIVALS_KEPT - n + k) % ARRIVALS_KEPT])
                .collect()
        };
        let min = *last.iter().min().unwrap_or(&0);
        last.iter_mut().for_each(|o| *o -= min);
        last.sort_unstable();
        last[((n - 1) as f64 * q) as usize]
    }

    /// Lateness in ms at the 50th, 95th and 99th percentiles and the most.
    pub fn percentiles(&self) -> [f64; 4] {
        let Some(&min) = self.offsets.iter().min() else {
            return [0.0; 4];
        };
        let mut late: Vec<i64> = self.offsets.iter().map(|o| o - min).collect();
        late.sort_unstable();
        let at = |q: f64| late[((late.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
        [at(0.5), at(0.95), at(0.99), at(1.0)]
    }
}

/// # The resampling ratio that keeps the buffered audio at a set point.
///
/// A PI controller on the buffered frames, smoothed over a few seconds:
/// more audio than wanted means the server's clock runs fast, so the
/// input is consumed faster (ratio above 1), within `MAX_PPM`. The
/// buffer integrates the rate error (-0.048 frames/s per ppm), so the
/// gains put the loop's time constant near 20 s, critically damped; the
/// integral (the clocks' steady drift) stops while the output is at its
/// limit. A backlog bigger than resampling can take up soon is the
/// caller's to drop (`excess`).
#[derive(Debug)]
pub struct Drift {
    setpoint: f64,
    smoothed: Option<f64>,
    integral: f64,
}

/// The most the ratio moves from 1: 2000 ppm is 3.5 cents of pitch.
pub const MAX_PPM: f64 = 2000.0;
const SMOOTH_SECS: f64 = 2.0;
/// ppm per frame of error: 2 * zeta * omega / 0.048 with omega 0.05 rad/s.
const KP: f64 = 2.0;
/// ppm per frame-second of error: omega^2 / 0.048.
const KI: f64 = 0.05;
/// The steady drift learnt at most.
const MAX_DRIFT_PPM: f64 = 1500.0;
/// The integral learns the drift only this close to the set point, so a
/// set point that moves (an adaptive delay) does not teach it a drift.
const INTEGRATE_WITHIN: f64 = 960.0;

impl Drift {
    /// `setpoint`: the frames to keep buffered.
    pub fn new(setpoint: f64) -> Self {
        Self {
            setpoint,
            smoothed: None,
            integral: 0.0,
        }
    }

    /// The buffered frames now, `dt` seconds after the last call; returns
    /// the ratio (input frames per output frame).
    pub fn update(&mut self, buffered: f64, dt: f64) -> f64 {
        let s = match self.smoothed {
            None => buffered,
            Some(s) => s + (buffered - s) * (dt / SMOOTH_SECS).min(1.0),
        };
        self.smoothed = Some(s);
        let error = s - self.setpoint;
        let free = KP * error + self.integral;
        if free.abs() < MAX_PPM && error.abs() < INTEGRATE_WITHIN {
            self.integral = (self.integral + KI * error * dt).clamp(-MAX_DRIFT_PPM, MAX_DRIFT_PPM);
        }
        let ppm = (KP * error + self.integral).clamp(-MAX_PPM, MAX_PPM);
        1.0 + ppm * 1e-6
    }

    /// A new set point, in frames.
    pub fn set_setpoint(&mut self, setpoint: f64) {
        self.setpoint = setpoint;
    }

    /// The smoothed buffered frames.
    pub fn smoothed(&self) -> f64 {
        self.smoothed.unwrap_or(0.0)
    }

    /// Frames over the set point (smoothed), for a caller that drops
    /// whole frames past some excess.
    pub fn excess(&self) -> f64 {
        self.smoothed() - self.setpoint
    }

    /// `frames` were dropped from the buffer.
    pub fn dropped(&mut self, frames: f64) {
        if let Some(s) = self.smoothed.as_mut() {
            *s -= frames;
        }
    }
}

/// The most audio held to ride out the network's stalls (300 ms).
pub const MAX_TARGET: f64 = 14_400.0;
/// Packets the delay is worked out over (3 s): it follows the network fast.
const TARGET_WINDOW: usize = 150;
const TARGET_QUANTILE: f64 = 0.95;
/// How slowly a delay no longer needed comes down (and by resampling).
const TARGET_DECAY_SECS: f64 = 15.0;
/// Packets kept waiting at most; a burst past it is dropped.
pub const MAX_QUEUED: usize = 24;

/// What to do with the next decoded frame when too much is buffered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drop {
    Keep,
    /// Drop it if it is quiet (a skip there goes unheard).
    IfQuiet,
    /// Drop it anyway: far too much is buffered.
    Always,
}

/// # The playout's decisions: when to start, what to play, the delay.
///
/// The delay (`target`, in frames buffered) is the network's recent
/// lateness (95th percentile over 3 s) above `base`, kept within
/// `floor..MAX_TARGET`; it rises at once and comes down over ~15 s. At
/// the start nothing plays until the delay is in hand. A packet missing
/// at the deadline is waited for one more frame while less than the
/// delay is buffered, else given up. Fixed (`adaptive` false), the delay
/// stays at `floor`.
///
/// ## Example
/// ```
/// use lunacore::stream::playout::{Next, Payload, Playout};
/// let mut p = Playout::new(1824.0, 2400.0, true);
/// for i in 0..3 {
///     p.arrived(i * 20_000, Some(i as u64), Payload::Silent);
/// }
/// assert!(p.ready(40_000));
/// assert_eq!(p.next(0.0, true), Next::Silent);
/// ```
#[derive(Debug)]
pub struct Playout {
    pub jitter: Jitter,
    pub arrivals: Arrivals,
    drift: Drift,
    base: f64,
    floor: f64,
    target: f64,
    adaptive: bool,
    pub primed: bool,
    first_us: Option<i64>,
    pub dropped: u64,
}

impl Playout {
    /// `base`: the frames the output needs on a steady network (its
    /// deadline and a frame); `floor`: the least delay kept.
    pub fn new(base: f64, floor: f64, adaptive: bool) -> Self {
        Self {
            jitter: Jitter::default(),
            arrivals: Arrivals::new((FRAME as i64 * 1_000_000) / 48_000),
            drift: Drift::new(floor),
            base,
            floor,
            target: floor,
            adaptive,
            primed: false,
            first_us: None,
            dropped: 0,
        }
    }

    /// A packet came at `us` (any steady clock).
    pub fn arrived(&mut self, us: i64, index: Option<u64>, p: Payload) {
        if let Some(i) = index {
            self.arrivals.arrived(us, i);
        }
        if !self.primed && self.first_us.is_none() {
            self.first_us = Some(us);
        }
        self.jitter.push(index, p);
        self.jitter.trim(MAX_QUEUED);
    }

    /// Whether playing may start (or go on): once the delay is in hand,
    /// by packets or by the time since the first one.
    pub fn ready(&mut self, now_us: i64) -> bool {
        if self.primed {
            return true;
        }
        let queued = self.jitter.queued();
        let waited = self
            .first_us
            .map_or(0.0, |f| (now_us - f) as f64 * 48_000.0 / 1e6);
        if queued > 0 && ((queued * FRAME) as f64 >= self.target || waited >= self.target) {
            self.primed = true;
            self.first_us = None;
        }
        self.primed
    }

    /// Play nothing until the delay is in hand again (after a stall).
    pub fn restart(&mut self) {
        self.primed = false;
        self.first_us = None;
    }

    /// Updates the delay and returns the resampling ratio for the
    /// `buffered` frames, `dt` seconds after the last call.
    pub fn tick(&mut self, buffered: f64, dt: f64) -> f64 {
        if self.adaptive {
            let late = self.arrivals.recent(TARGET_WINDOW, TARGET_QUANTILE) as f64 * 48_000.0 / 1e6;
            let want = (self.base + late).clamp(self.floor, MAX_TARGET);
            if want > self.target {
                self.target = want;
            } else {
                self.target -= (self.target - want) * (dt / TARGET_DECAY_SECS).min(1.0);
            }
        }
        self.drift.set_setpoint(self.target);
        self.drift.update(buffered, dt)
    }

    /// The next frame, `buffered` frames held and `due` when the output
    /// cannot wait.
    pub fn next(&mut self, buffered: f64, due: bool) -> Next {
        let grow = self.adaptive && buffered < self.target - (FRAME / 2) as f64;
        self.jitter.pop(due, grow)
    }

    /// Whether to drop the next decoded frame (too much buffered after a
    /// burst: resampling would take minutes).
    pub fn drop_next(&self) -> Drop {
        let limit = (self.target * 0.5).max(1920.0);
        let excess = self.drift.excess();
        if excess > 2.0 * limit {
            Drop::Always
        } else if excess > limit {
            Drop::IfQuiet
        } else {
            Drop::Keep
        }
    }

    /// A decoded frame was dropped.
    pub fn frame_dropped(&mut self) {
        self.dropped += 1;
        self.drift.dropped(FRAME as f64);
    }

    /// The delay aimed at, in frames.
    pub fn target(&self) -> f64 {
        self.target
    }

    /// The buffered frames, smoothed.
    pub fn buffered(&self) -> f64 {
        self.drift.smoothed()
    }
}

const TAPS: usize = 32;
const HALF: usize = TAPS / 2;
const PHASES: usize = 256;
const KAISER_BETA: f64 = 8.0;

/// # Stereo resampling at a ratio near 1, by a windowed sinc.
///
/// 32 taps (Kaiser, β 8: flat to about 19 kHz) at 256 phases with linear
/// interpolation between them. `push` interleaved input, `pull` output.
pub struct Resampler {
    table: Vec<f32>,
    buf: Vec<[f32; 2]>,
    pos: f64,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}

impl Default for Resampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Resampler {
    pub fn new() -> Self {
        let mut table = vec![0f32; (PHASES + 1) * TAPS];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut table[p * TAPS..(p + 1) * TAPS];
            let mut sum = 0.0;
            let mut taps = [0f64; TAPS];
            for (j, t) in taps.iter_mut().enumerate() {
                // Distance from the output point to input sample j.
                let x = frac + (HALF - 1) as f64 - j as f64;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let r = x / HALF as f64;
                let w = if r.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / bessel_i0(KAISER_BETA)
                };
                *t = sinc * w;
                sum += *t;
            }
            for (o, t) in row.iter_mut().zip(taps) {
                *o = (t / sum) as f32;
            }
        }
        Self {
            table,
            buf: vec![[0.0; 2]; HALF - 1],
            pos: (HALF - 1) as f64,
        }
    }

    pub fn push(&mut self, pcm: &[f32]) {
        self.buf.extend_from_slice(pcm.as_chunks::<2>().0);
    }

    /// Input frames not yet consumed.
    pub fn pending(&self) -> f64 {
        (self.buf.len() as f64 - self.pos - HALF as f64).max(0.0)
    }

    /// Up to `max` output frames at `ratio` (input frames per output
    /// frame), appended to `out` interleaved; returns how many.
    pub fn pull(&mut self, max: usize, ratio: f64, out: &mut Vec<f32>) -> usize {
        let mut n = 0;
        while n < max {
            let c = self.pos as usize;
            if c + HALF >= self.buf.len() {
                break;
            }
            let ph = (self.pos - c as f64) * PHASES as f64;
            let p = (ph as usize).min(PHASES - 1);
            let f = (ph - p as f64) as f32;
            let (r0, r1) = (
                &self.table[p * TAPS..][..TAPS],
                &self.table[(p + 1) * TAPS..][..TAPS],
            );
            let x = &self.buf[c + 1 - HALF..c + 1 + HALF];
            let (mut l, mut r) = (0f32, 0f32);
            for j in 0..TAPS {
                let h = r0[j] + (r1[j] - r0[j]) * f;
                l += x[j][0] * h;
                r += x[j][1] * h;
            }
            out.push(l);
            out.push(r);
            self.pos += ratio;
            n += 1;
        }
        let drop = (self.pos as usize).saturating_sub(HALF - 1);
        if drop > 0 {
            self.buf.drain(..drop.min(self.buf.len()));
            self.pos -= drop as f64;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(i: u8) -> Payload {
        Payload::Opus(vec![i])
    }

    #[test]
    fn reorders_and_conceals_only_when_due() {
        let mut j = Jitter::default();
        j.push(Some(10), opus(10));
        j.push(Some(12), opus(12));
        assert_eq!(j.pop(false, false), Next::Opus(vec![10]));
        assert_eq!(j.pop(false, false), Next::Wait);
        j.push(Some(11), opus(11));
        assert_eq!(j.pop(false, false), Next::Opus(vec![11]));
        assert_eq!(j.pop(false, false), Next::Opus(vec![12]));
        j.push(Some(14), opus(14));
        assert_eq!(j.pop(true, false), Next::Conceal);
        assert_eq!(j.pop(false, false), Next::Opus(vec![14]));
        j.push(Some(13), opus(13));
        assert_eq!((j.lost, j.late), (1, 1));
    }

    #[test]
    fn duplicates_are_dropped() {
        let mut j = Jitter::default();
        j.push(Some(0), opus(0));
        j.push(Some(0), opus(9));
        assert_eq!(j.duplicates, 1);
        assert_eq!(j.pop(false, false), Next::Opus(vec![0]));
    }

    #[test]
    fn a_big_jump_resyncs() {
        let mut j = Jitter::default();
        j.push(Some(0), opus(0));
        assert_eq!(j.pop(false, false), Next::Opus(vec![0]));
        j.push(Some(1000), opus(1));
        assert_eq!(j.pop(false, false), Next::Opus(vec![1]));
        assert_eq!(j.resyncs, 1);
    }

    #[test]
    fn a_stall_stops_concealing_then_restarts() {
        let mut j = Jitter::default();
        j.push(Some(0), opus(0));
        j.pop(true, false);
        for _ in 0..MAX_CONCEALED {
            assert_eq!(j.pop(true, false), Next::Conceal);
        }
        assert_eq!(j.pop(true, false), Next::Wait);
        j.push(Some(500), opus(5));
        assert_eq!(j.pop(false, false), Next::Opus(vec![5]));
    }

    #[test]
    fn trim_drops_the_oldest() {
        let mut j = Jitter::default();
        for i in 0..8 {
            j.push(Some(i), opus(i as u8));
        }
        j.trim(3);
        assert_eq!(j.dropped, 5);
        assert_eq!(j.pop(false, false), Next::Opus(vec![5]));
    }

    #[test]
    fn arrival_percentiles() {
        let mut a = Arrivals::new(20_000);
        for i in 0..100u64 {
            let late = if i % 10 == 0 { 15_000 } else { 0 };
            a.arrived(i as i64 * 20_000 + late + 7, i);
        }
        let p = a.percentiles();
        assert_eq!(p[0], 0.0);
        assert_eq!(p[3], 15.0);
    }

    #[test]
    fn drift_speeds_up_when_too_much_is_buffered() {
        let mut d = Drift::new(2400.0);
        assert!(d.update(2400.0, 0.02) == 1.0);
        let mut r = 1.0;
        for _ in 0..500 {
            r = d.update(2400.0 + 480.0, 0.02);
        }
        assert!(r > 1.0001 && r <= 1.0 + MAX_PPM * 1e-6, "{}", r);
        for _ in 0..5000 {
            r = d.update(0.0, 0.02);
        }
        assert!((1.0 - MAX_PPM * 1e-6..1.0 - 500e-6).contains(&r), "{}", r);
    }

    /// The buffer under a server clock off by up to 1400 ppm, starting on
    /// the set point or 40 ms over it: the ratio settles on the drift, the
    /// buffer on the set point, and it never runs 15 ms low.
    #[test]
    fn drift_settles_without_running_low() {
        let (setpoint, dt) = (2880.0, 0.02);
        for &drift in &[250e-6, -250e-6, 1400e-6, -1400e-6] {
            for &start in &[0.0, 1920.0] {
                let mut d = Drift::new(setpoint);
                let mut buffered = setpoint + start;
                let mut lowest = buffered;
                let mut r = 1.0;
                for _ in 0..15_000 {
                    r = d.update(buffered, dt);
                    buffered += 960.0 * (1.0 + drift) - 960.0 * r;
                    lowest = lowest.min(buffered);
                }
                assert!(
                    (r - 1.0 - drift).abs() < 20e-6,
                    "{} {}: {}",
                    drift,
                    start,
                    r
                );
                assert!(
                    (buffered - setpoint).abs() < 48.0,
                    "{} {}: {}",
                    drift,
                    start,
                    buffered
                );
                assert!(lowest > setpoint - 720.0, "{} {}: {}", drift, start, lowest);
            }
        }
    }

    /// What a simulated minute of playout gave.
    #[derive(Debug)]
    struct Run {
        concealed: u64,
        underrun_ms: u64,
        mean_target_ms: f64,
        last_target_ms: f64,
    }

    /// A server sending a packet every 20 ms over a network adding 10-18
    /// ms, plus a stall of `stall_ms` every 10 s (the packets sent during
    /// it come together at its end); the speakers take 48 frames a ms
    /// from a ring topped up to 50 ms every 3 ms, as luna's player does.
    fn simulate(adaptive: bool, stall_ms: u64, secs: u64) -> Run {
        const TOP: f64 = 2400.0;
        const DEADLINE: f64 = 384.0;
        let mut p = Playout::new(1824.0, 2400.0, adaptive);
        let mut arrivals: Vec<(u64, u64)> = Vec::new();
        let mut seed = 12345u32;
        for i in 0..secs * 50 {
            let sent = i * 20;
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let mut at = sent + 10 + (seed >> 29) as u64;
            let phase = sent % 10_000;
            if stall_ms > 0 && sent >= 5_000 && phase >= 5_000 && phase < 5_000 + stall_ms {
                at = at.max(sent - phase + 5_000 + stall_ms + (i % 3));
            }
            arrivals.push((at, i));
        }
        arrivals.sort();
        let (mut ring, mut pcm) = (0f64, 0f64);
        let mut run = Run {
            concealed: 0,
            underrun_ms: 0,
            mean_target_ms: 0.0,
            last_target_ms: 0.0,
        };
        let mut next_arrival = 0;
        let mut target_sum = 0.0;
        for ms in 0..secs * 1000 {
            while next_arrival < arrivals.len() && arrivals[next_arrival].0 <= ms {
                let (at, i) = arrivals[next_arrival];
                p.arrived(at as i64 * 1000, Some(i), Payload::Opus(vec![0]));
                next_arrival += 1;
            }
            if ring >= 48.0 {
                ring -= 48.0;
            } else if p.primed {
                run.underrun_ms += 1;
                ring = 0.0;
            }
            if ms % 3 != 0 {
                continue;
            }
            let buffered = ring + pcm + (p.jitter.queued() * FRAME) as f64;
            p.tick(buffered, 0.003);
            target_sum += p.target();
            if !p.ready(ms as i64 * 1000) {
                continue;
            }
            while ring < TOP {
                if pcm > 0.0 {
                    let n = pcm.min(TOP - ring);
                    ring += n;
                    pcm -= n;
                    continue;
                }
                let buffered = ring + (p.jitter.queued() * FRAME) as f64;
                match p.next(buffered, ring < DEADLINE) {
                    Next::Opus(_) | Next::Silent => {
                        if p.drop_next() == Drop::Always {
                            p.frame_dropped();
                        } else {
                            pcm += FRAME as f64;
                        }
                    }
                    Next::Conceal => {
                        run.concealed += 1;
                        pcm += FRAME as f64;
                    }
                    Next::Expand => pcm += FRAME as f64,
                    Next::Wait => {
                        if p.jitter.queued() == 0 && ring == 0.0 {
                            p.restart();
                        }
                        break;
                    }
                }
            }
        }
        run.mean_target_ms = target_sum / (secs * 1000 / 3) as f64 / 48.0;
        run.last_target_ms = p.target() / 48.0;
        run
    }

    #[test]
    fn a_steady_network_plays_without_loss_at_the_floor() {
        for adaptive in [false, true] {
            let r = simulate(adaptive, 0, 30);
            assert_eq!((r.concealed, r.underrun_ms), (0, 0), "{:?}", r);
            assert!(r.mean_target_ms < 55.0, "{:?}", r);
        }
    }

    #[test]
    fn stalls_cost_far_less_with_an_adaptive_delay() {
        let fixed = simulate(false, 250, 60);
        let adaptive = simulate(true, 250, 60);
        assert!(fixed.concealed >= 30, "{:?}", fixed);
        assert!(
            adaptive.concealed * 4 <= fixed.concealed,
            "{:?} vs {:?}",
            adaptive,
            fixed
        );
        assert!(adaptive.last_target_ms <= 300.0, "{:?}", adaptive);
    }

    #[test]
    fn the_delay_comes_down_once_the_stalls_stop() {
        // Stalls for 30 s (in `simulate`, from 5 s), then none for 30 s.
        let mut quiet = simulate(true, 250, 20);
        assert!(quiet.last_target_ms > 100.0, "{:?}", quiet);
        quiet = simulate(true, 0, 20);
        assert!(quiet.last_target_ms < 60.0, "{:?}", quiet);
    }

    /// A tone through the resampler at ratio 1 is the tone; at 1.001 it
    /// is 0.1% shorter and still clean.
    #[test]
    fn resampling_keeps_a_tone_clean() {
        let tone = |i: usize| {
            (2.0 * std::f64::consts::PI * 5000.0 * i as f64 / 48000.0).sin() as f32 * 0.5
        };
        let input: Vec<f32> = (0..48000).flat_map(|i| [tone(i), -tone(i)]).collect();
        for &ratio in &[1.0, 1.001, 0.999] {
            let mut rs = Resampler::new();
            let mut out = Vec::new();
            for chunk in input.chunks(1920) {
                rs.push(chunk);
                rs.pull(usize::MAX, ratio, &mut out);
            }
            let n = out.len() / 2;
            let expect = (48000.0 / ratio) as usize;
            assert!(
                (n as i64 - expect as i64).abs() <= 32,
                "{} vs {}",
                n,
                expect
            );
            // Output frame k is input position k * ratio.
            let (mut sig, mut err) = (0f64, 0f64);
            for k in 1000..n - 1000 {
                let t = k as f64 * ratio;
                let want = (2.0 * std::f64::consts::PI * 5000.0 * t / 48000.0).sin() * 0.5;
                sig += want * want;
                err += (out[2 * k] as f64 - want).powi(2);
                assert!((out[2 * k + 1] + out[2 * k]).abs() < 1e-5);
            }
            let snr = 10.0 * (sig / err).log10();
            assert!(snr > 70.0, "ratio {}: SNR {:.1} dB", ratio, snr);
        }
    }
}

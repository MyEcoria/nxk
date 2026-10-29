//! Makeup gain with a look-ahead peak limiter, for a stream that arrives
//! far below full scale (Luna's sits near -42 dBFS RMS, peaks near
//! -23 dBFS). Stereo, interleaved; one gain for both channels so the
//! image does not move.
//!
//! The gain needed to keep each frame under the ceiling is held over a
//! window of `LOOKAHEAD` frames, then averaged over the same window: it
//! has come down by the time the peak is played, as a ramp and not a
//! step. It goes back up with a one-pole release, which only ever
//! lowers the gain it follows, so the ceiling holds for any input.

use std::collections::VecDeque;

/// Frames the output is delayed by, and the span of the held minimum and
/// of its average (2 ms at 48 kHz).
const LOOKAHEAD: usize = 96;
/// Time constant of the gain's return, in frames (50 ms).
const RELEASE: f32 = 2400.0;

/// The linear factor of `db`.
pub fn from_db(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub struct Limiter {
    gain: f32,
    ceiling: f32,
    /// Frames waiting for their gain, oldest first.
    delay: VecDeque<[f32; 2]>,
    /// Needed gain of the last `LOOKAHEAD` input frames.
    need: VecDeque<f32>,
    /// Minimum of `need` as it was at each of the last `LOOKAHEAD` frames,
    /// and their sum.
    held: VecDeque<f32>,
    sum: f64,
    /// The gain reduction applied to the last frame (1 = none).
    smooth: f32,
    /// Frames, frames limited by more than 0.5 dB, greatest reduction (linear).
    frames: u64,
    limited: u64,
    lowest: f32,
}

impl Limiter {
    /// `gain_db` of makeup, peaks held under `ceiling_db` (dBFS, negative).
    pub fn new(gain_db: f32, ceiling_db: f32) -> Self {
        Self {
            gain: from_db(gain_db),
            ceiling: from_db(ceiling_db),
            delay: VecDeque::from(vec![[0.0; 2]; LOOKAHEAD - 1]),
            need: VecDeque::from(vec![1.0; LOOKAHEAD]),
            held: VecDeque::from(vec![1.0; LOOKAHEAD]),
            sum: LOOKAHEAD as f64,
            smooth: 1.0,
            frames: 0,
            limited: 0,
            lowest: 1.0,
        }
    }

    /// Applies the gain and the limit to interleaved stereo `pcm`, in place.
    pub fn process(&mut self, pcm: &mut [f32]) {
        for f in pcm.as_chunks_mut::<2>().0 {
            let x = [f[0] * self.gain, f[1] * self.gain];
            let peak = x[0].abs().max(x[1].abs());
            let need = if peak > self.ceiling {
                self.ceiling / peak
            } else {
                1.0
            };
            self.need.pop_front();
            self.need.push_back(need);
            let min = self.need.iter().copied().fold(1.0, f32::min);
            self.sum += (min - self.held.pop_front().unwrap_or(1.0)) as f64;
            self.held.push_back(min);
            let avg = ((self.sum / LOOKAHEAD as f64) as f32).clamp(0.0, 1.0);
            self.smooth = avg.min(self.smooth + (1.0 - self.smooth) / RELEASE);
            self.delay.push_back(x);
            let y = self.delay.pop_front().unwrap_or([0.0; 2]);
            f[0] = y[0] * self.smooth;
            f[1] = y[1] * self.smooth;
            self.frames += 1;
            self.limited += (self.smooth < 0.944) as u64;
            self.lowest = self.lowest.min(self.smooth);
        }
    }

    /// How often it acted, for the log line.
    pub fn report(&self) -> String {
        format!(
            "limiter limited {:.1}% of frames, at most {:.1} dB",
            100.0 * self.limited as f64 / self.frames.max(1) as f64,
            -20.0 * self.lowest.log10()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(l: &mut Limiter, pcm: &[f32]) -> Vec<f32> {
        let mut out = pcm.to_vec();
        l.process(&mut out);
        out
    }

    fn sine(amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (i as f32 * 0.05).sin();
                [v, v * 0.7]
            })
            .collect()
    }

    #[test]
    fn a_quiet_signal_comes_out_at_exactly_the_gain() {
        let mut l = Limiter::new(12.0, -1.0);
        let input = sine(0.01, 4000);
        let out = run(&mut l, &input);
        let g = from_db(12.0);
        // Past the look-ahead delay, sample k is input k - (LOOKAHEAD - 1).
        for k in 2 * LOOKAHEAD..4000 {
            let want = input[2 * (k - (LOOKAHEAD - 1))] * g;
            assert!((out[2 * k] - want).abs() < 1e-6, "frame {k}");
        }
    }

    #[test]
    fn nothing_goes_over_the_ceiling() {
        let mut l = Limiter::new(12.0, -1.0);
        let ceiling = from_db(-1.0);
        // A burst after silence, a loud passage, then quiet again.
        let mut input = vec![0.0; 2000];
        input.extend(sine(1.0, 6000));
        input.extend(sine(0.02, 6000));
        input.extend(sine(0.9, 100));
        let out = run(&mut l, &input);
        let top = out.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(top <= ceiling * 1.0001, "{top} > {ceiling}");
        assert!(top > ceiling * 0.9, "it should reach the ceiling: {top}");
    }

    #[test]
    fn the_gain_moves_in_small_steps() {
        let mut l = Limiter::new(0.0, -1.0);
        let mut input = vec![0.05; 2 * 4000];
        input.extend(vec![1.0; 2 * 4000]);
        let out = run(&mut l, &input);
        // Once the full-scale input has reached the output, a constant
        // input makes the output the gain: it must not jump.
        let from = 4000 + LOOKAHEAD;
        let worst = (from..7999)
            .map(|k| (out[2 * (k + 1)] - out[2 * k]).abs())
            .fold(0f32, f32::max);
        assert!(worst < 0.005, "step of {worst}");
        assert!(out[2 * 7999] < from_db(-1.0) * 1.0001);
    }

    #[test]
    fn the_gain_comes_back_after_a_peak() {
        let mut l = Limiter::new(0.0, -1.0);
        let mut input = vec![0.1; 2 * 500];
        input.extend(vec![1.0; 2 * 50]);
        input.extend(vec![0.1; 2 * 48000]);
        let out = run(&mut l, &input);
        let end = out[out.len() - 1];
        assert!((end - 0.1).abs() < 0.002, "{end}");
    }

    #[test]
    fn it_reports_what_it_did() {
        let mut l = Limiter::new(0.0, -1.0);
        run(&mut l, &sine(1.0, 3000));
        let r = l.report();
        assert!(r.contains("limiter limited"), "{r}");
        assert!(!r.contains("at most 0.0 dB"), "{r}");
    }
}

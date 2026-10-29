//! How loud the decoded audio is: peak and RMS per channel, the side
//! (L - R) level, and the spread of 1 s windows, all in dBFS, for the
//! player's report line.

/// Frames per window (1 s at 48 kHz).
const WINDOW: usize = 48_000;
/// Windows kept for the percentiles (an hour).
const KEPT: usize = 3600;
/// A window quieter than this is silence, not the game's level.
const SILENT_DB: f32 = -80.0;

/// Level of a mean square (full scale sine = -3 dBFS), -inf floored.
fn db(mean_square: f64) -> f32 {
    (10.0 * mean_square.max(1e-12).log10()) as f32
}

#[derive(Default)]
pub struct Meter {
    peak: [f32; 2],
    sumsq: [f64; 2],
    side: f64,
    frames: u64,
    win_sumsq: f64,
    win_frames: usize,
    windows: Vec<f32>,
    silent: u64,
}

impl Meter {
    /// Adds interleaved stereo samples (±1.0).
    pub fn push(&mut self, pcm: &[f32]) {
        for f in pcm.chunks_exact(2) {
            let (l, r) = (f[0], f[1]);
            self.peak[0] = self.peak[0].max(l.abs());
            self.peak[1] = self.peak[1].max(r.abs());
            let (l2, r2) = (l as f64 * l as f64, r as f64 * r as f64);
            self.sumsq[0] += l2;
            self.sumsq[1] += r2;
            self.side += ((l - r) as f64).powi(2) / 2.0;
            self.win_sumsq += l2 + r2;
            self.win_frames += 1;
            if self.win_frames == WINDOW {
                self.close_window();
            }
        }
        self.frames += (pcm.len() / 2) as u64;
    }

    fn close_window(&mut self) {
        let level = db(self.win_sumsq / (2 * WINDOW) as f64);
        if level < SILENT_DB {
            self.silent += 1;
        } else if self.windows.len() < KEPT {
            self.windows.push(level);
        }
        self.win_sumsq = 0.0;
        self.win_frames = 0;
    }

    /// The window level at `p` (0..=1) among the non-silent ones.
    fn percentile(&self, p: f32) -> Option<f32> {
        let mut w = self.windows.clone();
        w.sort_by(|a, b| a.total_cmp(b));
        w.get(((w.len() as f32 - 1.0) * p).round() as usize).copied()
    }

    /// One line: nothing until a frame was decoded.
    pub fn report(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let n = self.frames as f64;
        let peak = |c: usize| db(self.peak[c] as f64 * self.peak[c] as f64);
        let spread = match (self.percentile(0.5), self.percentile(0.95)) {
            (Some(a), Some(b)) => format!("p50 {:.1} p95 {:.1}", a, b),
            _ => "none yet".into(),
        };
        Some(format!(
            "level dBFS rms L {:.1} R {:.1} side {:.1}, peak L {:.1} R {:.1}; 1 s windows ({} silent) {}",
            db(self.sumsq[0] / n),
            db(self.sumsq[1] / n),
            db(self.side / n),
            peak(0),
            peak(1),
            self.silent,
            spread
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (i as f32 * 0.1).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn nothing_before_a_frame() {
        assert!(Meter::default().report().is_none());
    }

    #[test]
    fn full_scale_sine_is_minus_three_rms_zero_peak() {
        let mut m = Meter::default();
        m.push(&sine(1.0, WINDOW * 2));
        let r = m.report().unwrap();
        assert!(r.contains("rms L -3.0 R -3.0"), "{r}");
        assert!(r.contains("peak L 0.0 R 0.0"), "{r}");
        // L == R: no side signal.
        assert!(r.contains("side -120.0"), "{r}");
    }

    #[test]
    fn a_tenth_of_full_scale_is_twenty_db_down() {
        let mut m = Meter::default();
        m.push(&sine(0.1, WINDOW));
        assert!(m.report().unwrap().contains("rms L -23.0"));
    }

    #[test]
    fn silence_is_counted_apart_from_the_level() {
        let mut m = Meter::default();
        m.push(&vec![0.0; 2 * WINDOW]);
        m.push(&sine(0.5, WINDOW));
        let r = m.report().unwrap();
        assert!(r.contains("(1 silent) p50 -9.0 p95 -9.0"), "{r}");
    }

    #[test]
    fn one_channel_only_shows_in_the_side_level() {
        let mut m = Meter::default();
        let v: Vec<f32> = sine(1.0, 1000).chunks(2).flat_map(|f| [f[0], 0.0]).collect();
        m.push(&v);
        let r = m.report().unwrap();
        assert!(r.contains("R -120.0"), "{r}");
        assert!(!r.contains("side -120.0"), "{r}");
    }
}

//! Optional timing of the decoder's phases (feature `prof`): nanoseconds
//! spent in each, summed over every thread.

use std::sync::atomic::AtomicU64;

pub const MB: usize = 0;
pub const RESIDUAL: usize = 1;
pub const MC: usize = 2;
pub const RECON: usize = 3;
pub const NAMES: [&str; 4] = ["macroblocks (all)", "residual parse", "motion compensation", "reconstruction"];

pub static NS: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// Runs `$e` and adds its time to phase `$i` when the feature is on.
#[macro_export]
macro_rules! timed {
    ($i:expr, $e:expr) => {{
        #[cfg(feature = "prof")]
        let t0 = std::time::Instant::now();
        let r = $e;
        #[cfg(feature = "prof")]
        $crate::prof::NS[$i].fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
        r
    }};
}

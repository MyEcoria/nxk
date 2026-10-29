//! The boot screen (`nxk-bootsplash`, as nxk-bl31 and the kernel drew it)
//! until the users are read from the card: the bar goes on from where the
//! kernel left it, fills, and the library takes over.

use nxk_bootsplash::{self as splash, Splash};

use crate::screen::Screen;

/// The bar fills this fast once the users are known.
const FILL_NS: u64 = 200_000_000;
/// No card (or no `fs`): the library anyway.
const GIVE_UP_NS: u64 = 15_000_000_000;
/// The bar nears this while waiting, never reaching it.
const WAIT_TOP: u64 = 950;
/// Waiting time at which the bar is halfway from its start to `WAIT_TOP`.
const WAIT_HALF_NS: u64 = 2_000_000_000;

pub struct Boot {
    /// First frame drawn (0: not yet).
    since: u64,
    /// The users are known: (when, the bar then).
    done: Option<(u64, u32)>,
}

impl Boot {
    pub fn new() -> Self {
        Self {
            since: 0,
            done: None,
        }
    }

    /// The users are known: fill the bar and give way.
    pub fn finish(&mut self, now: u64) {
        if self.done.is_none() {
            let p = self.waiting(now);
            self.done = Some((now, p));
        }
    }

    /// Time to show the library.
    pub fn over(&self, now: u64) -> bool {
        match self.done {
            Some((at, _)) => now >= at + FILL_NS,
            None => self.since != 0 && now >= self.since + GIVE_UP_NS,
        }
    }

    pub fn draw(&mut self, s: &mut Screen, now: u64) {
        if self.since == 0 {
            self.since = now;
        }
        let p = match self.done {
            Some((at, p0)) => {
                let k = (now - at).min(FILL_NS);
                p0 + ((splash::DONE - p0) as u64 * k / FILL_NS) as u32
            }
            None => self.waiting(now),
        };
        draw(s, p);
    }

    /// The bar while waiting: slower and slower towards `WAIT_TOP`.
    fn waiting(&self, now: u64) -> u32 {
        let t = now.saturating_sub(self.since);
        let start = splash::AT_DISPLAY as u64;
        (start + (WAIT_TOP - start) * t / (t + WAIT_HALF_NS)) as u32
    }
}

/// The whole boot screen at progress `p` (thousandths).
pub fn draw(s: &mut Screen, p: u32) {
    let (w, h) = s.size();
    Splash::new(w as u32, h as u32).draw(p, &mut |x, y, w, h, c| {
        s.fill(x as usize, y as usize, w as usize, h as usize, c)
    });
}

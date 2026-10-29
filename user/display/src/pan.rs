//! Touch on the home row: a finger drags the row of games, a flick lets
//! it glide on. A big finger rolls and jitters, so nothing here depends on
//! staying inside a zone: a touch is a tap or a pan by how far it moved.

use alloc::vec::Vec;

use libnx::abi::proto::input;

use crate::home::{GAP, ICON, MIN_SLOTS, X0};
use crate::ui::{Id, Ui, W};

/// A finger moves this far (px, any direction) before it is no tap; along
/// the row, before it pans.
pub(crate) const TOUCH_SLOP: i64 = 26;
/// Where a pan can begin: the title strip, the icons and the gaps (the
/// status bar and the dock above and below have their own zones).
const BAND: (usize, usize) = (100, 462);
/// The last moves that give the speed of a flick, and how stale the last
/// one may be at the lift (the finger stopped, no flick).
const VEL_WINDOW_NS: u64 = 100_000_000;
const VEL_STALE_NS: u64 = 60_000_000;
const VEL_MIN_SPAN_NS: u64 = 15_000_000;
/// Fastest glide, slowest worth keeping (px/s), and its friction (1/s).
const GLIDE_MAX: f32 = 6000.0;
const GLIDE_MIN: f32 = 60.0;
const FRICTION: f32 = 4.0;
/// A frame longer than this steps the glide as if it were this long.
const STEP_MAX_NS: u64 = 50_000_000;

/// A finger down on the row: where, the row's offset then, whether it
/// panned yet, the recent offsets with their time.
struct Touch {
    x0: usize,
    scroll0: f32,
    moved: bool,
    samples: Vec<(u64, f32)>,
}

#[derive(Default)]
pub(crate) struct Pan {
    /// The row is where a hand left it, not where the selection puts it.
    pub free: bool,
    touch: Option<Touch>,
    /// The glide after a flick: px/s toward the end of the row, and the
    /// time of its last step.
    pub glide: f32,
    glide_at: u64,
    /// This touch landed on a gliding row: it stops it, it never taps.
    pub stopped: bool,
    /// Where the touch went down.
    pub from: (usize, usize),
}

impl Pan {
    /// Whether (x, y) is further than a tap allows from where the touch began.
    pub(crate) fn far(&self, x: usize, y: usize) -> bool {
        let (dx, dy) = (x as i64 - self.from.0 as i64, y as i64 - self.from.1 as i64);
        dx * dx + dy * dy > TOUCH_SLOP * TOUCH_SLOP
    }
}

impl Ui {
    /// Whether a finger may take the home row now.
    fn row_touchable(&self) -> bool {
        self.page == crate::ui::LIBRARY
            && !self.overlay()
            && self.flow.is_none()
            && self.starting.is_none()
            && !self.signing_in()
            && self.row_max() > 0
    }

    /// A touch begins: it may be on the row, and it stops a glide.
    pub(crate) fn pan_down(&mut self, x: usize, y: usize, hit: Option<Id>) {
        self.pan.touch = None;
        self.pan.from = (x, y);
        self.pan.stopped = core::mem::take(&mut self.pan.glide) != 0.0;
        let on_row = matches!(hit, None | Some(Id::Game(_))) && (BAND.0..BAND.1).contains(&y);
        if on_row && self.row_touchable() {
            self.pan.touch = Some(Touch {
                x0: x,
                scroll0: self.scroll,
                moved: false,
                samples: Vec::new(),
            });
        }
    }

    /// The finger moves: true once it pans the row (the tap is off).
    pub(crate) fn pan_move(&mut self, x: usize, now: u64) -> bool {
        let max = self.row_max() as f32;
        let Some(t) = &mut self.pan.touch else {
            return false;
        };
        if !t.moved {
            if (x as i64 - t.x0 as i64).abs() <= TOUCH_SLOP {
                return false;
            }
            // The row starts from where it is, not jumping by the slop.
            (t.moved, t.x0, t.scroll0) = (true, x, self.scroll);
            self.pan.free = true;
        }
        let s = (t.scroll0 - (x as f32 - t.x0 as f32)).clamp(0.0, max);
        self.scroll = s;
        self.scroll_to = s;
        t.samples.push((now, s));
        t.samples.retain(|k| now.saturating_sub(k.0) <= VEL_WINDOW_NS);
        true
    }

    /// The finger lifts: true if it panned, and the row glides on at the
    /// speed it had.
    pub(crate) fn pan_up(&mut self, now: u64) -> bool {
        let Some(t) = self.pan.touch.take().filter(|t| t.moved) else {
            return false;
        };
        self.pan.glide = flick_speed(&t.samples, now);
        self.pan.glide_at = now;
        true
    }

    /// The row's position while it is free: a glide advances, the games
    /// kept the row inside its ends.
    pub(crate) fn pan_step(&mut self, now: u64) {
        let max = self.row_max() as f32;
        self.scroll = self.scroll.clamp(0.0, max);
        if self.pan.touch.is_none() && self.pan.glide != 0.0 {
            let dt = now.saturating_sub(self.pan.glide_at).min(STEP_MAX_NS) as f32 / 1e9;
            self.pan.glide_at = now;
            let v = self.pan.glide;
            self.scroll = (self.scroll + v * dt).clamp(0.0, max);
            self.pan.glide = v - v * FRICTION * dt;
            let end = self.scroll <= 0.0 || self.scroll >= max;
            if end || self.pan.glide.abs() < GLIDE_MIN {
                self.pan.glide = 0.0;
            }
        }
        self.scroll_to = self.scroll;
    }

    /// A gamepad direction or A with the row left by hand: the selection
    /// takes the row back; if it is out of view it moves to the first game
    /// in view first (A then only does that, it never starts what the
    /// user did not see). True: the press is used.
    pub(crate) fn pan_pad(&mut self, code: u64) -> bool {
        use input::*;
        if !self.pan.free || !matches!(code, BTN_UP | BTN_DOWN | BTN_LEFT | BTN_RIGHT | BTN_A) {
            return false;
        }
        let (n, step, scroll) = (self.games.len(), ICON + GAP, self.scroll as usize);
        self.pan = Pan::default();
        if n == 0 {
            return false;
        }
        let g = self.last_game.min(n - 1);
        let left = X0 + g * step;
        if left >= scroll && left + ICON <= scroll + W {
            return false;
        }
        let first = (scroll.saturating_sub(X0).div_ceil(step)).min(n - 1);
        self.last_game = first;
        if self.home < n {
            self.home = first;
        }
        code == BTN_A && self.home < n
    }

    /// Furthest the row scrolls: its last icon ends as far from the right
    /// edge as the first starts from the left.
    pub(crate) fn row_max(&self) -> usize {
        let slots = self.games.len().max(MIN_SLOTS);
        let end = X0 + slots * ICON + (slots - 1) * GAP;
        end.saturating_sub(W - X0)
    }
}

/// Speed (px/s) of the last moves, 0 if they are too few or the finger
/// had stopped before it lifted. A big finger rolls as it lifts, so the
/// whole window counts, not the last two points.
fn flick_speed(samples: &[(u64, f32)], now: u64) -> f32 {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return 0.0;
    };
    let span = last.0 - first.0;
    if now.saturating_sub(last.0) > VEL_STALE_NS || span < VEL_MIN_SPAN_NS {
        return 0.0;
    }
    let v = (last.1 - first.1) / (span as f32 / 1e9);
    if v.abs() < GLIDE_MIN {
        0.0
    } else {
        v.clamp(-GLIDE_MAX, GLIDE_MAX)
    }
}

//! Analysis of a stream of 16-bit sequence numbers: losses, duplicates and reordering, in a sliding window.

/// Sequence numbers remembered behind the highest one seen: one second of packets at 4000 packets/s.
const WINDOW: usize = 4096;
const WORDS: usize = WINDOW / 64;

/// # What a sequence number turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// The highest number so far; `gap` numbers between it and the previous highest were skipped.
    Fresh { gap: u16 },
    /// Below the highest number, and not seen before: it filled a hole, `depth` numbers behind.
    Reordered { depth: u16 },
    /// Seen before.
    Duplicate,
    /// More than the window behind the highest number: not classified.
    TooLate,
}

/// # Counters of a [`SeqTracker`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqStats {
    /// Distinct sequence numbers received.
    pub unique: u64,
    pub duplicates: u64,
    /// Numbers that arrived after a higher one but filled a hole.
    pub reordered: u64,
    /// Largest distance, in numbers, between a reordered packet and the highest one.
    pub max_depth: u16,
    pub too_late: u64,
    /// Numbers between the first and the highest seen that never arrived (so far).
    pub lost: u64,
}

/// # Tracks 16-bit sequence numbers with wrap-around.
///
/// Memory is bounded: a bitmap of [`WINDOW`] numbers behind the highest one seen.
///
/// ## Example
/// ```
/// use lunacore::stream::reliability::{Arrival, SeqTracker};
/// let mut tracker = SeqTracker::default();
/// assert_eq!(tracker.observe(0xFFFF), Arrival::Fresh { gap: 0 });
/// assert_eq!(tracker.observe(1), Arrival::Fresh { gap: 1 });
/// assert_eq!(tracker.observe(0), Arrival::Reordered { depth: 1 });
/// assert_eq!(tracker.observe(0), Arrival::Duplicate);
/// assert_eq!(tracker.stats().lost, 0);
/// ```
#[derive(Debug, Clone)]
pub struct SeqTracker {
    started: bool,
    /// Highest number seen, extended past 16 bits.
    highest: u64,
    first: u64,
    bits: [u64; WORDS],
    stats: SeqStats,
}

impl Default for SeqTracker {
    fn default() -> Self {
        Self {
            started: false,
            highest: 0,
            first: 0,
            bits: [0; WORDS],
            stats: SeqStats::default(),
        }
    }
}

impl SeqTracker {
    /// # Records one received sequence number.
    pub fn observe(&mut self, seq: u16) -> Arrival {
        if !self.started {
            self.started = true;
            self.highest = u64::from(seq);
            self.first = self.highest;
            self.set(self.highest);
            self.stats.unique = 1;
            return Arrival::Fresh { gap: 0 };
        }
        let delta = seq.wrapping_sub(self.highest as u16).cast_signed();
        let arrival = if delta > 0 {
            self.advance(u64::from(delta.cast_unsigned()))
        } else {
            self.behind(u64::from(delta.unsigned_abs()))
        };
        self.stats.lost = (self.highest - self.first + 1).saturating_sub(self.stats.unique);
        arrival
    }

    /// # The counters so far.
    pub fn stats(&self) -> SeqStats {
        self.stats
    }

    /// # Numbers missing among the `len` consecutive ones starting at `start`.
    ///
    /// `None` when the range is not entirely known: it starts before the first number seen or beyond the window
    /// behind the highest, or it ends after the highest number seen.
    pub fn missing_in(&self, start: u16, len: u16) -> Option<u16> {
        if !self.started || len == 0 {
            return None;
        }
        let behind = u64::from((self.highest as u16).wrapping_sub(start));
        behind.checked_sub(u64::from(len) - 1)?;
        if behind >= WINDOW as u64 || behind > self.highest - self.first {
            return None;
        }
        let first_position = self.highest - behind;
        let missing = (0..u64::from(len))
            .filter(|offset| !self.get(first_position + offset))
            .count();
        u16::try_from(missing).ok()
    }

    fn advance(&mut self, by: u64) -> Arrival {
        let gap = by - 1;
        if by >= WINDOW as u64 {
            self.bits = [0; WORDS];
        } else {
            for offset in 1..=by {
                self.clear(self.highest + offset);
            }
        }
        self.highest += by;
        self.set(self.highest);
        self.stats.unique += 1;
        Arrival::Fresh {
            gap: u16::try_from(gap).unwrap_or(u16::MAX),
        }
    }

    fn behind(&mut self, by: u64) -> Arrival {
        if by >= WINDOW as u64 {
            self.stats.too_late += 1;
            return Arrival::TooLate;
        }
        let position = self.highest - by;
        if position < self.first {
            self.stats.too_late += 1;
            return Arrival::TooLate;
        }
        if self.get(position) {
            self.stats.duplicates += 1;
            return Arrival::Duplicate;
        }
        self.set(position);
        self.stats.unique += 1;
        self.stats.reordered += 1;
        let depth = u16::try_from(by).unwrap_or(u16::MAX);
        self.stats.max_depth = self.stats.max_depth.max(depth);
        Arrival::Reordered { depth }
    }

    fn set(&mut self, position: u64) {
        let slot = (position % WINDOW as u64) as usize;
        self.bits[slot / 64] |= 1 << (slot % 64);
    }

    fn clear(&mut self, position: u64) {
        let slot = (position % WINDOW as u64) as usize;
        self.bits[slot / 64] &= !(1 << (slot % 64));
    }

    fn get(&self, position: u64) -> bool {
        let slot = (position % WINDOW as u64) as usize;
        self.bits[slot / 64] & (1 << (slot % 64)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tracker: &mut SeqTracker, seqs: &[u16]) {
        for &seq in seqs {
            tracker.observe(seq);
        }
    }

    #[test]
    fn a_clean_run_has_no_loss() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &(0..1000).collect::<Vec<_>>());
        let stats = tracker.stats();
        assert_eq!((stats.unique, stats.lost, stats.duplicates), (1000, 0, 0));
    }

    #[test]
    fn a_skipped_number_is_lost_until_it_arrives_late() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[0, 1, 3, 4]);
        assert_eq!(tracker.stats().lost, 1);
        assert_eq!(tracker.observe(2), Arrival::Reordered { depth: 2 });
        let stats = tracker.stats();
        assert_eq!((stats.lost, stats.reordered, stats.max_depth), (0, 1, 2));
    }

    #[test]
    fn duplicates_are_counted_not_lost() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[5, 6, 6, 7, 5]);
        let stats = tracker.stats();
        assert_eq!((stats.unique, stats.duplicates, stats.lost), (3, 2, 0));
    }

    #[test]
    fn wrap_around_is_followed() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[0xFFFD, 0xFFFE, 0, 1]);
        let stats = tracker.stats();
        assert_eq!((stats.unique, stats.lost), (4, 1), "0xFFFF never arrived");
    }

    #[test]
    fn a_long_gap_clears_the_window() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[0, 1]);
        assert_eq!(tracker.observe(20_000), Arrival::Fresh { gap: 19_998 });
        assert_eq!(tracker.stats().lost, 19_998);
        assert_eq!(tracker.observe(1), Arrival::TooLate);
        assert_eq!(tracker.observe(19_999), Arrival::Reordered { depth: 1 });
    }

    #[test]
    fn missing_in_counts_the_holes_of_a_complete_range() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[0, 1, 3, 4, 6, 7]);
        assert_eq!(tracker.missing_in(0, 8), Some(2));
        assert_eq!(tracker.missing_in(2, 3), Some(1));
        assert_eq!(
            tracker.missing_in(5, 4),
            None,
            "ends after the highest number"
        );
        assert_eq!(tracker.missing_in(0, 0), None);
    }

    #[test]
    fn missing_in_refuses_ranges_it_cannot_know() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[100, 101, 102]);
        assert_eq!(tracker.missing_in(96, 4), None, "starts before the first");
        feed(&mut tracker, &[10_000]);
        assert_eq!(tracker.missing_in(100, 4), None, "out of the window");
    }

    #[test]
    fn missing_in_follows_the_wrap() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[0xFFFE, 0, 1]);
        assert_eq!(tracker.missing_in(0xFFFE, 4), Some(1));
    }

    #[test]
    fn numbers_before_the_first_are_not_classified() {
        let mut tracker = SeqTracker::default();
        feed(&mut tracker, &[100, 101]);
        assert_eq!(tracker.observe(50), Arrival::TooLate);
        assert_eq!(tracker.stats().lost, 0);
    }
}

//! In-order release of numbered items with a bounded wait for the missing ones.
//!
//! The server retransmits a lost packet about 50 ms after sending it (`docs/PROTOCOL.md` §2.4), so a message whose
//! fragment was lost completes some 65 ms late, after the messages that follow it. Releasing them as they complete
//! makes the consumer see a hole; holding them for a short time lets the late one slot back into place.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Items waiting behind a gap, at most: further than this from the next expected number, the sender and receiver are
/// considered out of step and the gap is abandoned.
pub const WINDOW: usize = 256;
/// Sequence numbers up to this value at the start of a channel are taken to follow a first number 0.
const START_NUMBERS: u16 = 8;

/// # What happened to an item given to [`OrderBuffer::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Released now, with any items it unblocked (all in `out`).
    Released,
    /// Held behind a gap, until the missing numbers arrive or the budget runs out.
    Held,
    /// Behind the next expected number: the gap was already abandoned.
    Late,
    /// The same number was already held.
    Duplicate,
}

/// # Counters of an [`OrderBuffer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrderStats {
    /// Items that waited for a gap to close and were released in order once the missing ones arrived.
    pub recovered: u64,
    /// Items that waited behind a gap that was finally given up on.
    pub delayed: u64,
    /// Numbers given up on after the budget (or the window, or the byte cap) ran out.
    pub skipped: u64,
    pub late: u64,
    /// Longest an item was held, microseconds.
    pub longest_hold_us: u64,
}

/// # Releases items in sequence order, waiting at most a budget for a gap to close.
///
/// Sequence numbers are 16-bit with wrap-around. Memory is bounded by [`WINDOW`] items and `max_bytes` bytes held.
///
/// ## Example
/// ```
/// use lunacore::stream::reliability::OrderBuffer;
/// use std::time::{Duration, Instant};
/// let mut buffer = OrderBuffer::new(Duration::from_millis(100), 1 << 20);
/// let (mut out, now) = (Vec::new(), Instant::now());
/// buffer.insert(0, 0u16, 10, now, &mut out);
/// buffer.insert(2, 2, 10, now, &mut out); // 1 is missing: 2 waits
/// assert_eq!(out, [0]);
/// buffer.insert(1, 1, 10, now, &mut out); // 1 arrives: both are released
/// assert_eq!(out, [0, 1, 2]);
/// ```
#[derive(Debug)]
pub struct OrderBuffer<T> {
    budget: Duration,
    max_bytes: usize,
    next: Option<u16>,
    /// Slot `i` is sequence number `next + i`; the front slot is empty while a gap is open.
    slots: VecDeque<Option<Held<T>>>,
    held_bytes: usize,
    stats: OrderStats,
}

#[derive(Debug)]
struct Held<T> {
    item: T,
    size: usize,
    since: Instant,
}

impl<T> OrderBuffer<T> {
    /// # An empty buffer.
    ///
    /// ## Arguments
    /// * `budget` - longest an item is held for a missing predecessor
    /// * `max_bytes` - largest total size of the items held; beyond it the oldest gap is abandoned
    pub fn new(budget: Duration, max_bytes: usize) -> Self {
        Self {
            budget,
            max_bytes,
            next: None,
            slots: VecDeque::new(),
            held_bytes: 0,
            stats: OrderStats::default(),
        }
    }

    pub fn stats(&self) -> OrderStats {
        self.stats
    }

    /// # The next number to be released, once one has been seen.
    pub fn next_expected(&self) -> Option<u16> {
        self.next
    }

    /// # Offers item number `seq` of `size` bytes; released items are appended to `out` in order.
    pub fn insert(
        &mut self,
        seq: u16,
        item: T,
        size: usize,
        now: Instant,
        out: &mut Vec<T>,
    ) -> Inserted {
        let next = *self
            .next
            .get_or_insert(if seq <= START_NUMBERS { 0 } else { seq });
        let distance = seq.wrapping_sub(next).cast_signed();
        if distance < 0 {
            self.stats.late += 1;
            return Inserted::Late;
        }
        let distance = usize::from(distance.cast_unsigned());
        if distance >= WINDOW {
            self.abandon_all(now, out);
            self.next = Some(seq);
            return self.insert(seq, item, size, now, out);
        }
        if self.slots.len() <= distance {
            self.slots.resize_with(distance + 1, || None);
        }
        if self.slots[distance].is_some() {
            return Inserted::Duplicate;
        }
        self.slots[distance] = Some(Held {
            item,
            size,
            since: now,
        });
        self.held_bytes += size;
        let held_before = distance > 0;
        self.release_front(now, out, true);
        while self.held_bytes > self.max_bytes && self.skip_to_oldest(now, out) {}
        if held_before && self.is_held(seq) {
            Inserted::Held
        } else {
            Inserted::Released
        }
    }

    /// # Abandons the gaps that have been open for a whole budget; released items are appended to `out`.
    ///
    /// ## Returns
    /// The number of sequence numbers given up on.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<T>) -> usize {
        let before = self.stats.skipped;
        while self.deadline().is_some_and(|deadline| deadline <= now) {
            self.skip_to_oldest(now, out);
        }
        (self.stats.skipped - before) as usize
    }

    /// # When the oldest gap has to be abandoned, if one is open.
    pub fn deadline(&self) -> Option<Instant> {
        if !matches!(self.slots.front(), Some(None)) {
            return None;
        }
        self.slots
            .iter()
            .flatten()
            .map(|held| held.since)
            .min()
            .map(|oldest| oldest + self.budget)
    }

    /// # Whether `seq` was already released or given up on (it is behind the next expected number), or is held.
    pub fn is_closed(&self, seq: u16) -> bool {
        let Some(next) = self.next else { return false };
        seq.wrapping_sub(next).cast_signed() < 0 || self.is_held(seq)
    }

    fn is_held(&self, seq: u16) -> bool {
        let Some(next) = self.next else { return false };
        let distance = usize::from(seq.wrapping_sub(next));
        matches!(self.slots.get(distance), Some(Some(_)))
    }

    /// Releases the items at the front of the line, as long as there is no gap.
    fn release_front(&mut self, now: Instant, out: &mut Vec<T>, gap_closed: bool) {
        while let Some(Some(_)) = self.slots.front() {
            let Some(Some(held)) = self.slots.pop_front() else {
                break;
            };
            let waited = now.saturating_duration_since(held.since);
            if !waited.is_zero() {
                if gap_closed {
                    self.stats.recovered += 1;
                } else {
                    self.stats.delayed += 1;
                }
                let us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
                self.stats.longest_hold_us = self.stats.longest_hold_us.max(us);
            }
            self.held_bytes -= held.size;
            self.next = self.next.map(|n| n.wrapping_add(1));
            out.push(held.item);
        }
        if self.slots.is_empty() {
            self.held_bytes = 0;
        }
    }

    /// Gives up on the numbers before the first held item. `false` when nothing is held.
    fn skip_to_oldest(&mut self, now: Instant, out: &mut Vec<T>) -> bool {
        let Some(gap) = self.slots.iter().position(Option::is_some) else {
            return false;
        };
        self.stats.skipped += gap as u64;
        self.slots.drain(..gap);
        self.next = self.next.map(|n| n.wrapping_add(gap as u16));
        self.release_front(now, out, false);
        true
    }

    /// Releases everything held, in order, giving up on every gap.
    fn abandon_all(&mut self, now: Instant, out: &mut Vec<T>) {
        while self.skip_to_oldest(now, out) {}
        self.slots.clear();
        self.held_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_millis(100);

    fn buffer() -> OrderBuffer<u16> {
        OrderBuffer::new(BUDGET, 1 << 20)
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn feed(buffer: &mut OrderBuffer<u16>, base: Instant, steps: &[(u16, u64)]) -> Vec<u16> {
        let mut out = Vec::new();
        for &(seq, ms) in steps {
            buffer.insert(seq, seq, 10, at(base, ms), &mut out);
        }
        out
    }

    #[test]
    fn items_in_order_pass_straight_through() {
        let (mut b, base) = (buffer(), Instant::now());
        assert_eq!(feed(&mut b, base, &[(0, 0), (1, 1), (2, 2)]), [0, 1, 2]);
        assert_eq!(b.stats(), OrderStats::default());
    }

    #[test]
    fn a_late_item_slots_back_into_place() {
        let (mut b, base) = (buffer(), Instant::now());
        let out = feed(&mut b, base, &[(0, 0), (2, 16), (3, 33), (1, 66)]);
        assert_eq!(out, [0, 1, 2, 3]);
        let stats = b.stats();
        assert_eq!((stats.recovered, stats.skipped), (2, 0));
        assert_eq!(stats.longest_hold_us, 50_000);
    }

    #[test]
    fn a_gap_is_abandoned_after_the_budget() {
        let (mut b, base) = (buffer(), Instant::now());
        let mut out = Vec::new();
        feed(&mut b, base, &[(0, 0)]);
        b.insert(2, 2, 10, at(base, 16), &mut out);
        b.insert(3, 3, 10, at(base, 33), &mut out);
        assert!(out.is_empty());
        assert_eq!(b.deadline(), Some(at(base, 116)));
        assert_eq!(b.expire(at(base, 115), &mut out), 0);
        assert_eq!(b.expire(at(base, 116), &mut out), 1);
        assert_eq!(out, [2, 3]);
        assert_eq!(b.deadline(), None);
        assert_eq!(b.stats().skipped, 1);
    }

    #[test]
    fn a_number_that_comes_after_its_gap_was_abandoned_is_late() {
        let (mut b, base) = (buffer(), Instant::now());
        let mut out = Vec::new();
        feed(&mut b, base, &[(0, 0), (2, 10)]);
        b.expire(at(base, 200), &mut out);
        assert_eq!(b.insert(1, 1, 10, at(base, 210), &mut out), Inserted::Late);
        assert_eq!(b.stats().late, 1);
    }

    #[test]
    fn duplicates_of_held_items_are_refused() {
        let (mut b, base) = (buffer(), Instant::now());
        let mut out = Vec::new();
        feed(&mut b, base, &[(0, 0), (2, 10)]);
        assert_eq!(
            b.insert(2, 2, 10, at(base, 11), &mut out),
            Inserted::Duplicate
        );
    }

    #[test]
    fn several_gaps_are_abandoned_one_after_the_other() {
        let (mut b, base) = (buffer(), Instant::now());
        let mut out = Vec::new();
        feed(&mut b, base, &[(0, 0), (2, 10), (4, 20)]);
        assert_eq!(b.expire(at(base, 110), &mut out), 1);
        assert_eq!(out, [2]);
        assert_eq!(b.expire(at(base, 120), &mut out), 1);
        assert_eq!(out, [2, 4]);
    }

    #[test]
    fn wrap_around_is_followed() {
        let (mut b, base) = (buffer(), Instant::now());
        let out = feed(&mut b, base, &[(0xFFFE, 0), (1, 1), (0xFFFF, 2), (0, 3)]);
        assert_eq!(out, [0xFFFE, 0xFFFF, 0, 1]);
    }

    #[test]
    fn the_first_number_seen_sets_the_start_unless_it_is_small() {
        let (mut b, base) = (buffer(), Instant::now());
        assert_eq!(feed(&mut b, base, &[(5000, 0), (5001, 1)]), [5000, 5001]);
        let (mut b, base) = (buffer(), Instant::now());
        assert_eq!(
            feed(&mut b, base, &[(1, 0), (0, 1)]),
            [0, 1],
            "0 was only reordered"
        );
    }

    #[test]
    fn a_number_far_ahead_resynchronises() {
        let (mut b, base) = (buffer(), Instant::now());
        let out = feed(&mut b, base, &[(0, 0), (2, 1), (1000, 2)]);
        assert_eq!(out, [0, 2, 1000]);
        assert_eq!(b.next_expected(), Some(1001));
    }

    #[test]
    fn held_memory_is_capped_by_bytes() {
        let mut b = OrderBuffer::new(BUDGET, 25);
        let (base, mut out) = (Instant::now(), Vec::new());
        b.insert(0, 0u16, 10, base, &mut out);
        b.insert(2, 2, 10, base, &mut out);
        b.insert(3, 3, 10, base, &mut out);
        b.insert(4, 4, 10, base, &mut out);
        assert_eq!(out, [0, 2, 3, 4], "the gap before 2 is abandoned at once");
    }
}

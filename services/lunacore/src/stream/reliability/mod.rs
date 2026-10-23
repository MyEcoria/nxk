//! Ordering of a channel's messages and sequence numbers, and what the
//! reassembler counts per channel.

mod order;
mod seq;

pub use order::{Inserted, OrderBuffer, OrderStats, WINDOW as ORDER_WINDOW};
pub use seq::{Arrival, SeqStats, SeqTracker};

/// # Messages of one channel as the reassembler saw them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageCounters {
    pub completed: u64,
    /// Messages given up on with fragments missing.
    pub abandoned: u64,
    /// Fragments missing from the abandoned messages, estimated from the largest fragment seen.
    pub missing_fragments: u64,
    /// Fragments that arrived for a message already delivered or abandoned.
    pub late_fragments: u64,
    /// Time from the first fragment to completion of multi-fragment messages, microseconds.
    pub assembly_total_us: u64,
    pub assembly_max_us: u64,
    pub assembled: u64,
    /// Messages that waited behind a gap and were released in order once it closed (retransmission worked).
    pub recovered: u64,
    /// Messages that waited behind a gap that was finally given up on.
    pub delayed: u64,
    /// Longest wait of such a message, microseconds.
    pub longest_hold_us: u64,
}

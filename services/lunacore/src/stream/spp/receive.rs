//! Receiving side added by task E: reassembly of fragmented messages, and the control traffic every client must
//! service (acknowledgements, time sync, channel confirmations, connection close). See `docs/PROTOCOL.md` §2.3-§2.5.
//!
//! Task N added bounded in-order release: the server retransmits what is not acknowledged within about 50 ms, so a
//! message that lost a fragment completes late. Messages of the application channels are held for at most
//! [`ORDER_BUDGET`] while a predecessor is missing, then the gap is abandoned (`reliability::OrderBuffer`).

use super::header::{pt, Packet, MAX_CHANNEL, MAX_RX_PAYLOAD};
use crate::stream::reliability::{MessageCounters, OrderBuffer};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Largest message length the receiver accepts (`spp_receive_buffer_receive` refuses anything from 100 000 000).
pub const MAX_MESSAGE_LEN: usize = 100_000_000;
/// Messages being reassembled per channel; older ones are dropped.
const MAX_PENDING: usize = 16;
/// Delivered message numbers remembered per channel when delivery is immediate, to drop retransmitted copies.
const DELIVERED_MEMORY: usize = 128;
/// Abandoned message numbers remembered per channel when delivery is immediate, to count their late fragments.
const ABANDONED_MEMORY: usize = 128;
/// How long a complete message waits for an earlier one that is missing. The server retransmits a packet it has not
/// seen acknowledged once, about 50 ms after sending it (`docs/PROTOCOL.md` §2.4): measured live, the copy arrived
/// 56 to 62 ms after the hole was noticed on a quiet link and at most 98 ms under 1 % injected burst loss, never
/// later. 120 ms covers that with a margin and bounds the delay added when the copy is lost too.
pub const ORDER_BUDGET: Duration = Duration::from_millis(120);
/// Largest total size of the complete messages held back behind a gap, per channel.
const ORDER_MAX_BYTES: usize = 16 << 20;

/// # A complete application message of one channel, padding and trailer removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub channel: u8,
    pub bytes: Vec<u8>,
}

/// # What the server sent that the session layer has to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A complete `PT_DATA` message.
    Message(Message),
    /// `PT_CHANNEL_CONFIRM` on `channel`; `in_order` is bit 0 of its single byte.
    ChannelConfirmed { channel: u8, in_order: bool },
    /// `PT_CONN_CLOSE`: the server ends the SPP connection.
    Closed,
    /// `PT_RTCP`: the server's target bitrate (unit not established) and its timestamp.
    TargetBitrate { rate: u32, timestamp: u32 },
}

/// A message being reassembled: fragments are stored by their offset from the first one seen.
#[derive(Debug)]
struct Partial {
    message_len: usize,
    base_seq: u16,
    fragments: BTreeMap<i16, Vec<u8>>,
    /// Bytes held in `fragments`.
    held: usize,
    started: Instant,
}

impl Partial {
    fn new(message_len: usize, base_seq: u16, now: Instant) -> Self {
        Self {
            message_len,
            base_seq,
            fragments: BTreeMap::new(),
            held: 0,
            started: now,
        }
    }

    /// Fragments still missing, judged by the largest one seen (all but the last are full).
    fn missing_fragments(&self) -> u64 {
        let largest = self.fragments.values().map(Vec::len).max().unwrap_or(0);
        if largest == 0 {
            return 0;
        }
        let expected = self.message_len.div_ceil(largest);
        expected.saturating_sub(self.fragments.len()) as u64
    }

    /// Stores a fragment; a copy of one already held is ignored, and so is anything beyond the size the message
    /// can have (its length plus the padding and trailer of the last fragment), which bounds the memory a sender
    /// can make us hold for one message.
    fn insert(&mut self, packet_seq: u16, payload: &[u8]) {
        let offset = packet_seq.wrapping_sub(self.base_seq).cast_signed();
        if self.held + payload.len() > self.message_len + MAX_RX_PAYLOAD {
            return;
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = self.fragments.entry(offset) {
            slot.insert(payload.to_vec());
            self.held += payload.len();
        }
    }

    /// The message once the consecutive fragments hold at least `message_len` bytes.
    fn complete(&self) -> Option<Vec<u8>> {
        let first = *self.fragments.keys().next()?;
        let last = *self.fragments.keys().next_back()?;
        let consecutive = usize::try_from(i32::from(last) - i32::from(first) + 1).ok()?;
        if consecutive != self.fragments.len() {
            return None;
        }
        let held: usize = self.fragments.values().map(Vec::len).sum();
        if held < self.message_len {
            return None;
        }
        let mut message: Vec<u8> = self.fragments.values().flatten().copied().collect();
        message.truncate(self.message_len);
        Some(message)
    }
}

#[derive(Debug, Default)]
struct ChannelReassembly {
    pending: HashMap<u16, Partial>,
    /// Insertion order of `pending`, oldest first.
    order: VecDeque<u16>,
    delivered: VecDeque<u16>,
    abandoned: VecDeque<u16>,
    counters: MessageCounters,
    /// Present when messages are released in order.
    ordering: Option<OrderBuffer<Vec<u8>>>,
}

impl ChannelReassembly {
    fn ordered(budget: Duration) -> Self {
        Self {
            ordering: Some(OrderBuffer::new(budget, ORDER_MAX_BYTES)),
            ..Self::default()
        }
    }

    fn add(&mut self, packet: &Packet, now: Instant, out: &mut Vec<Vec<u8>>) {
        let header = &packet.header;
        let Ok(message_len) = usize::try_from(header.message_len) else {
            return;
        };
        if message_len > MAX_MESSAGE_LEN {
            return;
        }
        let seq = header.message_seq;
        if self.is_closed(seq) {
            self.counters.late_fragments += 1;
            return;
        }
        if packet.payload.len() >= message_len {
            self.finish(seq, packet.payload[..message_len].to_vec(), None, now, out);
            return;
        }
        if !self.pending.contains_key(&seq) {
            self.evict_if_full();
            self.order.push_back(seq);
        }
        let partial = self
            .pending
            .entry(seq)
            .or_insert_with(|| Partial::new(message_len, header.packet_seq, now));
        if partial.message_len != message_len {
            return;
        }
        partial.insert(header.packet_seq, &packet.payload);
        let Some(message) = partial.complete() else {
            return;
        };
        let assembly = now.saturating_duration_since(partial.started);
        self.pending.remove(&seq);
        self.order.retain(|s| *s != seq);
        self.finish(seq, message, Some(assembly), now, out);
    }

    /// Whether message `seq` was already delivered or given up on.
    fn is_closed(&self, seq: u16) -> bool {
        match &self.ordering {
            Some(ordering) => ordering.is_closed(seq),
            None => self.delivered.contains(&seq) || self.abandoned.contains(&seq),
        }
    }

    /// Hands a complete message to the consumer, now or when its predecessors are there.
    fn finish(
        &mut self,
        seq: u16,
        message: Vec<u8>,
        assembly: Option<Duration>,
        now: Instant,
        out: &mut Vec<Vec<u8>>,
    ) {
        self.counters.completed += 1;
        if let Some(assembly) = assembly {
            let us = u64::try_from(assembly.as_micros()).unwrap_or(u64::MAX);
            self.counters.assembled += 1;
            self.counters.assembly_total_us += us;
            self.counters.assembly_max_us = self.counters.assembly_max_us.max(us);
        }
        match &mut self.ordering {
            Some(ordering) => {
                let size = message.len();
                ordering.insert(seq, message, size, now, out);
                self.after_ordering();
            }
            None => {
                self.remember(seq);
                out.push(message);
            }
        }
    }

    /// Releases the messages whose wait is over.
    fn expire(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        if let Some(ordering) = &mut self.ordering {
            ordering.expire(now, out);
            self.after_ordering();
        }
    }

    /// When the oldest gap has to be given up.
    fn deadline(&self) -> Option<Instant> {
        self.ordering.as_ref()?.deadline()
    }

    /// Drops the partial messages that ordering gave up on, and copies the ordering counters.
    fn after_ordering(&mut self) {
        let Some(ordering) = &self.ordering else {
            return;
        };
        let stats = ordering.stats();
        self.counters.abandoned = stats.skipped;
        self.counters.recovered = stats.recovered;
        self.counters.delayed = stats.delayed;
        self.counters.longest_hold_us = stats.longest_hold_us;
        let stale: Vec<u16> = self
            .pending
            .keys()
            .copied()
            .filter(|seq| ordering.is_closed(*seq))
            .collect();
        for seq in stale {
            if let Some(partial) = self.pending.remove(&seq) {
                self.counters.missing_fragments += partial.missing_fragments();
            }
            self.order.retain(|s| *s != seq);
        }
    }

    fn evict_if_full(&mut self) {
        while self.order.len() >= MAX_PENDING {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(partial) = self.pending.remove(&oldest) {
                self.counters.missing_fragments += partial.missing_fragments();
            }
            if self.ordering.is_none() {
                self.counters.abandoned += 1;
                if self.abandoned.len() >= ABANDONED_MEMORY {
                    self.abandoned.pop_front();
                }
                self.abandoned.push_back(oldest);
            }
        }
    }

    fn remember(&mut self, message_seq: u16) {
        if self.delivered.len() >= DELIVERED_MEMORY {
            self.delivered.pop_front();
        }
        self.delivered.push_back(message_seq);
    }
}

/// # Reassembles the `PT_DATA` messages of every channel.
///
/// Fragments may arrive in any order and more than once: the server retransmits unacknowledged packets, and
/// copies of a message already delivered are dropped. A message with a missing fragment is never delivered.
/// [`Reassembler::default`] delivers messages as soon as they are complete; [`Reassembler::ordered`] releases those
/// of the application channels in message order, holding them for a missing predecessor for a bounded time.
#[derive(Debug)]
pub struct Reassembler {
    channels: [ChannelReassembly; MAX_CHANNEL as usize + 1],
}

impl Default for Reassembler {
    fn default() -> Self {
        Self {
            channels: std::array::from_fn(|_| ChannelReassembly::default()),
        }
    }
}

impl Reassembler {
    /// # In-order release on the application channels (1 to 7), each waiting at most `budget` for a gap.
    pub fn ordered(budget: Duration) -> Self {
        Self {
            channels: std::array::from_fn(|channel| {
                if channel == 0 {
                    ChannelReassembly::default()
                } else {
                    ChannelReassembly::ordered(budget)
                }
            }),
        }
    }

    /// # Feeds one `PT_DATA` packet; returns the message when this packet completes it.
    ///
    /// Meant for immediate delivery: with ordering, a packet can release several messages or none, use
    /// [`Reassembler::push_at`].
    ///
    /// ## Example
    /// ```
    /// use lunacore::stream::spp::{Header, Packet, Reassembler};
    /// let header = |seq: u16, len: u16| Header {
    ///     payload_len: len, channel: 1, packet_seq: seq, message_len: 5, ..Header::default()
    /// };
    /// let mut reassembler = Reassembler::default();
    /// let first = Packet { header: header(0, 3), payload: vec![1, 2, 3] };
    /// let last = Packet { header: header(1, 2), payload: vec![4, 5] };
    /// assert_eq!(reassembler.push(&first), None);
    /// assert_eq!(reassembler.push(&last), Some(vec![1, 2, 3, 4, 5]));
    /// ```
    pub fn push(&mut self, packet: &Packet) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.push_at(packet, Instant::now(), &mut out);
        out.into_iter().next().map(|message| message.bytes)
    }

    /// # Feeds one `PT_DATA` packet at time `now`; released messages are appended to `out`.
    pub fn push_at(&mut self, packet: &Packet, now: Instant, out: &mut Vec<Message>) {
        let channel = packet.header.channel;
        let Some(state) = self.channels.get_mut(usize::from(channel)) else {
            return;
        };
        let mut released = Vec::new();
        state.add(packet, now, &mut released);
        out.extend(released.into_iter().map(|bytes| Message { channel, bytes }));
    }

    /// # Gives up on the gaps that have waited a whole budget; released messages are appended to `out`.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<Message>) {
        for (channel, state) in self.channels.iter_mut().enumerate() {
            let mut released = Vec::new();
            state.expire(now, &mut released);
            let channel = channel as u8;
            out.extend(released.into_iter().map(|bytes| Message { channel, bytes }));
        }
    }

    /// # When the next gap has to be given up, if any is open.
    pub fn deadline(&self) -> Option<Instant> {
        self.channels
            .iter()
            .filter_map(ChannelReassembly::deadline)
            .min()
    }

    /// # Counters of the messages of `channel` so far.
    pub fn counters(&self, channel: u8) -> MessageCounters {
        self.channels
            .get(usize::from(channel))
            .map_or_else(MessageCounters::default, |c| c.counters)
    }
}

/// # Receive-side state of the event loop: the reassembler and the events not yet handed out.
#[derive(Debug)]
pub struct Receiver {
    reassembler: Reassembler,
    queue: VecDeque<Event>,
}

impl Default for Receiver {
    /// Application channels are released in order, waiting at most [`ORDER_BUDGET`] for a missing message.
    fn default() -> Self {
        Self::with_reassembler(Reassembler::ordered(ORDER_BUDGET))
    }
}

impl Receiver {
    /// # A receiver with a given reassembler, e.g. [`Reassembler::default`] for immediate delivery.
    pub fn with_reassembler(reassembler: Reassembler) -> Self {
        Self {
            reassembler,
            queue: VecDeque::new(),
        }
    }

    /// # Counters of the messages of `channel` so far.
    pub fn counters(&self, channel: u8) -> MessageCounters {
        self.reassembler.counters(channel)
    }

    /// # The next event produced by earlier packets, if any.
    pub fn pop(&mut self) -> Option<Event> {
        self.queue.pop_front()
    }

    /// # When the next gap has to be given up, if any is open.
    pub fn deadline(&self) -> Option<Instant> {
        self.reassembler.deadline()
    }

    /// # Gives up on the gaps that have waited long enough; the released messages become events for [`Receiver::pop`].
    pub fn expire(&mut self, now: Instant) {
        let mut released = Vec::new();
        self.reassembler.expire(now, &mut released);
        self.queue.extend(released.into_iter().map(Event::Message));
    }

    /// # Turns a packet that needs no reply into events; returns the first, the others wait for [`Receiver::pop`].
    ///
    /// Acknowledgements, FEC, diagnostics and time requests produce no event: the caller has already answered them.
    /// With ordering, a data packet can produce several events or none.
    pub fn classify(&mut self, packet: &Packet) -> Option<Event> {
        self.classify_at(packet, Instant::now())
    }

    /// # [`Receiver::classify`] at a given time.
    pub fn classify_at(&mut self, packet: &Packet, now: Instant) -> Option<Event> {
        let header = &packet.header;
        match header.payload_type {
            pt::DATA => {
                let mut released = Vec::new();
                self.reassembler.push_at(packet, now, &mut released);
                self.queue.extend(released.into_iter().map(Event::Message));
            }
            pt::CHANNEL_CONFIRM => {
                let flags = *packet.single_fragment_message()?.first()?;
                self.queue.push_back(Event::ChannelConfirmed {
                    channel: header.channel,
                    in_order: flags & 1 != 0,
                });
            }
            pt::CONN_CLOSE => self.queue.push_back(Event::Closed),
            pt::RTCP => {
                let body = packet.single_fragment_message()?;
                let rate = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
                let timestamp = u32::from_le_bytes(body.get(4..8)?.try_into().ok()?);
                self.queue
                    .push_back(Event::TargetBitrate { rate, timestamp });
            }
            _ => {}
        }
        self.queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::spp::Header;

    fn fragment(
        channel: u8,
        message_seq: u16,
        packet_seq: u16,
        total: u32,
        bytes: &[u8],
    ) -> Packet {
        Packet {
            header: Header {
                payload_len: u16::try_from(bytes.len()).unwrap(),
                channel,
                packet_seq,
                message_seq,
                message_len: total,
                ..Header::default()
            },
            payload: bytes.to_vec(),
        }
    }

    fn pieces(message: &[u8], size: usize) -> Vec<Packet> {
        message
            .chunks(size)
            .enumerate()
            .map(|(i, c)| fragment(1, 7, u16::try_from(i).unwrap(), message.len() as u32, c))
            .collect()
    }

    fn message(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    #[test]
    fn a_single_fragment_is_cut_at_the_message_length() {
        let mut padded = vec![9, 8, 7, 0];
        padded.extend_from_slice(&[0xAA; 8]);
        let packet = fragment(1, 0, 0, 3, &padded);
        assert_eq!(Reassembler::default().push(&packet), Some(vec![9, 8, 7]));
    }

    #[test]
    fn fragments_in_order_make_the_message() {
        let data = message(3000);
        let mut reassembler = Reassembler::default();
        let results: Vec<_> = pieces(&data, 1000)
            .iter()
            .map(|p| reassembler.push(p))
            .collect();
        assert_eq!(results[..2], [None, None]);
        assert_eq!(results[2], Some(data));
    }

    #[test]
    fn reordered_and_duplicated_fragments_make_the_same_message() {
        let data = message(2500);
        let parts = pieces(&data, 1000);
        let mut reassembler = Reassembler::default();
        let mut delivered = Vec::new();
        for i in [2, 0, 0, 2, 1, 1] {
            delivered.extend(reassembler.push(&parts[i]));
        }
        assert_eq!(delivered, vec![data]);
    }

    #[test]
    fn the_last_fragment_may_carry_padding_and_a_trailer() {
        let data = message(1030);
        let mut parts = pieces(&data, 984);
        parts[1].payload.extend_from_slice(&[0; 10]);
        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(&parts[0]), None);
        assert_eq!(reassembler.push(&parts[1]), Some(data));
    }

    #[test]
    fn a_retransmitted_message_is_delivered_once() {
        let data = message(2500);
        let parts = pieces(&data, 1000);
        let mut reassembler = Reassembler::default();
        let count = parts
            .iter()
            .chain(parts.iter())
            .filter_map(|p| reassembler.push(p))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_retransmitted_single_fragment_message_is_delivered_once() {
        let packet = fragment(2, 3, 5, 2, &[1, 2]);
        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(&packet), Some(vec![1, 2]));
        assert_eq!(reassembler.push(&packet), None);
    }

    #[test]
    fn a_missing_fragment_blocks_the_message() {
        let data = message(3000);
        let parts = pieces(&data, 1000);
        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(&parts[0]), None);
        assert_eq!(reassembler.push(&parts[2]), None);
    }

    #[test]
    fn channels_do_not_mix() {
        let a = pieces(&message(1500), 1000);
        let mut b = pieces(&message(1500), 1000);
        b.iter_mut().for_each(|p| p.header.channel = 2);
        let mut reassembler = Reassembler::default();
        assert_eq!(reassembler.push(&a[0]), None);
        assert_eq!(reassembler.push(&b[0]), None);
        assert!(reassembler.push(&b[1]).is_some());
        assert!(reassembler.push(&a[1]).is_some());
    }

    #[test]
    fn sequence_numbers_may_wrap_during_a_message() {
        let data = message(3000);
        let mut parts = pieces(&data, 1000);
        for (packet, seq) in parts.iter_mut().zip([0xFFFF, 0, 1]) {
            packet.header.packet_seq = seq;
        }
        let mut reassembler = Reassembler::default();
        let delivered: Vec<_> = [2, 1, 0]
            .iter()
            .filter_map(|&i| reassembler.push(&parts[i]))
            .collect();
        assert_eq!(delivered, vec![data]);
    }

    #[test]
    fn oversized_messages_and_fragment_floods_are_bounded() {
        let mut reassembler = Reassembler::default();
        let huge = fragment(
            1,
            0,
            0,
            u32::try_from(MAX_MESSAGE_LEN).unwrap() + 1,
            &[0; 10],
        );
        assert_eq!(reassembler.push(&huge), None);
        for seq in 0..100 {
            reassembler.push(&fragment(1, seq, 0, 5000, &[0; 10]));
        }
        assert!(reassembler.channels[1].pending.len() <= MAX_PENDING);
    }

    const SHORT: Duration = Duration::from_millis(100);

    #[test]
    fn fragments_beyond_the_size_of_the_message_are_not_kept() {
        let mut r = Reassembler::default();
        // a message claiming 3000 bytes, flooded with distinct fragments that never make it complete
        for seq in (0..2000u16).step_by(2) {
            r.push(&fragment(1, 0, seq, 3000, &[0; 1000]));
        }
        let held = r.channels[1].pending.get(&0).map_or(0, |p| p.held);
        assert!(held <= 3000 + MAX_RX_PAYLOAD, "held {held} bytes");
    }

    /// Fragments of message `message_seq` of channel 1, `packet_seq` numbered from `first_packet`.
    fn message_pieces(message_seq: u16, first_packet: u16, bytes: &[u8]) -> Vec<Packet> {
        bytes
            .chunks(1000)
            .enumerate()
            .map(|(i, c)| {
                fragment(
                    1,
                    message_seq,
                    first_packet + u16::try_from(i).unwrap(),
                    bytes.len() as u32,
                    c,
                )
            })
            .collect()
    }

    fn push(r: &mut Reassembler, packet: &Packet, now: Instant) -> Vec<Message> {
        let mut out = Vec::new();
        r.push_at(packet, now, &mut out);
        out
    }

    #[test]
    fn a_message_that_lost_a_fragment_holds_back_the_next_until_it_is_retransmitted() {
        let (first, second) = (message(2500), message(900));
        let (a, b) = (message_pieces(0, 0, &first), message_pieces(1, 3, &second));
        let (mut r, t0) = (Reassembler::ordered(SHORT), Instant::now());
        assert!(push(&mut r, &a[0], t0).is_empty());
        assert!(push(&mut r, &a[2], t0).is_empty());
        assert!(push(&mut r, &b[0], t0).is_empty(), "held behind message 0");
        let released = push(&mut r, &a[1], t0 + Duration::from_millis(65));
        let bytes: Vec<_> = released.iter().map(|m| &m.bytes).collect();
        assert_eq!(bytes, [&first, &second]);
        let counters = r.counters(1);
        assert_eq!((counters.completed, counters.abandoned), (2, 0));
        assert_eq!(counters.recovered, 1);
        assert_eq!(counters.longest_hold_us, 65_000);
    }

    #[test]
    fn a_message_that_never_completes_is_given_up_after_the_budget() {
        let (first, second) = (message(2500), message(900));
        let (a, b) = (message_pieces(0, 0, &first), message_pieces(1, 3, &second));
        let (mut r, t0) = (Reassembler::ordered(SHORT), Instant::now());
        push(&mut r, &a[0], t0);
        push(&mut r, &b[0], t0 + Duration::from_millis(10));
        assert_eq!(r.deadline(), Some(t0 + Duration::from_millis(110)));
        let mut out = Vec::new();
        r.expire(t0 + Duration::from_millis(109), &mut out);
        assert!(out.is_empty());
        r.expire(t0 + Duration::from_millis(110), &mut out);
        assert_eq!(
            out,
            [Message {
                channel: 1,
                bytes: second
            }]
        );
        let counters = r.counters(1);
        assert_eq!(counters.abandoned, 1);
        assert_eq!(
            counters.missing_fragments, 2,
            "two of three fragments never came"
        );
        assert_eq!(r.deadline(), None);
        // the retransmission comes too late: it is counted, not delivered
        assert!(push(&mut r, &a[1], t0 + Duration::from_millis(200)).is_empty());
        assert_eq!(r.counters(1).late_fragments, 1);
    }

    #[test]
    fn the_control_channel_is_never_held() {
        let mut r = Reassembler::ordered(SHORT);
        let skipped_ahead = fragment(0, 5, 5, 2, &[1, 2]);
        assert_eq!(push(&mut r, &skipped_ahead, Instant::now()).len(), 1);
    }

    #[test]
    fn held_messages_are_bounded() {
        let mut r = Reassembler::ordered(SHORT);
        let t0 = Instant::now();
        let mut delivered = 0;
        push(&mut r, &fragment(1, 0, 0, 1, &[0]), t0);
        // message 1 never comes; messages 2, 3, ... keep arriving
        for seq in 2..2000u16 {
            delivered += push(&mut r, &fragment(1, seq, seq, 1, &[1]), t0).len();
        }
        assert!(delivered > 1500, "the window forces the gap to be given up");
        assert!(r.channels[1].ordering.as_ref().unwrap().stats().skipped >= 1);
    }

    #[test]
    fn the_receiver_hands_out_released_messages_one_by_one() {
        let mut receiver = Receiver::with_reassembler(Reassembler::ordered(SHORT));
        let (a, b) = (
            message_pieces(0, 0, &message(1500)),
            message_pieces(1, 2, &[7, 7]),
        );
        let t0 = Instant::now();
        assert_eq!(receiver.classify_at(&a[0], t0), None);
        assert_eq!(receiver.classify_at(&b[0], t0), None);
        assert!(receiver.deadline().is_some());
        let first = receiver.classify_at(&a[1], t0 + Duration::from_millis(60));
        assert!(matches!(first, Some(Event::Message(m)) if m.bytes.len() == 1500));
        assert!(matches!(receiver.pop(), Some(Event::Message(m)) if m.bytes == [7, 7]));
        assert_eq!(receiver.pop(), None);
    }

    #[test]
    fn classify_maps_control_packets_to_events() {
        let mut receiver = Receiver::default();
        let confirm = Packet {
            header: Header {
                payload_type: pt::CHANNEL_CONFIRM,
                channel: 3,
                payload_len: 1,
                message_len: 1,
                ..Header::default()
            },
            payload: vec![1],
        };
        assert_eq!(
            receiver.classify(&confirm),
            Some(Event::ChannelConfirmed {
                channel: 3,
                in_order: true
            })
        );
        let rtcp = Packet {
            header: Header {
                payload_type: pt::RTCP,
                payload_len: 8,
                message_len: 8,
                ..Header::default()
            },
            payload: vec![1, 0, 0, 0, 2, 0, 0, 0],
        };
        assert_eq!(
            receiver.classify(&rtcp),
            Some(Event::TargetBitrate {
                rate: 1,
                timestamp: 2
            })
        );
        let ack = Packet {
            header: Header {
                payload_type: pt::ACK,
                ..Header::default()
            },
            payload: vec![],
        };
        assert_eq!(receiver.classify(&ack), None);
    }
}

//! Forward error correction of the server's packets (`docs/PROTOCOL.md` §2.3): after every 32 data packets of a
//! channel the server sends one parity packet (type 16), the XOR of their payloads padded with zeros and of 8 bytes
//! of their headers. A group that lost exactly one packet can rebuild it without waiting for the retransmission.
//! The parity's payload is only as long as the group's *last* packet (seen live: 1080 bytes for a group of 1384-byte
//! fragments), so it covers that prefix of every packet: a lost packet longer than the parity cannot be rebuilt.
//!
//! The wire format is known from the decompiled client only, never checked against real packets, so a recovery is
//! *shadowed* until the parity of complete groups has matched: the payload and the 8 header bytes of at least
//! [`TRUST_AFTER`] complete groups of the channel. A packet rebuilt while shadowed is kept to be compared with the
//! retransmitted copy when it arrives, and the first mismatch of any kind turns the channel's recovery off for good.
//! Only channels 1 to 7 are covered: the control channel's packets are not all part of a sequence.

use super::header::{Header, Packet, MAX_CHANNEL, MAX_RX_PAYLOAD};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Packets per group, as the server sends them (`spp_send_buffer_init`); a parity packet that says otherwise turns the
/// channel's recovery off.
pub const GROUP: usize = 32;
/// Complete groups of a channel whose parity must match before a recovery is used.
pub const TRUST_AFTER: u32 = 3;
/// Groups kept per channel while they fill.
const KEPT: usize = 6;
/// Mismatches dumped to the log so the real layout can be worked out from them.
const DUMPS: u32 = 3;

/// The 8 bytes of a data packet's header that the parity covers, in the sender's memory order (little endian):
/// payload length, type, message number, low three bytes of the message length.
fn sources(h: &Header) -> [u8; 8] {
    let (l, m, t) = (
        h.payload_len.to_le_bytes(),
        h.message_seq.to_le_bytes(),
        h.message_len.to_le_bytes(),
    );
    [l[0], l[1], h.payload_type, m[0], m[1], t[0], t[1], t[2]]
}

/// Where a parity packet carries the XOR of those 8 bytes: wire bytes 22, 19 (rotated one bit), 31, 30, 29, 28, 27, 26.
fn parity_of(h: &Header) -> [u8; 8] {
    let flags = (u8::from(h.force_ack) << 7) | h.spare_flags;
    let p = h.parity;
    [
        (h.message_len >> 24) as u8,
        flags.rotate_left(1),
        p[5],
        p[4],
        p[3],
        p[2],
        p[1],
        p[0],
    ]
}

/// The header of a data packet rebuilt from the parity of its group.
fn header_from(sources: [u8; 8], channel: u8, packet_seq: u16) -> Header {
    Header {
        payload_len: u16::from_le_bytes([sources[0], sources[1]]),
        payload_type: sources[2],
        channel,
        packet_seq,
        force_ack: true,
        message_seq: u16::from_le_bytes([sources[3], sources[4]]),
        message_len: u32::from_le_bytes([sources[5], sources[6], sources[7], 0]),
        ..Header::default()
    }
}

/// A figure over the run: how many, their sum and largest (µs).
#[derive(Debug, Default, Clone, Copy)]
struct Span {
    n: u64,
    sum: u64,
    max: u64,
}

impl Span {
    fn add(&mut self, d: Duration) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        (self.n, self.sum, self.max) = (self.n + 1, self.sum + us, self.max.max(us));
    }

    fn text(&self) -> String {
        if self.n == 0 {
            return "-".into();
        }
        format!(
            "{:.0}/{:.0}",
            self.sum as f64 / self.n as f64 / 1000.0,
            self.max as f64 / 1000.0
        )
    }
}

/// What the parity did over the run, for the report line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FecCounters {
    /// Parity packets received.
    pub parity: u64,
    /// Groups whose 32 packets were all there when the parity came, and whether it matched.
    pub complete_ok: u64,
    pub complete_bad: u64,
    /// Groups left alone because a packet came behind a later one.
    pub late: u64,
    /// Groups that lost one packet longer than the parity, which cannot be rebuilt.
    pub too_long: u64,
    /// Groups short of one packet, or of more, when the parity came.
    pub one_lost: u64,
    pub many_lost: u64,
    /// The hole was already filled (retransmission) when the parity came.
    pub filled_first: u64,
    /// Packets rebuilt and kept for comparison, and handed to the receiver.
    pub shadowed: u64,
    pub injected: u64,
    /// Rebuilt packets compared with the retransmitted copy.
    pub compared_ok: u64,
    pub compared_bad: u64,
}

/// One group of a channel while it fills.
struct Group {
    key: u16,
    present: u64,
    count: usize,
    /// XOR of the payloads, of the 8 header bytes, and of every header byte (for the dump).
    xor: Vec<u8>,
    src: [u8; 8],
    hdr: [u8; 32],
    parity: Option<Packet>,
    /// The lowest packet missing when a later one arrived, and when.
    hole: Option<(usize, Instant)>,
    /// A packet rebuilt (index, copy) and waiting for its retransmission to be compared.
    rebuilt: Option<(usize, Packet)>,
    /// Settled: complete and checked, or rebuilt.
    done: bool,
    /// The highest index seen, and whether a packet came behind it (a retransmitted copy, or a reordered one): a copy
    /// may differ from the original the parity was made from, so such a group is neither checked nor rebuilt.
    max_seen: Option<usize>,
    late: bool,
    /// Payload length of each packet held.
    lens: [u16; GROUP],
    /// Counted as a group that lost more than one packet.
    many: bool,
}

impl Group {
    fn new(key: u16) -> Self {
        Self {
            key,
            present: 0,
            count: 0,
            xor: vec![0; MAX_RX_PAYLOAD],
            src: [0; 8],
            hdr: [0; 32],
            parity: None,
            hole: None,
            rebuilt: None,
            done: false,
            max_seen: None,
            late: false,
            lens: [0; GROUP],
            many: false,
        }
    }

    fn has(&self, i: usize) -> bool {
        self.present >> i & 1 == 1
    }

    fn missing(&self) -> Option<usize> {
        (0..GROUP).find(|i| !self.has(*i))
    }

    fn add(&mut self, i: usize, p: &Packet) {
        self.present |= 1 << i;
        self.count += 1;
        self.lens[i] = p.header.payload_len;
        self.late |= self.max_seen.is_some_and(|m| i < m);
        self.max_seen = self.max_seen.max(Some(i));
        for (x, b) in self.xor.iter_mut().zip(&p.payload) {
            *x ^= b;
        }
        for (x, b) in self.src.iter_mut().zip(sources(&p.header)) {
            *x ^= b;
        }
        for (x, b) in self.hdr.iter_mut().zip(p.header.encode()) {
            *x ^= b;
        }
    }

    /// The parity's payload equals the first bytes of the XOR of the group's payloads (it is as long as the last
    /// packet of the group, not as the longest).
    fn payload_matches(&self, parity: &Packet) -> bool {
        let n = parity.payload.len().min(self.xor.len());
        parity.payload[..n] == self.xor[..n]
    }
}

#[derive(Default)]
struct ChannelFec {
    groups: VecDeque<Group>,
    /// Complete groups that matched; recovery is used from `TRUST_AFTER`.
    verified: u32,
    off: bool,
}

/// # Rebuilds the packet a group lost, from its parity packet.
#[derive(Default)]
pub struct Fec {
    channels: [ChannelFec; MAX_CHANNEL as usize + 1],
    counters: FecCounters,
    parity_lag: Span,
    retransmit_lag: Span,
    dumped: u32,
    notes: Vec<String>,
}

impl std::fmt::Debug for Fec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.line())
    }
}

impl Fec {
    /// What happened so far.
    pub fn counters(&self) -> FecCounters {
        self.counters
    }

    /// Whether `channel`'s packets are rebuilt (parity verified, no mismatch).
    pub fn trusted(&self, channel: u8) -> bool {
        self.channels
            .get(usize::from(channel))
            .is_some_and(|c| !c.off && c.verified >= TRUST_AFTER)
    }

    /// One line for the log: the counters and the two lags (average/largest, ms) between a hole noticed and the
    /// parity that could fill it, and between a hole noticed and the retransmitted copy.
    pub fn line(&self) -> String {
        let c = &self.counters;
        let on: Vec<String> = self
            .channels
            .iter()
            .enumerate()
            .filter(|(_, s)| s.verified > 0 || s.off)
            .map(|(i, s)| format!("ch{} {}{}", i, s.verified, if s.off { " OFF" } else { "" }))
            .collect();
        format!(
            "luna: fec {} parity, complete {} ok {} bad, {} left alone (copies), {} lost one ({} too long for the parity) {} more, {} filled first, rebuilt {} (used {}), \
             compared {} ok {} bad; ms avg/max hole to parity {}, hole to retransmission {}; groups verified [{}]",
            c.parity,
            c.complete_ok,
            c.complete_bad,
            c.late,
            c.one_lost,
            c.too_long,
            c.many_lost,
            c.filled_first,
            c.shadowed + c.injected,
            c.injected,
            c.compared_ok,
            c.compared_bad,
            self.parity_lag.text(),
            self.retransmit_lag.text(),
            on.join(", ")
        )
    }

    /// Lines worth printing once (a mismatch with the bytes to work the layout out from).
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    fn live(&self, channel: u8) -> bool {
        (1..=MAX_CHANNEL).contains(&channel) && !self.channels[usize::from(channel)].off
    }

    fn group_mut(&mut self, channel: u8, key: u16) -> &mut Group {
        let groups = &mut self.channels[usize::from(channel)].groups;
        if let Some(i) = groups.iter().position(|g| g.key == key) {
            return &mut groups[i];
        }
        if groups.len() == KEPT {
            groups.pop_front();
        }
        groups.push_back(Group::new(key));
        groups.back_mut().expect("just pushed")
    }

    /// # A data packet arrived (any non-parity packet of a channel 1 to 7).
    ///
    /// Returns a packet rebuilt from a parity that came first, when this one completes what it needed.
    pub fn on_data(&mut self, p: &Packet, now: Instant) -> Option<Packet> {
        let h = &p.header;
        if !self.live(h.channel) {
            return None;
        }
        let (key, i) = (
            h.packet_seq / GROUP as u16,
            usize::from(h.packet_seq) % GROUP,
        );
        let group = self.group_mut(h.channel, key);
        if let Some((_, built)) = group.rebuilt.take_if(|(j, _)| *j == i) {
            if let Some((_, at)) = group.hole.filter(|(m, _)| *m == i) {
                self.retransmit_lag.add(now.saturating_duration_since(at));
            }
            let same = built.payload == p.payload && sources(&built.header) == sources(h);
            self.compare(h.channel, same, &built, p);
            return None;
        }
        if group.has(i) {
            return None;
        }
        let first_missing = group.missing();
        group.add(i, p);
        // A packet that came after a hole: the hole is noticed now.
        if let Some(m) = first_missing.filter(|m| *m < i) {
            group.hole.get_or_insert((m, now));
        }
        let filled = group.hole.filter(|(m, _)| *m == i);
        let ready = group.parity.is_some() && group.count >= GROUP - 1;
        if let Some((_, at)) = filled {
            self.retransmit_lag.add(now.saturating_duration_since(at));
        }
        if ready {
            return self.settle(h.channel, key, now);
        }
        None
    }

    /// # A parity packet arrived.
    ///
    /// Returns the packet it rebuilds when the group lacks exactly one and the channel's recovery is trusted.
    pub fn on_parity(&mut self, p: &Packet, now: Instant) -> Option<Packet> {
        let h = &p.header;
        if !self.live(h.channel) {
            return None;
        }
        self.counters.parity += 1;
        let size = (h.message_len & 0xffff) as usize;
        if size != GROUP {
            self.channels[usize::from(h.channel)].off = true;
            self.notes.push(format!(
                "luna: fec: channel {} groups of {} packets, not {}: recovery off",
                h.channel, size, GROUP
            ));
            return None;
        }
        let group = self.group_mut(h.channel, h.message_seq);
        if group.parity.is_some() || group.done {
            return None;
        }
        group.parity = Some(p.clone());
        self.settle(h.channel, h.message_seq, now)
    }

    /// With the group's parity in hand: checks a complete group, or rebuilds the one packet it lacks.
    fn settle(&mut self, channel: u8, key: u16, now: Instant) -> Option<Packet> {
        let trusted = self.trusted(channel);
        let group = self.group_mut(channel, key);
        let parity = group.parity.clone()?;
        if group.done {
            return None;
        }
        let want = parity_of(&parity.header);
        if group.late && (group.count == GROUP || group.count + 1 == GROUP) {
            group.done = true;
            self.counters.late += 1;
            return None;
        }
        if group.count == GROUP {
            group.done = true;
            let ok = group.src == want && group.payload_matches(&parity);
            let filled_first = group.hole.is_some();
            self.counters.filled_first += u64::from(filled_first);
            self.checked(channel, key, ok);
            return None;
        }
        if group.count + 1 < GROUP {
            if !group.many {
                group.many = true;
                self.counters.many_lost += 1;
            }
            return None;
        }
        let i = group.missing()?;
        let mut src = want;
        for (s, g) in src.iter_mut().zip(group.src) {
            *s ^= g;
        }
        group.done = true;
        let payload_len = usize::from(u16::from_le_bytes([src[0], src[1]]));
        let fits = payload_len <= parity.payload.len() && payload_len <= MAX_RX_PAYLOAD;
        let mut payload = parity.payload;
        for (b, g) in payload.iter_mut().zip(&group.xor) {
            *b ^= g;
        }
        payload.truncate(payload_len);
        let seq = (usize::from(key) * GROUP + i) as u16;
        let rebuilt = Packet {
            header: header_from(src, channel, seq),
            payload,
        };
        let lag = group.hole.map(|(_, at)| now.saturating_duration_since(at));
        if fits {
            group.rebuilt = Some((i, rebuilt.clone()));
        }
        self.counters.one_lost += 1;
        if !fits {
            self.counters.too_long += 1;
            return None;
        }
        if let Some(lag) = lag {
            self.parity_lag.add(lag);
        }
        if trusted {
            self.counters.injected += 1;
            Some(rebuilt)
        } else {
            self.counters.shadowed += 1;
            None
        }
    }

    /// The outcome of a complete group's parity check.
    fn checked(&mut self, channel: u8, key: u16, ok: bool) {
        let state = &mut self.channels[usize::from(channel)];
        if ok {
            self.counters.complete_ok += 1;
            state.verified += 1;
            return;
        }
        self.counters.complete_bad += 1;
        state.off = true;
        if self.dumped >= DUMPS {
            return;
        }
        self.dumped += 1;
        if let Some(g) = state.groups.iter().find(|g| g.key == key) {
            let hex = |b: &[u8]| b.iter().map(|x| format!("{:02x}", x)).collect::<String>();
            let fec = g.parity.as_ref().map_or([0; 32], |p| p.header.encode());
            let pay = g
                .parity
                .as_ref()
                .map(|p| p.payload.as_slice())
                .unwrap_or_default();
            let diff: Vec<usize> = (0..pay.len()).filter(|&o| pay[o] != g.xor[o]).collect();
            self.notes.push(format!(
                "luna: fec: ch{} group {} does not match: parity header {} xor of the 32 headers {} (payload {} bytes, \
                 {} differ from offset {:?} to {:?}), lengths {:?}",
                channel,
                key,
                hex(&fec[12..]),
                hex(&g.hdr[12..]),
                pay.len(),
                diff.len(),
                diff.first(),
                diff.last(),
                g.lens
            ));
        }
    }

    /// A rebuilt packet against the copy the server retransmitted.
    fn compare(&mut self, channel: u8, same: bool, built: &Packet, real: &Packet) {
        if same {
            self.counters.compared_ok += 1;
            return;
        }
        self.counters.compared_bad += 1;
        self.channels[usize::from(channel)].off = true;
        if self.dumped < DUMPS {
            self.dumped += 1;
            self.notes.push(format!(
                "luna: fec: ch{} rebuilt packet {} differs from the retransmitted one: payload {} vs {} bytes, \
                 header bytes {:?} vs {:?}",
                channel,
                real.header.packet_seq,
                built.payload.len(),
                real.payload.len(),
                sources(&built.header),
                sources(&real.header)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::spp::{pt, Receiver};

    const FULL: usize = 1384;

    /// A run of `count` packets of `channel` from packet number `start`: messages of 1 to 7 fragments, the last one
    /// short and followed by an 8-byte trailer, numbered from `message_seq`.
    fn packets(channel: u8, start: u16, count: usize, message_seq: u16) -> Vec<Packet> {
        let mut out = Vec::new();
        let (mut seq, mut msg) = (start, message_seq);
        let mut n = 0usize;
        while out.len() < count {
            let fragments = [1, 3, 5, 2, 7, 4][n % 6];
            let last = 100 + (n * 211) % 1000;
            let total = (fragments - 1) * FULL + last;
            for f in 0..fragments {
                if out.len() == count {
                    break;
                }
                let len = if f + 1 == fragments { last + 8 } else { FULL };
                let payload: Vec<u8> = (0..len)
                    .map(|i| (i * 7 + usize::from(seq) * 13 + f) as u8)
                    .collect();
                out.push(Packet {
                    header: Header {
                        payload_len: len as u16,
                        payload_type: pt::DATA,
                        channel,
                        packet_seq: seq,
                        message_seq: msg,
                        message_len: total as u32,
                        ..Header::default()
                    },
                    payload,
                });
                seq = seq.wrapping_add(1);
            }
            msg = msg.wrapping_add(1);
            n += 1;
        }
        out
    }

    /// The parity packet of group `key` as the server builds it (the layout of `docs/PROTOCOL.md` §2.3).
    fn parity(channel: u8, key: u16, group: &[Packet]) -> Packet {
        // As long as the group's last packet (rounded to 8), covering that prefix of every payload.
        let size = group.last().unwrap().payload.len().next_multiple_of(8);
        let mut payload = vec![0u8; size];
        let mut x = [0u8; 8];
        for p in group {
            for (a, b) in payload.iter_mut().zip(&p.payload) {
                *a ^= b;
            }
            for (a, b) in x.iter_mut().zip(sources(&p.header)) {
                *a ^= b;
            }
        }
        let flags = x[1].rotate_right(1);
        Packet {
            header: Header {
                payload_len: size as u16,
                payload_type: pt::FEC,
                channel,
                message_seq: key,
                message_len: u32::from(x[0]) << 24 | GROUP as u32,
                force_ack: flags & 0x80 != 0,
                spare_flags: flags & 0x7f,
                parity: [x[7], x[6], x[5], x[4], x[3], x[2]],
                ..Header::default()
            },
            payload,
        }
    }

    /// How many bytes of a packet the group's parity covers.
    fn fits(group: &[Packet]) -> usize {
        group.last().unwrap().payload.len().next_multiple_of(8)
    }

    /// The first packet of the group of `count` packets from `start` that the parity covers, outside its edges.
    fn coverable(group: &[Packet]) -> usize {
        (1..GROUP - 1)
            .find(|&i| group[i].payload.len() <= fits(group))
            .expect("one")
    }

    fn feed(fec: &mut Fec, ps: &[Packet], t: Instant) -> Vec<Packet> {
        ps.iter().filter_map(|p| fec.on_data(p, t)).collect()
    }

    /// Groups 0.. of channel 1, `count` of them, all complete and followed by their parity.
    fn trusted(fec: &mut Fec, t: Instant) -> Vec<Packet> {
        let all = packets(1, 0, GROUP * TRUST_AFTER as usize, 0);
        for (k, g) in all.chunks(GROUP).enumerate() {
            assert!(feed(fec, g, t).is_empty());
            assert!(fec.on_parity(&parity(1, k as u16, g), t).is_none());
        }
        all
    }

    fn same(a: &Packet, b: &Packet) -> bool {
        a.header.payload_len == b.header.payload_len
            && a.header.payload_type == b.header.payload_type
            && a.header.channel == b.header.channel
            && a.header.packet_seq == b.header.packet_seq
            && a.header.message_seq == b.header.message_seq
            && a.header.message_len == b.header.message_len
            && a.payload == b.payload
    }

    #[test]
    fn complete_groups_are_checked_and_trusted_after_three() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        assert!(!fec.trusted(1));
        trusted(&mut fec, t);
        let c = fec.counters();
        assert_eq!((c.complete_ok, c.complete_bad, c.parity), (3, 0, 3));
        assert!(fec.trusted(1) && !fec.trusted(2));
    }

    #[test]
    fn every_position_that_fits_the_parity_is_rebuilt_exactly() {
        for lost in 0..GROUP {
            let (mut fec, t) = (Fec::default(), Instant::now());
            trusted(&mut fec, t);
            let g = packets(1, 96, GROUP, 40);
            let rest: Vec<Packet> = g
                .iter()
                .cloned()
                .enumerate()
                .filter(|(i, _)| *i != lost)
                .map(|(_, p)| p)
                .collect();
            assert!(feed(&mut fec, &rest, t).is_empty());
            let built = fec.on_parity(&parity(1, 3, &g), t);
            if g[lost].payload.len() <= fits(&g) {
                let built = built.expect("rebuilt");
                assert!(
                    same(&built, &g[lost]),
                    "position {lost}: {:?} vs {:?}",
                    built.header,
                    g[lost].header
                );
                assert_eq!(fec.counters().injected, 1);
            } else {
                assert!(built.is_none(), "position {lost} is longer than the parity");
                assert_eq!(fec.counters().too_long, 1);
            }
        }
    }

    #[test]
    fn before_trust_the_rebuilt_packet_is_only_compared_with_the_retransmission() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        let rest: Vec<Packet> = g[..9].iter().chain(&g[10..]).cloned().collect();
        feed(&mut fec, &rest, t);
        assert!(fec.on_parity(&parity(1, 0, &g), t).is_none());
        assert_eq!((fec.counters().shadowed, fec.counters().injected), (1, 0));
        assert!(fec.on_data(&g[9], t + Duration::from_millis(60)).is_none());
        assert_eq!(
            (fec.counters().compared_ok, fec.counters().compared_bad),
            (1, 0)
        );
        assert!(!fec.channels[1].off);
    }

    #[test]
    fn a_rebuilt_packet_that_differs_turns_the_channel_off() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        let rest: Vec<Packet> = g[..9].iter().chain(&g[10..]).cloned().collect();
        feed(&mut fec, &rest, t);
        fec.on_parity(&parity(1, 0, &g), t);
        let mut other = g[9].clone();
        other.payload[3] ^= 1;
        fec.on_data(&other, t);
        assert_eq!(fec.counters().compared_bad, 1);
        assert!(fec.channels[1].off && !fec.take_notes().is_empty());
    }

    #[test]
    fn a_parity_that_does_not_match_a_complete_group_turns_the_channel_off() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        feed(&mut fec, &g, t);
        let mut p = parity(1, 0, &g);
        p.header.parity[2] ^= 0x10;
        assert!(fec.on_parity(&p, t).is_none());
        assert_eq!((fec.counters().complete_bad, fec.trusted(1)), (1, false));
        let notes = fec.take_notes();
        assert!(notes[0].contains("does not match"), "{notes:?}");
        // Off for good: even a good parity rebuilds nothing now.
        let g2 = packets(1, 32, GROUP, 9);
        let rest: Vec<Packet> = g2[1..].to_vec();
        feed(&mut fec, &rest, t);
        assert!(fec.on_parity(&parity(1, 1, &g2), t).is_none());
    }

    #[test]
    fn a_payload_that_does_not_match_a_complete_group_is_bad_too() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        feed(&mut fec, &g, t);
        let mut p = parity(1, 0, &g);
        p.payload[5] ^= 1;
        fec.on_parity(&p, t);
        assert_eq!(fec.counters().complete_bad, 1);
    }

    #[test]
    fn two_lost_packets_are_not_rebuilt_and_a_late_copy_leaves_the_group_alone() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        trusted(&mut fec, t);
        let g = packets(1, 96, GROUP, 40);
        let rest: Vec<Packet> = g[..4]
            .iter()
            .chain(&g[5..20])
            .chain(&g[21..])
            .cloned()
            .collect();
        feed(&mut fec, &rest, t);
        assert!(fec.on_parity(&parity(1, 3, &g), t).is_none());
        assert_eq!(fec.counters().many_lost, 1);
        // The retransmitted copy of 4 comes behind later ones: the group is not trusted any more.
        assert!(fec.on_data(&g[4], t).is_none());
        assert_eq!((fec.counters().late, fec.counters().injected), (1, 0));
    }

    #[test]
    fn a_parity_that_comes_before_the_last_packet_waits_for_a_packet_in_order() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        trusted(&mut fec, t);
        let g = packets(1, 96, GROUP, 40);
        let before: Vec<Packet> = g[..20]
            .iter()
            .chain(&g[21..30])
            .chain(&g[31..])
            .cloned()
            .collect();
        feed(&mut fec, &before, t);
        assert!(fec.on_parity(&parity(1, 3, &g), t).is_none());
        // 30 comes behind 31: a copy or a reordering, the group is left alone.
        assert!(fec.on_data(&g[30], t).is_none());
        assert_eq!(fec.counters().late, 1);
    }

    #[test]
    fn a_copy_of_a_packet_is_counted_once() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        trusted(&mut fec, t);
        let g = packets(1, 96, GROUP, 40);
        let mut rest: Vec<Packet> = g[..7].iter().chain(&g[8..]).cloned().collect();
        rest.extend(g[..5].iter().cloned());
        feed(&mut fec, &rest, t);
        let built = fec.on_parity(&parity(1, 3, &g), t).expect("rebuilt");
        assert!(same(&built, &g[7]));
    }

    #[test]
    fn the_packet_numbers_wrap_with_their_groups() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let all = packets(2, 65_440, GROUP * 5, 7);
        for (k, g) in all.chunks(GROUP).enumerate().take(3) {
            let key = (65_536 - 96 + k * GROUP) / GROUP % 2048;
            feed(&mut fec, g, t);
            fec.on_parity(&parity(2, key as u16, g), t);
        }
        assert!(fec.trusted(2));
        let g = &all[3 * GROUP..4 * GROUP];
        assert_eq!(g[0].header.packet_seq, 0);
        let lost = coverable(g);
        let rest: Vec<Packet> = g[..lost].iter().chain(&g[lost + 1..]).cloned().collect();
        feed(&mut fec, &rest, t);
        let built = fec.on_parity(&parity(2, 0, g), t).expect("rebuilt");
        assert!(same(&built, &g[lost]));
    }

    #[test]
    fn the_lags_to_the_parity_and_to_the_retransmission_are_measured() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        feed(&mut fec, &g[..5], t);
        feed(&mut fec, &g[6..20], t + Duration::from_millis(3));
        feed(&mut fec, &g[20..], t + Duration::from_millis(10));
        fec.on_parity(&parity(1, 0, &g), t + Duration::from_millis(40));
        fec.on_data(&g[5], t + Duration::from_millis(61));
        let line = fec.line();
        assert!(line.contains("hole to parity 37/37"), "{line}");
        assert!(line.contains("hole to retransmission 58/58"), "{line}");
    }

    #[test]
    fn groups_of_another_size_turn_recovery_off() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(1, 0, GROUP, 0);
        let mut p = parity(1, 0, &g);
        p.header.message_len = 64;
        assert!(fec.on_parity(&p, t).is_none());
        assert!(fec.channels[1].off && fec.take_notes()[0].contains("64"));
    }

    #[test]
    fn the_control_channel_is_left_alone() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        let g = packets(0, 0, GROUP, 0);
        assert!(feed(&mut fec, &g, t).is_empty());
        assert!(fec.on_parity(&parity(0, 0, &g), t).is_none());
        assert_eq!(fec.counters().parity, 0);
    }

    #[test]
    fn a_group_with_a_packet_behind_a_later_one_is_left_alone() {
        let (mut fec, t) = (Fec::default(), Instant::now());
        trusted(&mut fec, t);
        let g = packets(1, 96, GROUP, 40);
        // Packet 9 comes after 10 (a copy, or reordered), packet 20 is lost.
        let order: Vec<Packet> = g[..9]
            .iter()
            .chain([&g[10], &g[9]])
            .chain(&g[11..20])
            .chain(&g[21..])
            .cloned()
            .collect();
        assert!(feed(&mut fec, &order, t).is_empty());
        assert!(fec.on_parity(&parity(1, 3, &g), t).is_none());
        let c = fec.counters();
        assert_eq!((c.late, c.one_lost, c.injected), (1, 0, 0));
    }

    #[test]
    fn the_receiver_completes_a_message_whose_fragment_was_rebuilt() {
        let (mut rx, t) = (Receiver::default(), Instant::now());
        let all = packets(1, 0, GROUP * 9, 0);
        for (k, g) in all.chunks(GROUP).enumerate().take(3) {
            for p in g {
                rx.classify_at(p, t);
            }
            rx.classify_at(&parity(1, k as u16, g), t);
        }
        while rx.pop().is_some() {}
        // A group with a fragment in the middle of a message that the parity covers.
        let (key, g) = (3..9)
            .map(|k| (k, &all[k * GROUP..(k + 1) * GROUP]))
            .find(|(_, g)| {
                (1..GROUP - 1).any(|i| {
                    g[i].header.message_seq == g[i - 1].header.message_seq
                        && g[i].header.message_seq == g[i + 1].header.message_seq
                        && g[i].payload.len() <= fits(g)
                })
            })
            .expect("such a group");
        // The groups before it arrive whole, so its messages are next in order.
        for k in 3..key {
            for p in &all[k * GROUP..(k + 1) * GROUP] {
                rx.classify_at(p, t);
            }
        }
        while rx.pop().is_some() {}
        // Lose a fragment in the middle of a multi-fragment message.
        let lost = (0..GROUP)
            .find(|&i| {
                i > 0
                    && i + 1 < GROUP
                    && g[i].header.message_seq == g[i - 1].header.message_seq
                    && g[i].header.message_seq == g[i + 1].header.message_seq
                    && g[i].payload.len() <= fits(g)
            })
            .expect("such a fragment");
        let msg = g[lost].header.message_seq;
        let mut got = Vec::new();
        for (i, p) in g.iter().enumerate() {
            if i != lost {
                rx.classify_at(p, t);
            }
        }
        rx.classify_at(&parity(1, key as u16, g), t);
        while let Some(e) = rx.pop() {
            got.push(e);
        }
        assert_eq!(rx.take_rebuilt(), vec![(1, g[lost].header.packet_seq)]);
        assert!(
            !got.is_empty(),
            "messages released, message {msg} among them"
        );
        assert!(rx.fec_line().contains("rebuilt 1 (used 1)"));
    }
}

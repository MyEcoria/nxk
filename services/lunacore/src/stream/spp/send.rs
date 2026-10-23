//! Sequence numbering and fragmentation of outgoing messages, without retransmission (`docs/PROTOCOL.md` §2.3-§2.4).

use super::header::{pt, Header, HEADER_LEN, MAX_CHANNEL};
type Result<T> = std::result::Result<T, String>;

/// Payload bytes per packet for control messages (packet size `0x3F8` minus the header).
pub const CONTROL_PAYLOAD: usize = 984;
/// Largest payload the sender emits (`0x568`).
pub const MAX_TX_PAYLOAD: usize = 0x568;
/// The first packets of every channel ask for an acknowledgement.
const FORCE_ACK_PACKETS: u16 = 8;
const TRAILER_LEN: usize = 8;
const ALIGN: usize = 4;

/// Message types that are never sent with `force_ack`: acknowledgements and time synchronisation.
const NEVER_ACKED: [u8; 5] = [
    pt::ACK,
    pt::TIME_REQUEST_32,
    pt::TIME_REPLY_32,
    pt::TIME_REQUEST_64,
    pt::TIME_REPLY_64,
];

#[derive(Debug, Clone, Copy, Default)]
struct ChannelCounters {
    packet_seq: u16,
    message_seq: u16,
}

/// # Counters of one outgoing connection, producing ready-to-send datagrams.
///
/// Three counters are kept: the connection sequence (every datagram), and per channel the packet number
/// (every fragment) and the message number (every message). Nothing is remembered for retransmission.
#[derive(Debug, Clone, Default)]
pub struct SendState {
    connection_seq: u16,
    channels: [ChannelCounters; MAX_CHANNEL as usize + 1],
    trailer: bool,
}

impl SendState {
    /// # Builds the counters at zero.
    ///
    /// ## Arguments
    /// * `trailer` - pad the last fragment to a multiple of 4 bytes and append the 8-byte timestamp trailer when it
    ///   fits, as the Android client does; the receiver accepts packets without it.
    pub fn new(trailer: bool) -> Self {
        Self {
            trailer,
            ..Self::default()
        }
    }

    /// # Splits `message` into datagrams and numbers them.
    ///
    /// ## Arguments
    /// * `max_payload` - payload bytes per datagram, at most [`MAX_TX_PAYLOAD`]; [`CONTROL_PAYLOAD`] for control messages
    /// * `now_us` - sender clock in microseconds; its low 32 bits go to every header, the whole value to the trailer
    ///
    /// ## Errors
    /// Fails on a channel above 7, a `max_payload` that is zero or too large, or a message of 4 GiB or more.
    pub fn fragment(
        &mut self,
        channel: u8,
        payload_type: u8,
        message: &[u8],
        max_payload: usize,
        now_us: u64,
    ) -> Result<Vec<Vec<u8>>> {
        if channel > MAX_CHANNEL {
            return Err(format!("channel {channel} is out of range"));
        }
        if !(1..=MAX_TX_PAYLOAD).contains(&max_payload) {
            return Err(format!("payload size {max_payload} is out of range"));
        }
        let Ok(message_len) = u32::try_from(message.len()) else {
            return Err(format!("message of {} bytes is too large", message.len()));
        };
        let message_seq = self.channels[usize::from(channel)].message_seq;
        self.channels[usize::from(channel)].message_seq = message_seq.wrapping_add(1);
        let chunks: Vec<&[u8]> = if message.is_empty() {
            vec![message]
        } else {
            message.chunks(max_payload).collect()
        };
        let last = chunks.len() - 1;
        let mut datagrams = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.into_iter().enumerate() {
            let tail = (index == last && self.trailer && !chunk.is_empty())
                .then(|| padding_and_trailer(chunk.len(), max_payload, now_us))
                .flatten();
            let counters = &mut self.channels[usize::from(channel)];
            let packet_seq = counters.packet_seq;
            counters.packet_seq = packet_seq.wrapping_add(1);
            let tail = tail.unwrap_or_default();
            let header = Header {
                connection_seq: self.next_connection_seq(),
                timestamp: low_32(now_us),
                payload_len: u16::try_from(chunk.len() + tail.len()).map_err(|e| e.to_string())?,
                payload_type,
                channel,
                packet_seq,
                force_ack: packet_seq < FORCE_ACK_PACKETS && !NEVER_ACKED.contains(&payload_type),
                message_seq,
                message_len,
                ..Header::default()
            };
            let mut datagram = Vec::with_capacity(HEADER_LEN + chunk.len() + tail.len());
            datagram.extend_from_slice(&header.encode());
            datagram.extend_from_slice(chunk);
            datagram.extend_from_slice(&tail);
            datagrams.push(datagram);
        }
        Ok(datagrams)
    }

    fn next_connection_seq(&mut self) -> u16 {
        let seq = self.connection_seq;
        self.connection_seq = seq.wrapping_add(1);
        seq
    }
}

/// Padding to a multiple of 4 bytes then the timestamp trailer, only when that still fits in `max_payload`.
fn padding_and_trailer(rest: usize, max_payload: usize, now_us: u64) -> Option<Vec<u8>> {
    let padded = rest.next_multiple_of(ALIGN);
    if padded + TRAILER_LEN >= max_payload {
        return None;
    }
    let mut tail = vec![0u8; padded - rest];
    tail.extend_from_slice(&now_us.to_le_bytes());
    Some(tail)
}

fn low_32(value: u64) -> u32 {
    u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::spp::Packet;

    const TS: u64 = 0x0000_0001_0102_0304;

    fn packets(datagrams: &[Vec<u8>]) -> Vec<Packet> {
        datagrams
            .iter()
            .map(|d| Packet::decode(d).unwrap())
            .collect()
    }

    fn connect_like(trailer: bool) -> Vec<Vec<u8>> {
        let message: Vec<u8> = (0..1030).map(|i| (i % 251) as u8).collect();
        SendState::new(trailer)
            .fragment(0, pt::CONN_REQUEST, &message, CONTROL_PAYLOAD, TS)
            .unwrap()
    }

    #[test]
    fn connect_is_two_fragments_with_consecutive_packet_numbers() {
        let parsed = packets(&connect_like(true));
        assert_eq!(parsed.len(), 2);
        let [a, b] = [&parsed[0].header, &parsed[1].header];
        assert_eq!((a.connection_seq, b.connection_seq), (0, 1));
        assert_eq!((a.packet_seq, b.packet_seq), (0, 1));
        assert_eq!((a.message_seq, b.message_seq), (0, 0));
        assert_eq!((a.message_len, b.message_len), (1030, 1030));
        assert!(a.force_ack && b.force_ack);
        assert_eq!(a.timestamp, 0x0102_0304);
    }

    /// Headers A and B of `docs/PROTOCOL.md` §2.6, timestamp `01020304`.
    #[test]
    fn connect_headers_match_the_documented_vectors() {
        let datagrams = connect_like(true);
        let header_a =
            "80140000 01020304 00000000 03d8 14 00 0000 00 80 0000 00000406 000000000000";
        let header_b =
            "80140001 01020304 00000000 0038 14 00 0001 00 80 0000 00000406 000000000000";
        for (datagram, expected) in datagrams.iter().zip([header_a, header_b]) {
            let digits: String = expected.split_whitespace().collect();
            let bytes: Vec<u8> = (0..digits.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
                .collect();
            assert_eq!(datagram[..HEADER_LEN], bytes[..]);
        }
        assert_eq!(datagrams[0].len(), 32 + 984);
        assert_eq!(datagrams[1].len(), 32 + 0x38);
    }

    #[test]
    fn last_fragment_carries_padding_and_trailer_like_the_android_client() {
        let parsed = packets(&connect_like(true));
        assert_eq!(parsed[0].header.payload_len, 984);
        assert_eq!(parsed[1].header.payload_len, 0x38);
        let payload = &parsed[1].payload;
        assert_eq!(&payload[46..48], &[0, 0]);
        assert_eq!(payload[48..], TS.to_le_bytes());
    }

    #[test]
    fn without_trailer_the_last_fragment_is_exact() {
        let parsed = packets(&connect_like(false));
        assert_eq!(parsed[1].header.payload_len, 0x2e);
        assert_eq!(parsed[1].header.message_len, 1030);
    }

    #[test]
    fn fragments_reassemble_to_the_message() {
        let message: Vec<u8> = (0..1030).map(|i| (i % 251) as u8).collect();
        let parsed = packets(&connect_like(true));
        let mut joined = parsed[0].payload.clone();
        joined.extend_from_slice(&parsed[1].payload);
        assert_eq!(joined[..1030], message[..]);
    }

    #[test]
    fn trailer_is_skipped_when_it_would_not_fit() {
        // 984 - 8 = 976: a 976-byte rest plus the trailer reaches the limit, so the packet stays exact.
        let mut state = SendState::new(true);
        let datagrams = state
            .fragment(0, pt::CONN_REQUEST, &[7; 976], CONTROL_PAYLOAD, TS)
            .unwrap();
        assert_eq!(packets(&datagrams)[0].header.payload_len, 976);
        let datagrams = state
            .fragment(0, pt::CONN_REQUEST, &[7; 972], CONTROL_PAYLOAD, TS)
            .unwrap();
        assert_eq!(packets(&datagrams)[0].header.payload_len, 980);
    }

    #[test]
    fn counters_are_per_channel_and_the_connection_counter_is_global() {
        let mut state = SendState::new(false);
        let a = state.fragment(0, pt::DATA, b"one", 100, TS).unwrap();
        let b = state.fragment(2, pt::DATA, b"two", 100, TS).unwrap();
        let c = state.fragment(2, pt::DATA, b"three", 100, TS).unwrap();
        let [a, b, c] = [&a, &b, &c].map(|d| packets(d)[0].header);
        assert_eq!(
            [a.connection_seq, b.connection_seq, c.connection_seq],
            [0, 1, 2]
        );
        assert_eq!([a.packet_seq, b.packet_seq, c.packet_seq], [0, 0, 1]);
        assert_eq!([a.message_seq, b.message_seq, c.message_seq], [0, 0, 1]);
    }

    #[test]
    fn counters_wrap_around_at_sixteen_bits() {
        let mut state = SendState::new(false);
        state.connection_seq = u16::MAX;
        state.channels[1].packet_seq = u16::MAX;
        state.channels[1].message_seq = u16::MAX;
        let first = state.fragment(1, pt::DATA, b"x", 100, TS).unwrap();
        let second = state.fragment(1, pt::DATA, b"y", 100, TS).unwrap();
        let [first, second] = [&first, &second].map(|d| packets(d)[0].header);
        assert_eq!((first.connection_seq, second.connection_seq), (u16::MAX, 0));
        assert_eq!((first.packet_seq, second.packet_seq), (u16::MAX, 0));
        assert_eq!((first.message_seq, second.message_seq), (u16::MAX, 0));
    }

    #[test]
    fn force_ack_covers_the_first_eight_packets_of_a_channel_only() {
        let mut state = SendState::new(false);
        let flags: Vec<bool> = (0..10)
            .map(|_| {
                let d = state.fragment(1, pt::DATA, b"x", 100, TS).unwrap();
                packets(&d)[0].header.force_ack
            })
            .collect();
        assert_eq!(
            flags,
            [true, true, true, true, true, true, true, true, false, false]
        );
    }

    #[test]
    fn acknowledgements_and_time_sync_never_ask_for_an_ack() {
        let mut state = SendState::new(false);
        for payload_type in NEVER_ACKED {
            let d = state.fragment(0, payload_type, b"xx", 100, TS).unwrap();
            assert!(!packets(&d)[0].header.force_ack, "type {payload_type}");
        }
    }

    #[test]
    fn empty_message_is_one_empty_packet_without_trailer() {
        let d = SendState::new(true)
            .fragment(0, pt::CONN_CLOSE, &[], CONTROL_PAYLOAD, TS)
            .unwrap();
        let parsed = packets(&d);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].header.message_len, 0);
        assert_eq!(parsed[0].header.payload_len, 0);
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        let mut state = SendState::new(false);
        assert!(state.fragment(8, pt::DATA, b"x", 100, TS).is_err());
        assert!(state.fragment(0, pt::DATA, b"x", 0, TS).is_err());
        assert!(state
            .fragment(0, pt::DATA, b"x", MAX_TX_PAYLOAD + 1, TS)
            .is_err());
    }
}

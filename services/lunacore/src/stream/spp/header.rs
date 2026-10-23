//! The 32-byte SPP packet header and the payload type codes, see `docs/PROTOCOL.md` §2.

/// Size of the header that starts every SPP datagram.
pub const HEADER_LEN: usize = 32;
/// Largest payload the receiver accepts (`0x588`); senders stay below `0x568`.
pub const MAX_RX_PAYLOAD: usize = 0x588;
/// Highest channel identifier: channel 0 is the control channel, 1 to 7 are application channels.
pub const MAX_CHANNEL: u8 = 7;
/// RTP version carried in the top bits of byte 0.
const RTP_VERSION: u8 = 2;
const FORCE_ACK_BIT: u8 = 0x80;
/// Payload types are strictly below this value.
const PAYLOAD_TYPE_LIMIT: u8 = 0x18;

/// Payload types (`PT_*`), byte 14 of the header.
pub mod pt {
    /// Application message of a channel.
    pub const DATA: u8 = 0;
    pub const ACK: u8 = 4;
    pub const CHANNEL_REQUEST: u8 = 6;
    pub const TIME_REQUEST_32: u8 = 7;
    pub const TIME_REPLY_32: u8 = 8;
    pub const DIAGNOSTIC: u8 = 13;
    /// XOR parity packet; never acknowledged.
    pub const FEC: u8 = 16;
    pub const CONN_CLOSE: u8 = 17;
    pub const RTCP: u8 = 18;
    pub const CHANNEL_CONFIRM: u8 = 19;
    pub const CONN_REQUEST: u8 = 20;
    pub const CONN_RESPONSE: u8 = 21;
    pub const TIME_REQUEST_64: u8 = 22;
    pub const TIME_REPLY_64: u8 = 23;
}

/// # Why a datagram is not a valid SPP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SppError {
    Truncated(usize),
    Version(u8),
    PayloadLength { declared: usize, available: usize },
    PayloadType(u8),
    Channel(u8),
}

impl std::fmt::Display for SppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated(n) => {
                write!(f, "datagram of {} bytes is shorter than the SPP header", n)
            }
            Self::Version(v) => write!(f, "unsupported RTP version {}", v),
            Self::PayloadLength {
                declared,
                available,
            } => {
                write!(
                    f,
                    "payload length {} does not fit the {} bytes after the header",
                    declared, available
                )
            }
            Self::PayloadType(t) => write!(f, "payload type {:#04x} is out of range", t),
            Self::Channel(c) => write!(f, "channel {} is greater than the maximum", c),
        }
    }
}

/// # Decoded SPP header. Multi-byte fields are big-endian on the wire.
///
/// Byte 1 (a copy of the payload type), byte 18 (reserved, always 0) and the low bits of byte 19 are not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// Bytes 2-3: counts every datagram sent on the connection.
    pub connection_seq: u16,
    /// Bytes 4-7: sender clock in microseconds, low 32 bits.
    pub timestamp: u32,
    /// Bytes 8-11: 0 for the client's connection.
    pub connection_id: u32,
    /// Bytes 12-13: payload length, including any padding and trailer.
    pub payload_len: u16,
    /// Byte 14: one of [`pt`].
    pub payload_type: u8,
    /// Byte 15: 0 for control, 1 to 7 for application channels.
    pub channel: u8,
    /// Bytes 16-17: per-channel packet number, +1 per fragment.
    pub packet_seq: u16,
    /// Byte 19, bit 7: the receiver must answer with a [`pt::ACK`].
    pub force_ack: bool,
    /// Bytes 20-21: per-channel message number, shared by all fragments of a message.
    pub message_seq: u16,
    /// Bytes 22-25: length of the whole message before fragmentation.
    pub message_len: u32,
    /// Bytes 26-31: zero for data packets, header parity for FEC packets.
    pub parity: [u8; 6],
}

impl Header {
    /// # Serializes the header into its 32 wire bytes.
    ///
    /// ## Example
    /// ```
    /// use lunacore::stream::spp::{Header, HEADER_LEN};
    /// let bytes = Header { channel: 3, ..Header::default() }.encode();
    /// assert_eq!(bytes.len(), HEADER_LEN);
    /// assert_eq!(bytes[0], 0x80);
    /// assert_eq!(bytes[15], 3);
    /// ```
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = RTP_VERSION << 6;
        out[1] = self.payload_type & 0x7f;
        out[2..4].copy_from_slice(&self.connection_seq.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.connection_id.to_be_bytes());
        out[12..14].copy_from_slice(&self.payload_len.to_be_bytes());
        out[14] = self.payload_type;
        out[15] = self.channel;
        out[16..18].copy_from_slice(&self.packet_seq.to_be_bytes());
        out[19] = if self.force_ack { FORCE_ACK_BIT } else { 0 };
        out[20..22].copy_from_slice(&self.message_seq.to_be_bytes());
        out[22..26].copy_from_slice(&self.message_len.to_be_bytes());
        out[26..32].copy_from_slice(&self.parity);
        out
    }

    /// # Parses the first 32 bytes of a datagram.
    ///
    /// ## Errors
    /// Fails on a short buffer, an RTP version other than 2, a payload type of `0x18` or more, or a channel above 7.
    /// The payload length is checked against the datagram by [`Packet::decode`].
    pub fn decode(datagram: &[u8]) -> Result<Self, SppError> {
        let bytes: &[u8; HEADER_LEN] = datagram
            .get(..HEADER_LEN)
            .and_then(|head| head.try_into().ok())
            .ok_or(SppError::Truncated(datagram.len()))?;
        let version = bytes[0] >> 6;
        if version != RTP_VERSION {
            return Err(SppError::Version(version));
        }
        let (payload_type, channel) = (bytes[14], bytes[15]);
        if payload_type >= PAYLOAD_TYPE_LIMIT {
            return Err(SppError::PayloadType(payload_type));
        }
        if channel > MAX_CHANNEL {
            return Err(SppError::Channel(channel));
        }
        Ok(Self {
            connection_seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            connection_id: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            payload_len: u16::from_be_bytes([bytes[12], bytes[13]]),
            payload_type,
            channel,
            packet_seq: u16::from_be_bytes([bytes[16], bytes[17]]),
            force_ack: bytes[19] & FORCE_ACK_BIT != 0,
            message_seq: u16::from_be_bytes([bytes[20], bytes[21]]),
            message_len: u32::from_be_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]),
            parity: [
                bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
            ],
        })
    }
}

/// # A received datagram: header and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    /// Exactly `header.payload_len` bytes; may end with padding and an 8-byte trailer.
    pub payload: Vec<u8>,
}

impl Packet {
    /// # Parses and validates a whole datagram.
    ///
    /// ## Errors
    /// Everything [`Header::decode`] rejects, plus a payload length above `0x588` or beyond the end of the datagram.
    pub fn decode(datagram: &[u8]) -> Result<Self, SppError> {
        let header = Header::decode(datagram)?;
        let declared = usize::from(header.payload_len);
        let available = datagram.len() - HEADER_LEN;
        if declared > MAX_RX_PAYLOAD || declared > available {
            return Err(SppError::PayloadLength {
                declared,
                available,
            });
        }
        Ok(Self {
            header,
            payload: datagram[HEADER_LEN..HEADER_LEN + declared].to_vec(),
        })
    }

    /// # The message carried by a single-fragment packet, without padding or trailer.
    ///
    /// The cut is made at the total message length (bytes 22-25): the sender pads and appends a trailer
    /// that are counted in the payload length. `None` when the message continues in other fragments.
    pub fn single_fragment_message(&self) -> Option<&[u8]> {
        let total = usize::try_from(self.header.message_len).ok()?;
        self.payload.get(..total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header A of the connect request, `docs/PROTOCOL.md` §2.6, with the timestamp `01020304`.
    const CONNECT_A: &str =
        "80140000 01020304 00000000 03d8 14 00 0000 00 80 0000 00000406 000000000000";

    fn hex(spaced: &str) -> Vec<u8> {
        let digits: String = spaced.split_whitespace().collect();
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    }

    fn connect_a() -> Header {
        Header {
            timestamp: 0x0102_0304,
            payload_len: 984,
            payload_type: pt::CONN_REQUEST,
            force_ack: true,
            message_len: 1030,
            ..Header::default()
        }
    }

    #[test]
    fn known_vector_is_encoded_byte_for_byte() {
        assert_eq!(connect_a().encode().to_vec(), hex(CONNECT_A));
    }

    #[test]
    fn known_vector_is_decoded_field_by_field() {
        assert_eq!(Header::decode(&hex(CONNECT_A)).unwrap(), connect_a());
    }

    #[test]
    fn every_field_survives_a_round_trip() {
        let header = Header {
            connection_seq: 0xBEEF,
            timestamp: 0xDEAD_BEEF,
            connection_id: 0x0102_0304,
            payload_len: 1384,
            payload_type: pt::CHANNEL_REQUEST,
            channel: 7,
            packet_seq: 0xFFFE,
            force_ack: true,
            message_seq: 0x1234,
            message_len: 0x00AB_CDEF,
            parity: [1, 2, 3, 4, 5, 6],
        };
        assert_eq!(Header::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn force_ack_is_bit_seven_of_byte_nineteen() {
        let mut header = Header::default();
        assert_eq!(header.encode()[19], 0x00);
        header.force_ack = true;
        assert_eq!(header.encode()[19], 0x80);
        let mut bytes = header.encode();
        bytes[19] = 0x7f;
        assert!(!Header::decode(&bytes).unwrap().force_ack);
    }

    #[test]
    fn byte_one_copies_the_payload_type() {
        let header = Header {
            payload_type: pt::FEC,
            ..Header::default()
        };
        assert_eq!(&header.encode()[..2], &[0x80, 0x10]);
    }

    #[test]
    fn malformed_headers_are_rejected() {
        let good = connect_a().encode();
        assert_eq!(Header::decode(&good[..31]), Err(SppError::Truncated(31)));
        let mut bad = good;
        bad[0] = 0x40;
        assert_eq!(Header::decode(&bad), Err(SppError::Version(1)));
        let mut bad = good;
        bad[14] = 0x18;
        assert_eq!(Header::decode(&bad), Err(SppError::PayloadType(0x18)));
        let mut bad = good;
        bad[15] = 8;
        assert_eq!(Header::decode(&bad), Err(SppError::Channel(8)));
    }

    #[test]
    fn payload_length_must_fit_the_datagram_and_the_receive_limit() {
        let mut datagram = connect_a().encode().to_vec();
        datagram.extend_from_slice(&[0; 983]);
        assert_eq!(
            Packet::decode(&datagram),
            Err(SppError::PayloadLength {
                declared: 984,
                available: 983
            })
        );
        let header = Header {
            payload_len: 0x589,
            ..connect_a()
        };
        let mut datagram = header.encode().to_vec();
        datagram.extend_from_slice(&[0; 0x589]);
        assert!(matches!(
            Packet::decode(&datagram),
            Err(SppError::PayloadLength { .. })
        ));
    }

    #[test]
    fn padding_and_trailer_are_cut_at_the_message_length() {
        let header = Header {
            payload_len: 12,
            payload_type: pt::CONN_RESPONSE,
            message_len: 2,
            ..Header::default()
        };
        let mut datagram = header.encode().to_vec();
        datagram.extend_from_slice(&[1, 2, 0, 0, 9, 9, 9, 9, 9, 9, 9, 9]);
        let packet = Packet::decode(&datagram).unwrap();
        assert_eq!(packet.single_fragment_message(), Some(&[1, 2][..]));
    }

    #[test]
    fn a_fragment_of_a_longer_message_is_not_a_message() {
        let packet = Packet {
            header: connect_a(),
            payload: vec![0; 984],
        };
        assert_eq!(packet.single_fragment_message(), None);
    }
}

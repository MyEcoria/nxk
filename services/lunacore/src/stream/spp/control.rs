//! Payloads of the control messages a client sends in answer to the server or to keep the connection alive:
//! time sync replies and `PT_RTCP`, see `docs/PROTOCOL.md` §2.5. All integers are little-endian (observed in the
//! server's time requests).

use super::header::pt;

/// `PT_TIME_REQUEST_32` payload: `[u16 id][4 bytes][u32 ms]`.
const REQUEST_32_LEN: usize = 10;
/// `PT_TIME_REQUEST_64` payload: `[u16 id][8 bytes][u64 ms]`.
const REQUEST_64_LEN: usize = 18;

/// # The clocks a time reply reports about this side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clocks {
    /// Seconds since the Unix epoch when the connection started (the "session start" field; clock deduced).
    pub start_secs: u32,
    /// Milliseconds on this side's monotonic clock.
    pub now_ms: u64,
}

/// # Builds the reply to a `PT_TIME_REQUEST_32` or `PT_TIME_REQUEST_64`.
///
/// The reply repeats the request's id and echo bytes (so the server can compute the round trip), then adds
/// the session start in seconds and this side's clock in milliseconds.
///
/// ## Arguments
/// * `payload_type` - [`pt::TIME_REQUEST_32`] or [`pt::TIME_REQUEST_64`]
/// * `request` - the request message, padding and trailer already removed
///
/// ## Returns
/// The reply's payload type and payload, or `None` when the type is not a time request or the request has the
/// wrong length.
///
/// ## Example
/// ```
/// use lunacore::stream::spp::{Clocks, pt, time_reply};
/// let request = [7, 0, 0, 0, 0, 0, 0xE8, 0x03, 0, 0];
/// let (reply_type, reply) = time_reply(pt::TIME_REQUEST_32, &request, Clocks { start_secs: 5, now_ms: 9 }).unwrap();
/// assert_eq!(reply_type, pt::TIME_REPLY_32);
/// assert_eq!(reply.len(), 18);
/// assert_eq!(reply[..10], request);
/// ```
pub fn time_reply(payload_type: u8, request: &[u8], clocks: Clocks) -> Option<(u8, Vec<u8>)> {
    let (reply_type, wide) = match payload_type {
        pt::TIME_REQUEST_32 if request.len() == REQUEST_32_LEN => (pt::TIME_REPLY_32, false),
        pt::TIME_REQUEST_64 if request.len() == REQUEST_64_LEN => (pt::TIME_REPLY_64, true),
        _ => return None,
    };
    let mut reply = request.to_vec();
    reply.extend_from_slice(&clocks.start_secs.to_le_bytes());
    if wide {
        reply.extend_from_slice(&clocks.now_ms.to_le_bytes());
    } else {
        reply.extend_from_slice(&(clocks.now_ms as u32).to_le_bytes());
    }
    Some((reply_type, reply))
}

/// # Payload of a client `PT_RTCP`: `[u32 receive rate][u32 timestamp of the last packet received]`.
///
/// The rate is the client's estimate of the incoming bit rate; its exact unit is not established (bits per
/// second assumed).
///
/// ## Example
/// ```
/// use lunacore::stream::spp::rtcp_payload;
/// assert_eq!(rtcp_payload(1, 2), [1, 0, 0, 0, 2, 0, 0, 0]);
/// ```
pub fn rtcp_payload(rate: u32, last_timestamp: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&rate.to_le_bytes());
    out[4..].copy_from_slice(&last_timestamp.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOCKS: Clocks = Clocks {
        start_secs: 0x0102_0304,
        now_ms: 0x0000_0001_0000_0002,
    };

    #[test]
    fn reply_32_echoes_the_request_then_adds_the_clocks() {
        let request = [0x2A, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        let (kind, reply) = time_reply(pt::TIME_REQUEST_32, &request, CLOCKS).unwrap();
        assert_eq!(kind, pt::TIME_REPLY_32);
        let expected = [
            0x2A, 0, 1, 2, 3, 4, 5, 6, 7, 8, // id and echo, verbatim
            4, 3, 2, 1, // start in seconds
            2, 0, 0, 0, // low 32 bits of the milliseconds
        ];
        assert_eq!(reply, expected);
    }

    #[test]
    fn reply_64_echoes_the_request_then_adds_the_clocks() {
        let request: Vec<u8> = (0..18).collect();
        let (kind, reply) = time_reply(pt::TIME_REQUEST_64, &request, CLOCKS).unwrap();
        assert_eq!(kind, pt::TIME_REPLY_64);
        assert_eq!(reply.len(), 30);
        assert_eq!(reply[..18], request[..]);
        assert_eq!(reply[18..22], [4, 3, 2, 1]);
        assert_eq!(reply[22..], [2, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn requests_of_the_wrong_length_or_type_are_not_answered() {
        assert!(time_reply(pt::TIME_REQUEST_32, &[0; 9], CLOCKS).is_none());
        assert!(time_reply(pt::TIME_REQUEST_64, &[0; 10], CLOCKS).is_none());
        assert!(time_reply(pt::DIAGNOSTIC, &[0; 10], CLOCKS).is_none());
    }

    #[test]
    fn rtcp_is_two_little_endian_words() {
        assert_eq!(
            rtcp_payload(0x0A0B_0C0D, 1),
            [0x0D, 0x0C, 0x0B, 0x0A, 1, 0, 0, 0]
        );
    }
}

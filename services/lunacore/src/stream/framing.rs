//! Framing of application messages inside an SPP message, see `docs/PROTOCOL.md` §4.2.
//!
//! ```text
//! [M u32 LE][P u32 LE][protobuf message, M bytes][zero padding to a multiple of 8][raw payload, P bytes]
//! ```

/// The two length words that start every framed message.
const PREFIX_LEN: usize = 8;
const ALIGN: usize = 8;

/// # Why a received message is not a valid frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Truncated(usize),
    Lengths {
        message: usize,
        raw: usize,
        available: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated(n) => write!(
                f,
                "framed message of {} bytes is shorter than its prefix",
                n
            ),
            Self::Lengths {
                message,
                raw,
                available,
            } => write!(
                f,
                "prefix announces {} protobuf bytes and {} raw bytes but {} follow",
                message, raw, available
            ),
        }
    }
}

/// # One framed message: the protobuf bytes and the optional raw payload (video, audio).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub message: Vec<u8>,
    pub raw: Vec<u8>,
}

impl Frame {
    /// # Serializes the frame as it is sent over SPP.
    ///
    /// ## Example
    /// ```
    /// use lunacore::stream::framing::Frame;
    /// let bytes = Frame { message: vec![1, 2, 3], raw: vec![9] }.encode();
    /// assert_eq!(bytes, [3, 0, 0, 0, 1, 0, 0, 0, 1, 2, 3, 0, 0, 0, 0, 0, 9]);
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let padded = self.message.len().next_multiple_of(ALIGN);
        let mut out = Vec::with_capacity(PREFIX_LEN + padded + self.raw.len());
        out.extend_from_slice(&length_word(self.message.len()));
        out.extend_from_slice(&length_word(self.raw.len()));
        out.extend_from_slice(&self.message);
        out.resize(PREFIX_LEN + padded, 0);
        out.extend_from_slice(&self.raw);
        out
    }

    /// # Parses a reassembled SPP message.
    ///
    /// The byte order is not guessed: a prefix that does not fit the message is an error.
    ///
    /// ## Errors
    /// Fails on a message shorter than 8 bytes or whose announced lengths exceed what follows the prefix.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        let prefix = bytes
            .get(..PREFIX_LEN)
            .ok_or(FrameError::Truncated(bytes.len()))?;
        let message_len = word(&prefix[..4]);
        let raw_len = word(&prefix[4..]);
        let body = &bytes[PREFIX_LEN..];
        let bad = || FrameError::Lengths {
            message: message_len,
            raw: raw_len,
            available: body.len(),
        };
        // The display channel sends its raw payload right after the protobuf, without padding (observed live).
        let unpadded = message_len.checked_add(raw_len) == Some(body.len());
        let raw_start = if unpadded {
            message_len
        } else {
            message_len
                .checked_next_multiple_of(ALIGN)
                .ok_or_else(bad)?
        };
        let message = body.get(..message_len).ok_or_else(bad)?;
        let raw_end = raw_start.checked_add(raw_len).ok_or_else(bad)?;
        // A sender without raw payload may leave the padding out.
        let raw = if raw_len == 0 {
            &[][..]
        } else {
            body.get(raw_start..raw_end).ok_or_else(bad)?
        };
        Ok(Self {
            message: message.to_vec(),
            raw: raw.to_vec(),
        })
    }
}

fn length_word(len: usize) -> [u8; 4] {
    u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes()
}

fn word(bytes: &[u8]) -> usize {
    let mut word = [0u8; 4];
    word.copy_from_slice(bytes);
    u32::from_le_bytes(word) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_only_is_padded_to_eight() {
        let frame = Frame {
            message: vec![0xAB; 5],
            raw: vec![],
        };
        let bytes = frame.encode();
        assert_eq!(bytes[..8], [5, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(bytes.len(), 8 + 8);
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn raw_payload_starts_on_an_eight_byte_boundary() {
        let frame = Frame {
            message: vec![1; 9],
            raw: vec![7, 8, 9],
        };
        let bytes = frame.encode();
        assert_eq!(bytes.len(), 8 + 16 + 3);
        assert_eq!(bytes[8 + 16..], [7, 8, 9]);
        assert_eq!(Frame::decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn an_aligned_message_gets_no_extra_padding() {
        let frame = Frame {
            message: vec![1; 8],
            raw: vec![2],
        };
        assert_eq!(frame.encode().len(), 8 + 8 + 1);
    }

    #[test]
    fn padding_may_be_missing_when_there_is_no_raw_payload() {
        let bytes = [3, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3];
        assert_eq!(Frame::decode(&bytes).unwrap().message, [1, 2, 3]);
    }

    #[test]
    fn raw_payload_without_padding_is_accepted() {
        // Seen on the display channel: 47 protobuf bytes and 753 raw bytes in 800 bytes after the prefix.
        let mut bytes = vec![47, 0, 0, 0, 0xf1, 2, 0, 0];
        bytes.extend(vec![1; 47]);
        bytes.extend(vec![9; 753]);
        let frame = Frame::decode(&bytes).unwrap();
        assert_eq!((frame.message.len(), frame.raw.len()), (47, 753));
        assert!(frame.message.iter().all(|&b| b == 1) && frame.raw.iter().all(|&b| b == 9));
    }

    #[test]
    fn bad_lengths_are_rejected() {
        assert_eq!(Frame::decode(&[1, 2, 3]), Err(FrameError::Truncated(3)));
        let too_long = [200, 0, 0, 0, 0, 0, 0, 0, 1, 2];
        assert!(matches!(
            Frame::decode(&too_long),
            Err(FrameError::Lengths { .. })
        ));
        let raw_too_long = [1, 0, 0, 0, 9, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 1];
        assert!(Frame::decode(&raw_too_long).is_err());
        let big_endian = [0, 0, 0, 5, 0, 0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 0];
        assert!(
            Frame::decode(&big_endian).is_err(),
            "no endianness guessing"
        );
        let huge = [255, 255, 255, 255, 255, 255, 255, 255, 0, 0];
        assert!(Frame::decode(&huge).is_err());
    }

    #[test]
    fn an_empty_frame_round_trips() {
        let bytes = Frame::default().encode();
        assert_eq!(bytes, [0; 8]);
        assert_eq!(Frame::decode(&bytes).unwrap(), Frame::default());
    }
}

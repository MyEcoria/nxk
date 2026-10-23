//! The SPP connect request (`PT_CONN_REQUEST`) and its response, see `docs/PROTOCOL.md` §3.

/// SPP protocol version spoken by the client; the server refuses anything above 2.
pub const SPP_VERSION: u8 = 2;
/// Length of the complete connect payload: version, highest channel, state id and the attribute list.
pub const FULL_LEN: usize = 1030;
/// Length of the short form: version, highest channel and state id.
const SHORT_LEN: usize = 6;

/// # Why a connect attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    BadResponse(Vec<u8>),
    Refused { server_version: u8 },
    Timeout(std::time::Duration),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadResponse(b) => write!(f, "connect response must be 2 bytes, got {:02x?}", b),
            Self::Refused { server_version } => {
                write!(
                    f,
                    "server refused the connection (server SPP version {})",
                    server_version
                )
            }
            Self::Timeout(t) => write!(f, "no connect response within {:?}", t),
        }
    }
}

/// # Which connect payload is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectForm {
    /// 1030 bytes, split in two packets: what the Android client sends, with an empty attribute list.
    #[default]
    Full,
    /// 6 bytes, a single packet.
    Short,
}

/// # The connect message sent on the control channel right after the DTLS handshake.
///
/// Layout: byte 0 SPP version, byte 1 highest channel id in use (0), bytes 2-5 random application state id
/// (little-endian, deduced), then for [`ConnectForm::Full`] the attribute list, here empty, zero-filled to 1030 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectRequest {
    pub form: ConnectForm,
    /// Identifies the connection for a later hand-over; any value works for a first connection.
    pub state_id: u32,
}

impl ConnectRequest {
    /// # Builds a request with a random state id (the caller's random).
    pub fn new(form: ConnectForm, state_id: u32) -> Self {
        Self { form, state_id }
    }

    /// # Serializes the payload.
    ///
    /// ## Example
    /// ```
    /// use lunacore::stream::spp::{ConnectForm, ConnectRequest};
    /// let bytes = ConnectRequest { form: ConnectForm::Short, state_id: 1 }.encode();
    /// assert_eq!(bytes, [2, 0, 1, 0, 0, 0]);
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let len = match self.form {
            ConnectForm::Full => FULL_LEN,
            ConnectForm::Short => SHORT_LEN,
        };
        let mut out = vec![0u8; len];
        out[0] = SPP_VERSION;
        out[2..SHORT_LEN].copy_from_slice(&self.state_id.to_le_bytes());
        out
    }
}

/// # The server's answer to a connect request: `[accepted][server SPP version]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectResponse {
    pub accepted: bool,
    pub server_version: u8,
}

impl ConnectResponse {
    /// # Parses the 2-byte message of a `PT_CONN_RESPONSE`.
    ///
    /// ## Errors
    /// Fails when the message is not exactly 2 bytes or the first one is neither 0 nor 1.
    pub fn decode(message: &[u8]) -> Result<Self, ConnectError> {
        match *message {
            [accepted @ (0 | 1), server_version] => Ok(Self {
                accepted: accepted == 1,
                server_version,
            }),
            _ => Err(ConnectError::BadResponse(message.to_vec())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_request_is_1030_bytes_with_an_empty_attribute_list() {
        let bytes = ConnectRequest {
            form: ConnectForm::Full,
            state_id: 0x0403_0201,
        }
        .encode();
        assert_eq!(bytes.len(), 1030);
        assert_eq!(bytes[..7], [2, 0, 1, 2, 3, 4, 0]);
        assert!(bytes[6..].iter().all(|&b| b == 0));
    }

    #[test]
    fn short_request_is_six_bytes() {
        let bytes = ConnectRequest {
            form: ConnectForm::Short,
            state_id: 0x0403_0201,
        }
        .encode();
        assert_eq!(bytes, [2, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn the_state_id_is_the_callers() {
        assert_eq!(ConnectRequest::new(ConnectForm::Short, 7).state_id, 7);
    }

    #[test]
    fn accepted_and_refused_responses_are_decoded() {
        assert_eq!(
            ConnectResponse::decode(&[1, 2]),
            Ok(ConnectResponse {
                accepted: true,
                server_version: 2
            })
        );
        assert_eq!(
            ConnectResponse::decode(&[0, 1]),
            Ok(ConnectResponse {
                accepted: false,
                server_version: 1
            })
        );
    }

    #[test]
    fn malformed_responses_are_rejected() {
        for bad in [&[][..], &[1], &[1, 2, 3], &[2, 2]] {
            assert!(matches!(
                ConnectResponse::decode(bad),
                Err(ConnectError::BadResponse(_))
            ));
        }
    }
}

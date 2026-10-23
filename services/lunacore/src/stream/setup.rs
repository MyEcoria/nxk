//! Builders and parsers of the `dcv.setup` messages and of the SPP channel request, see `docs/PROTOCOL.md` §4.

use super::framing::Frame;
use super::{Secret, SessionError};
use crate::config;
use crate::proto::setup::{
    self, client_message, server_message, ChannelConnectionConfirm, ChannelConnectionRequest,
    ClientContext, ConnectionConfirm, ConnectionRequest, HardwareInfo, ProtocolVersionNumber,
    SoftwareInfo,
};
use prost::Message;

/// Length of the `PT_CHANNEL_REQUEST` payload: a 64-byte zero-padded name and one flag byte.
pub const CHANNEL_REQUEST_LEN: usize = 65;
const NAME_LEN: usize = 64;
/// Protocol version announced for `main` by the Android client; the per-channel value is not established.
const PROTOCOL_VERSION: (u32, u32) = (1, 1);

/// # Who this client says it is in `ConnectionRequest`.
#[derive(Debug, Clone)]
pub struct ClientIdentity {
    pub name: String,
    pub os: String,
    pub arch: String,
    /// Major, minor, revision, extended.
    pub version: [u32; 4],
    pub device_type: String,
}

impl Default for ClientIdentity {
    /// The Android client's name and version, on this machine's OS and architecture.
    fn default() -> Self {
        let mut numbers = config::CLIENT_VERSION
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|part| part.parse().ok());
        let mut next = || numbers.next().unwrap_or(0);
        Self {
            name: config::APP_NAME.to_owned(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            version: [next(), next(), next(), next()],
            device_type: config::PROXY_DEVICE_TYPE.to_owned(),
        }
    }
}

/// # The name of the SPP channel to ask for when the application channel is `name`.
///
/// The server announces `display` but only answers a display setup sent on an SPP channel requested as `display2`
/// (the name the Android client uses, observed live); the other channels keep their name.
///
/// ## Example
/// ```
/// use lunacore::stream::setup::spp_channel_name;
/// assert_eq!(spp_channel_name("display"), "display2");
/// assert_eq!(spp_channel_name("audio"), "audio");
/// ```
pub fn spp_channel_name(name: &str) -> &str {
    if name == "display" {
        "display2"
    } else {
        name
    }
}

/// # Whether the channel starts with a `ChannelConnectionRequest`.
///
/// The display channel does not (observed live): once its SPP channel is confirmed the server speaks
/// `dcv.display` straight away, which is how the Android client's separate display stack behaves.
pub fn needs_setup(name: &str) -> bool {
    name != "display"
}

/// # The payload of a `PT_CHANNEL_REQUEST`: the channel name, zero-padded to 64 bytes, then a flag byte.
///
/// Bit 0 of the flag asks for in-order delivery; the server's confirmation has the last word.
///
/// ## Errors
/// Fails when the name does not fit in 64 bytes.
pub fn channel_request_payload(name: &str, ordered: bool) -> Result<Vec<u8>, SessionError> {
    if name.len() >= NAME_LEN || name.is_empty() {
        return Err(SessionError::ChannelName(name.to_owned()));
    }
    let mut payload = vec![0u8; CHANNEL_REQUEST_LEN];
    payload[..name.len()].copy_from_slice(name.as_bytes());
    payload[NAME_LEN] = u8::from(ordered);
    Ok(payload)
}

fn version() -> ProtocolVersionNumber {
    ProtocolVersionNumber {
        major: Some(PROTOCOL_VERSION.0),
        minor: Some(PROTOCOL_VERSION.1),
    }
}

/// # The framed `ConnectionRequest` that opens the session on `main`.
pub fn connection_request(session_id: &str, token: &Secret, identity: &ClientIdentity) -> Frame {
    let [major, minor, revision, extended] = identity.version;
    let request = ConnectionRequest {
        session_id: Some(session_id.to_owned()),
        authentication_token: Some(token.expose().to_owned()),
        client_info: Some(SoftwareInfo {
            name: Some(identity.name.clone()),
            version: Some(setup::software_info::VersionNumber {
                major: Some(major),
                minor: Some(minor),
                revision: Some(revision),
                extended: Some(extended),
            }),
            os: Some(identity.os.clone()),
            arch: Some(identity.arch.clone()),
            hostname: None,
        }),
        max_protocol_version: Some(version()),
        hardware_info: Some(HardwareInfo {
            device_type: Some(identity.device_type.clone()),
            device_serial: None,
        }),
        client_context: Some(ClientContext {
            client_type: Some(setup::client_context::ClientType::Computer as i32),
            connection_recipient: Some(setup::client_context::ConnectionRecipient::Player as i32),
        }),
        customer_info: None,
    };
    wrap(client_message::Msg::ConnectionRequest(request))
}

/// # The framed `ChannelConnectionRequest` sent on a freshly opened SPP channel.
pub fn channel_connection_request(
    session_id: &str,
    connection_id: u32,
    channel_name: &str,
    channel_token: &Secret,
) -> Frame {
    let request = ChannelConnectionRequest {
        session_id: Some(session_id.to_owned()),
        connection_id: Some(connection_id),
        channel_name: Some(channel_name.to_owned()),
        authentication_token: Some(channel_token.expose().to_owned()),
        max_protocol_version: Some(version()),
        requested_encryption_mode: Some(
            setup::channel_connection_request::EncryptionMode::ServerDecides as i32,
        ),
    };
    wrap(client_message::Msg::ChannelConnectionRequest(request))
}

fn wrap(msg: client_message::Msg) -> Frame {
    Frame {
        message: setup::ClientMessage { msg: Some(msg) }.encode_to_vec(),
        raw: Vec::new(),
    }
}

/// What a channel in its setup phase can receive.
fn server_msg(frame: &Frame) -> Result<server_message::Msg, SessionError> {
    setup::ServerMessage::decode(frame.message.as_slice())
        .map_err(|e| SessionError::Decode(e.to_string()))?
        .msg
        .ok_or_else(|| SessionError::Unexpected("empty setup message".into()))
}

/// # Reads the server's answer to `ConnectionRequest`.
///
/// ## Errors
/// [`SessionError::Aborted`] for a `ConnectionAbort`, [`SessionError::Unexpected`] for anything else.
pub fn parse_connection_answer(frame: &Frame) -> Result<ConnectionConfirm, SessionError> {
    match server_msg(frame)? {
        server_message::Msg::ConnectionConfirm(confirm) => Ok(confirm),
        server_message::Msg::ConnectionAbort(abort) => {
            Err(SessionError::Aborted(abort.reason.map_or_else(
                || "no reason given".to_owned(),
                |r| {
                    setup::connection_abort::AbortingReason::try_from(r)
                        .map_or_else(|_| format!("unknown reason {r}"), |r| format!("{r:?}"))
                },
            )))
        }
        server_message::Msg::ChannelConnectionConfirm(_) => Err(SessionError::Unexpected(
            "channel confirmation before the connection confirmation".into(),
        )),
    }
}

/// # Reads the server's answer to `ChannelConnectionRequest`.
///
/// ## Errors
/// [`SessionError::Aborted`] for a `ConnectionAbort`, [`SessionError::Unexpected`] for anything else.
pub fn parse_channel_answer(frame: &Frame) -> Result<ChannelConnectionConfirm, SessionError> {
    match server_msg(frame)? {
        server_message::Msg::ChannelConnectionConfirm(confirm) => Ok(confirm),
        server_message::Msg::ConnectionAbort(abort) => Err(SessionError::Aborted(format!(
            "channel refused ({:?})",
            abort.reason
        ))),
        server_message::Msg::ConnectionConfirm(_) => Err(SessionError::Unexpected(
            "connection confirmed twice".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::setup::{connection_abort::AbortingReason, ConnectionAbort, ServerMessage};

    fn frame_of(msg: server_message::Msg) -> Frame {
        Frame {
            message: ServerMessage { msg: Some(msg) }.encode_to_vec(),
            raw: vec![],
        }
    }

    #[test]
    fn channel_request_is_a_padded_name_and_a_flag() {
        let payload = channel_request_payload("main", true).unwrap();
        assert_eq!(payload.len(), 65);
        assert_eq!(&payload[..4], b"main");
        assert!(payload[4..64].iter().all(|b| *b == 0));
        assert_eq!(payload[64], 1);
        assert_eq!(channel_request_payload("display", false).unwrap()[64], 0);
    }

    #[test]
    fn display_is_requested_as_display2() {
        assert_eq!(spp_channel_name("display"), "display2");
        assert_eq!(spp_channel_name("display2"), "display2");
        assert_eq!(spp_channel_name("input"), "input");
    }

    #[test]
    fn channel_names_must_fit() {
        assert!(channel_request_payload("", true).is_err());
        assert!(channel_request_payload(&"x".repeat(64), true).is_err());
        assert!(channel_request_payload(&"x".repeat(63), true).is_ok());
    }

    #[test]
    fn connection_request_carries_the_ids_and_the_computer_context() {
        let frame = connection_request("S1", &Secret::new("T1"), &ClientIdentity::default());
        let sent = setup::ClientMessage::decode(frame.message.as_slice()).unwrap();
        let Some(client_message::Msg::ConnectionRequest(request)) = sent.msg else {
            panic!("not a connection request");
        };
        assert_eq!(request.session_id.as_deref(), Some("S1"));
        assert_eq!(request.authentication_token.as_deref(), Some("T1"));
        assert_eq!(request.max_protocol_version, Some(version()));
        let context = request.client_context.unwrap();
        assert_eq!(context.client_type, Some(1));
        assert_eq!(context.connection_recipient, Some(1));
        assert_eq!(
            request.hardware_info.unwrap().device_type.as_deref(),
            Some(config::PROXY_DEVICE_TYPE)
        );
        assert_eq!(request.client_info.unwrap().version.unwrap().major, Some(1));
    }

    #[test]
    fn default_identity_follows_the_android_version() {
        assert_eq!(ClientIdentity::default().version, [1, 14002, 21, 0]);
    }

    #[test]
    fn channel_connection_request_uses_the_connection_id_and_the_channel_token() {
        let frame = channel_connection_request("S1", 42, "audio", &Secret::new("CT"));
        let sent = setup::ClientMessage::decode(frame.message.as_slice()).unwrap();
        let Some(client_message::Msg::ChannelConnectionRequest(request)) = sent.msg else {
            panic!("not a channel connection request");
        };
        assert_eq!(request.connection_id, Some(42));
        assert_eq!(request.channel_name.as_deref(), Some("audio"));
        assert_eq!(request.authentication_token.as_deref(), Some("CT"));
        assert_eq!(request.requested_encryption_mode, Some(0));
    }

    #[test]
    fn an_abort_names_its_reason() {
        let abort = frame_of(server_message::Msg::ConnectionAbort(ConnectionAbort {
            reason: Some(AbortingReason::AuthenticationFailed as i32),
        }));
        let err = parse_connection_answer(&abort).unwrap_err();
        assert!(err.to_string().contains("AuthenticationFailed"), "{err}");
    }

    #[test]
    fn a_confirmation_is_returned() {
        let confirm = frame_of(server_message::Msg::ConnectionConfirm(ConnectionConfirm {
            connection_id: Some(9),
            ..ConnectionConfirm::default()
        }));
        assert_eq!(
            parse_connection_answer(&confirm).unwrap().connection_id,
            Some(9)
        );
        assert!(parse_channel_answer(&confirm).is_err());
    }

    #[test]
    fn garbage_is_a_decode_error_not_a_panic() {
        let garbage = Frame {
            message: vec![0xFF; 6],
            raw: vec![],
        };
        assert!(matches!(
            parse_connection_answer(&garbage),
            Err(SessionError::Decode(_))
        ));
    }
}

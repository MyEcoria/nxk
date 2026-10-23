//! The `dcv.audio` messages of this client's side of the exchange
//! (docs/PROTOCOL.md §6.1 of the desktop client): Opus offered in mono and
//! stereo, the configuration, the stream started when offered, the packets.

use crate::proto::audio::{
    AudioPacket, AudioService, AudioStatus, ClientMessage, Codecs, Configuration, StartService,
    client_message, codecs, server_message,
};
use crate::stream::framing::Frame;
use prost::Message;

/// Profiles announced for Opus, in the protocol's order: the first two cover mono and stereo.
const PROFILES: [&str; 2] = ["mono", "generic"];

/// # What the server said on the audio channel.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// The server waits for the client's codec list.
    HandshakeStart,
    Configuration(Configuration),
    /// The server offers the audio stream: the client must ask for it.
    StreamAvailable,
    /// Header of an audio packet; the Opus bytes are the frame's raw payload.
    Packet(AudioPacket),
    /// Any other message (levels, spectrum, other services): not needed for playback.
    Ignored,
}

/// # The client's codec list: Opus with the mono and stereo profiles.
///
/// ## Example
/// ```
/// use lunacore::proto::audio::client_message::Msg;
/// let Some(Msg::Codecs(codecs)) = lunacore::stream::audio::codecs_message().msg else { panic!() };
/// assert_eq!(codecs.audio_codecs[0].name.as_deref(), Some("opus"));
/// ```
pub fn codecs_message() -> ClientMessage {
    ClientMessage {
        msg: Some(client_message::Msg::Codecs(Codecs {
            audio_codecs: vec![codecs::Codec {
                name: Some("opus".to_owned()),
                audio_profiles: PROFILES.map(str::to_owned).to_vec(),
            }],
        })),
    }
}

/// # The request to start the playback stream.
pub fn start_stream_message() -> ClientMessage {
    ClientMessage {
        msg: Some(client_message::Msg::StartService(StartService {
            service: Some(AudioService::Stream as i32),
        })),
    }
}

/// # Classifies a frame received on the audio channel.
///
/// ## Errors
/// Fails if the protobuf part does not decode.
/// A message of the audio channel that did not decode.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeError(pub String);

pub fn parse(frame: &Frame) -> Result<ServerEvent, DecodeError> {
    let message = crate::proto::audio::ServerMessage::decode(frame.message.as_slice())
        .map_err(|e| DecodeError(e.to_string()))?;
    Ok(match message.msg {
        Some(server_message::Msg::HandshakeStart(_)) => ServerEvent::HandshakeStart,
        Some(server_message::Msg::Configuration(c)) => ServerEvent::Configuration(c),
        Some(server_message::Msg::AudioPacket(p)) => ServerEvent::Packet(p),
        Some(server_message::Msg::StatusUpdate(update)) => stream_offer(&update),
        _ => ServerEvent::Ignored,
    })
}

/// The Android client reacts to `STREAM` becoming `AVAILABLE` (not yet `ACTIVE`) by requesting it. The server
/// leaves `service` out for `STREAM` (value 0, the protobuf default), observed live.
fn stream_offer(update: &crate::proto::audio::StatusUpdate) -> ServerEvent {
    let offered = update.status.iter().any(|s| {
        s.service.unwrap_or(AudioService::Stream as i32) == AudioService::Stream as i32
            && s.status == Some(AudioStatus::Available as i32)
    });
    if offered {
        ServerEvent::StreamAvailable
    } else {
        ServerEvent::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::audio::{ServerMessage, StatusUpdate, status_update::ServiceStatus};

    fn frame(msg: server_message::Msg) -> Frame {
        Frame {
            message: ServerMessage { msg: Some(msg) }.encode_to_vec(),
            raw: Vec::new(),
        }
    }

    #[test]
    fn codecs_announce_opus_mono_and_generic() {
        let Some(client_message::Msg::Codecs(codecs)) = codecs_message().msg else {
            panic!("not a codec list")
        };
        assert_eq!(codecs.audio_codecs.len(), 1);
        assert_eq!(codecs.audio_codecs[0].name.as_deref(), Some("opus"));
        assert_eq!(codecs.audio_codecs[0].audio_profiles, ["mono", "generic"]);
    }

    #[test]
    fn handshake_start_is_recognised() {
        let hello = frame(server_message::Msg::HandshakeStart(Default::default()));
        assert_eq!(parse(&hello).unwrap(), ServerEvent::HandshakeStart);
    }

    #[test]
    fn available_stream_is_an_offer_and_active_is_not() {
        let update = |status: AudioStatus| {
            frame(server_message::Msg::StatusUpdate(StatusUpdate {
                status: vec![ServiceStatus {
                    service: Some(AudioService::Stream as i32),
                    status: Some(status as i32),
                }],
            }))
        };
        assert_eq!(
            parse(&update(AudioStatus::Available)).unwrap(),
            ServerEvent::StreamAvailable
        );
        assert_eq!(
            parse(&update(AudioStatus::Active)).unwrap(),
            ServerEvent::Ignored
        );
    }

    #[test]
    fn stream_without_a_service_field_is_still_the_stream() {
        let observed = frame(server_message::Msg::StatusUpdate(StatusUpdate {
            status: vec![
                ServiceStatus {
                    service: None,
                    status: Some(AudioStatus::Available as i32),
                },
                ServiceStatus {
                    service: Some(AudioService::Injection as i32),
                    status: Some(AudioStatus::Available as i32),
                },
            ],
        }));
        assert_eq!(parse(&observed).unwrap(), ServerEvent::StreamAvailable);
    }

    #[test]
    fn injection_alone_is_not_a_stream_offer() {
        let mic = frame(server_message::Msg::StatusUpdate(StatusUpdate {
            status: vec![ServiceStatus {
                service: Some(AudioService::Injection as i32),
                status: Some(AudioStatus::Available as i32),
            }],
        }));
        assert_eq!(parse(&mic).unwrap(), ServerEvent::Ignored);
    }

    #[test]
    fn garbage_is_a_decode_error() {
        let bad = Frame {
            message: vec![0xFF, 0xFF, 0xFF],
            raw: Vec::new(),
        };
        assert!(matches!(parse(&bad), Err(DecodeError(_))));
    }
}

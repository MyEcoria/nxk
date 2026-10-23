//! The `display` channel of this client's side of the exchange (the desktop
//! client's `docs/PROTOCOL.md` §5): a framed message becomes a typed
//! [`DisplayMessage`], the server's capabilities are answered, a video frame
//! is one `tile_update` with the encoded access unit (Annex B) as its raw
//! payload. Everything the server sends is untrusted and checked here.

use crate::proto::display::{
    Ack2, BatchedAck2, ClientBody, ClientMessage, Codec, Display, DisplaySessionConfiguration,
    HandshakeStart, Ping, Pong, Probe, Ready, Resolution, ServerBody, ServerMessage, StreamEnd,
    StreamStart, TileUpdate, VideoCodec,
};
use crate::stream::framing::Frame;
use prost::Message;

// ---- Annex B

/// NAL unit type of a coded slice of an IDR picture.
pub const NAL_IDR: u8 = 5;
/// NAL unit type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL unit type of a picture parameter set.
pub const NAL_PPS: u8 = 8;

/// # `nal_unit_type` of every NAL unit of an Annex B stream, in order.
///
/// A start code is `00 00 01`, optionally preceded by one more zero byte. Bytes before the first start code are
/// ignored; a stream without start code gives an empty list.
///
/// ## Example
/// ```
/// use lunacore::stream::display::nal_types;
/// let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
/// assert_eq!(nal_types(&stream), [7, 8, 5]);
/// ```
pub fn nal_types(stream: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while let Some(start) = find_start_code(stream, i) {
        let header = start + 3;
        if let Some(byte) = stream.get(header) {
            types.push(byte & 0x1f);
        }
        i = header;
    }
    types
}

/// # Whether the access unit can start a decoder: it holds an IDR slice.
pub fn is_key_frame(stream: &[u8]) -> bool {
    nal_types(stream).contains(&NAL_IDR)
}

/// Position of the next `00 00 01` at or after `from`.
fn find_start_code(stream: &[u8], from: usize) -> Option<usize> {
    stream
        .get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|p| p + from)
}

// ---- Depacketization

/// # Why a message of the `display` channel was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpackError {
    Decode(String),
    EmptyPayload(u64),
    SizeMismatch {
        frame: u64,
        declared: u32,
        actual: usize,
    },
    BadSliceOffsets {
        frame: u64,
    },
    SliceCount {
        frame: u64,
        declared: u32,
        offsets: usize,
    },
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "could not decode the display message: {}", e),
            Self::EmptyPayload(n) => write!(f, "video frame {} has no payload", n),
            Self::SizeMismatch {
                frame,
                declared,
                actual,
            } => write!(
                f,
                "video frame {}: frame_size {} but {} payload bytes",
                frame, declared, actual
            ),
            Self::BadSliceOffsets { frame } => write!(
                f,
                "video frame {}: slice offsets are not ascending and inside the payload",
                frame
            ),
            Self::SliceCount {
                frame,
                declared,
                offsets,
            } => write!(
                f,
                "video frame {}: {} slices declared but {} offsets given",
                frame, declared, offsets
            ),
        }
    }
}

impl std::error::Error for UnpackError {}

/// # One encoded video frame with its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub frame_index: u64,
    pub stream_id: u32,
    /// Size of the encoded picture, when the server states it.
    pub stream_resolution: Option<Resolution>,
    pub capture_resolution: Option<Resolution>,
    /// Server timestamp of the frame (unit not established).
    pub timestamp: u64,
    /// The encoded access unit.
    pub data: Vec<u8>,
    /// Start of each slice inside `data`, ascending; empty when the frame is not sliced.
    pub slice_offsets: Vec<usize>,
    /// Payload bytes beyond `frame_size`, cut off (expected 0).
    pub trailing_bytes: usize,
}

/// # A message of the `display` channel, from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayMessage {
    Handshake(HandshakeStart),
    Session(DisplaySessionConfiguration),
    StreamStart(StreamStart),
    StreamEnd(StreamEnd),
    Video(EncodedFrame),
    /// A frame older than one already accepted on the same stream: dropped.
    Stale {
        frame_index: u64,
        newest: u64,
    },
    Ping(Ping),
    Probe(Probe),
    /// A message this client does not read, with the number of its `oneof` field.
    Other(u32),
}

/// # Turns the channel's frames into [`DisplayMessage`]s and remembers the last frame index of the stream.
#[derive(Debug, Default)]
pub struct Unpacker {
    stream: Option<(u32, u64)>,
}

impl Unpacker {
    /// # Decodes one frame of the channel.
    ///
    /// ## Errors
    /// Fails when the protobuf does not decode or a video frame's metadata contradicts its payload; the caller
    /// should log it and carry on with the next frame.
    pub fn unpack(&mut self, frame: Frame) -> Result<DisplayMessage, UnpackError> {
        let message = ServerMessage::decode(frame.message.as_slice())
            .map_err(|e| UnpackError::Decode(e.to_string()))?;
        Ok(match message.msg {
            Some(ServerBody::HandshakeStart(m)) => DisplayMessage::Handshake(m),
            Some(ServerBody::DisplaySession(m)) => DisplayMessage::Session(m),
            Some(ServerBody::StreamStart(m)) => DisplayMessage::StreamStart(m),
            Some(ServerBody::StreamEnd(m)) => DisplayMessage::StreamEnd(m),
            Some(ServerBody::Ping(m)) => DisplayMessage::Ping(m),
            Some(ServerBody::Probe(m)) => DisplayMessage::Probe(m),
            Some(ServerBody::TileUpdate(tile)) => self.video(tile, frame.raw)?,
            None => DisplayMessage::Other(first_field_number(&frame.message)),
        })
    }

    fn video(&mut self, tile: TileUpdate, raw: Vec<u8>) -> Result<DisplayMessage, UnpackError> {
        let encoded = encoded_frame(tile, raw)?;
        let stream = encoded.stream_id;
        match self.stream {
            Some((id, newest)) if id == stream && encoded.frame_index < newest => {
                return Ok(DisplayMessage::Stale {
                    frame_index: encoded.frame_index,
                    newest,
                });
            }
            _ => self.stream = Some((stream, encoded.frame_index)),
        }
        Ok(DisplayMessage::Video(encoded))
    }
}

/// Validates a `TileUpdate` against its payload.
fn encoded_frame(tile: TileUpdate, mut raw: Vec<u8>) -> Result<EncodedFrame, UnpackError> {
    let frame = tile.frame_index;
    if raw.is_empty() {
        return Err(UnpackError::EmptyPayload(frame));
    }
    let mut trailing = 0;
    if tile.frame_size != 0 {
        let declared = tile.frame_size as usize;
        if declared > raw.len() {
            return Err(UnpackError::SizeMismatch {
                frame,
                declared: tile.frame_size,
                actual: raw.len(),
            });
        }
        trailing = raw.len() - declared;
        raw.truncate(declared);
    }
    let slice_offsets = slice_offsets(&tile, raw.len())?;
    Ok(EncodedFrame {
        frame_index: frame,
        stream_id: tile.stream_id,
        stream_resolution: tile.stream_resolution,
        capture_resolution: tile.capture_resolution,
        timestamp: tile.timestamp,
        data: raw,
        slice_offsets,
        trailing_bytes: trailing,
    })
}

fn slice_offsets(tile: &TileUpdate, payload_len: usize) -> Result<Vec<usize>, UnpackError> {
    let frame = tile.frame_index;
    let offsets = &tile.frame_slice_offsets;
    if offsets.is_empty() {
        return Ok(Vec::new());
    }
    if tile.frame_slices != 0 && tile.frame_slices as usize != offsets.len() {
        return Err(UnpackError::SliceCount {
            frame,
            declared: tile.frame_slices,
            offsets: offsets.len(),
        });
    }
    let ascending = offsets.windows(2).all(|pair| pair[0] < pair[1]);
    let inside = offsets
        .last()
        .is_some_and(|&last| (last as usize) < payload_len);
    if !(ascending && inside) {
        return Err(UnpackError::BadSliceOffsets { frame });
    }
    Ok(offsets.iter().map(|&o| o as usize).collect())
}

/// Number of the first protobuf field of `bytes` (the `oneof` arm), 0 when it cannot be read.
fn first_field_number(bytes: &[u8]) -> u32 {
    let mut key = 0u64;
    for (i, byte) in bytes.iter().take(5).enumerate() {
        key |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return u32::try_from(key >> 3).unwrap_or(0);
        }
    }
    0
}

// ---- Replies

/// H.264 profile and level announced to the server: the values `createGameSessionJob` already carries
/// (`api::models::create_job_request`, MediaCodec constants: High profile, level 5.2).
const AVC_PROFILE: u32 = 8;
const AVC_LEVEL: u32 = 65536;

/// # What the client announces about the display and the video it can decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub min_bits_per_second: u32,
    pub max_bits_per_second: u32,
    pub start_bits_per_second: u32,
}

impl Default for DisplayConfig {
    /// The console's panel, and a bit rate cap below the desktop client's 20 Mbit/s: the server sends
    /// 1080p whatever is announced, and the decoder's cost follows the bit rate.
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            refresh_rate: 60,
            min_bits_per_second: 2_000_000,
            max_bits_per_second: 12_000_000,
            start_bits_per_second: 6_000_000,
        }
    }
}

/// # The `display_session` message: capabilities offered in answer to `handshake_start`.
pub fn display_session(config: &DisplayConfig, negotiation_id: u64) -> ClientMessage {
    let resolution = Resolution {
        width: config.width,
        height: config.height,
    };
    client(ClientBody::DisplaySession(DisplaySessionConfiguration {
        negotiation_id,
        capture_resolution: Some(resolution),
        display: Some(Display {
            resolution: Some(resolution),
            refresh_rate: config.refresh_rate,
            safe_zones: None,
        }),
        codecs: vec![VideoCodec {
            video_codec: Codec::Avc as i32,
            profile: AVC_PROFILE,
            level: AVC_LEVEL,
            tier: 0,
            max_reference_frames_num: 0,
        }],
        features: Vec::new(),
        min_bits_per_second: config.min_bits_per_second,
        max_bits_per_second: config.max_bits_per_second,
        start_bits_per_second: config.start_bits_per_second,
        clamping_mode: 0,
    }))
}

/// # The `ready` message, with the highest bit rate the client accepts.
pub fn ready(config: &DisplayConfig, negotiation_id: u64) -> ClientMessage {
    client(ClientBody::Ready(Ready {
        negotiation_id,
        max_bit_rate: config.max_bits_per_second,
    }))
}

/// # The answer to a server `ping`.
pub fn pong(ping: &Ping) -> ClientMessage {
    client(ClientBody::Pong(Pong {
        ping_timestamp: ping.ping_timestamp,
    }))
}

/// # Acknowledges a received frame (message and frame ack), `received_usec` on the client's clock.
pub fn frame_ack(stream_id: u32, frame_index: u64, received_usec: u64) -> ClientMessage {
    ack_batch(
        received_usec,
        Ack2 {
            stream_id: u64::from(stream_id),
            frame_index,
            message_ack: true,
            received_usec,
            frame_ack: true,
            force_i_frame: false,
            current_stream_id: u64::from(stream_id),
        },
    )
}

/// # Asks the server for a new key frame, after a decoding error or a gap.
pub fn keyframe_request(stream_id: u32, frame_index: u64, now_usec: u64) -> ClientMessage {
    ack_batch(
        now_usec,
        Ack2 {
            stream_id: u64::from(stream_id),
            frame_index,
            message_ack: true,
            received_usec: now_usec,
            frame_ack: true,
            force_i_frame: true,
            current_stream_id: u64::from(stream_id),
        },
    )
}

/// # The automatic replies to a server message.
///
/// `handshake_start` gets the capabilities, `ping` a `pong`, a video frame its acknowledgement.
pub fn replies(
    message: &DisplayMessage,
    config: &DisplayConfig,
    now_usec: u64,
) -> Vec<ClientMessage> {
    match message {
        DisplayMessage::Handshake(start) => {
            vec![display_session(config, start.negotiation_id)]
        }
        DisplayMessage::Ping(ping) => vec![pong(ping)],
        DisplayMessage::Video(frame) => {
            vec![frame_ack(frame.stream_id, frame.frame_index, now_usec)]
        }
        _ => Vec::new(),
    }
}

fn client(body: ClientBody) -> ClientMessage {
    ClientMessage { msg: Some(body) }
}

fn ack_batch(sent_usec: u64, ack: Ack2) -> ClientMessage {
    client(ClientBody::Acks(BatchedAck2 {
        sent_usec,
        acks: vec![ack],
    }))
}

// ---- The channel's state

use std::time::{Duration, Instant};

/// How long the server may stay silent after `display_session` before `ready` is sent as well.
pub const READY_DELAY: Duration = Duration::from_secs(2);
/// Key frames are asked for at most this often: the server answers with an IDR, which takes a while.
pub const KEYFRAME_EVERY: Duration = Duration::from_millis(500);

/// What came on the channel since the start.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub frames: u64,
    pub bytes: u64,
    pub key_frames: u64,
    /// Frames of the stream's numbering that never came.
    pub missing: u64,
    pub stale: u64,
    /// Messages that did not decode or contradicted their payload.
    pub rejected: u64,
    pub keyframe_requests: u64,
}

/// What a frame of the channel leads to.
#[derive(Debug, Default)]
pub struct Step {
    /// To send back on the channel.
    pub replies: Vec<ClientMessage>,
    /// A video frame for the decoder.
    pub video: Option<EncodedFrame>,
}

/// The `display` channel's state: replies, frame numbering, key frame requests.
pub struct Channel {
    unpacker: Unpacker,
    config: DisplayConfig,
    started: Instant,
    negotiation_id: u64,
    ready_at: Option<Instant>,
    /// Stream and index of the newest frame given to the decoder.
    newest: Option<(u32, u64)>,
    last_keyframe_ask: Option<Instant>,
    pub stats: Stats,
}

impl Channel {
    pub fn new(config: DisplayConfig, now: Instant) -> Self {
        Self {
            unpacker: Unpacker::default(),
            config,
            started: now,
            negotiation_id: 0,
            ready_at: None,
            newest: None,
            last_keyframe_ask: None,
            stats: Stats::default(),
        }
    }

    fn now_usec(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_micros() as u64
    }

    /// A frame of the channel: the answers, and the video for the decoder.
    pub fn on_frame(&mut self, frame: Frame, now: Instant) -> Step {
        let message = match self.unpacker.unpack(frame) {
            Ok(m) => m,
            Err(_) => {
                self.stats.rejected += 1;
                return Step::default();
            }
        };
        match &message {
            DisplayMessage::Handshake(start) => {
                self.negotiation_id = start.negotiation_id;
                self.ready_at = Some(now + READY_DELAY);
            }
            DisplayMessage::Session(_)
            | DisplayMessage::StreamStart(_)
            | DisplayMessage::Video(_) => self.ready_at = None,
            DisplayMessage::Stale { .. } => self.stats.stale += 1,
            _ => {}
        }
        let mut step = Step {
            replies: replies(&message, &self.config, self.now_usec(now)),
            video: None,
        };
        if let DisplayMessage::Video(f) = message {
            self.stats.frames += 1;
            self.stats.bytes += f.data.len() as u64;
            if is_key_frame(&f.data) {
                self.stats.key_frames += 1;
            }
            let gap = match self.newest {
                Some((id, last)) if id == f.stream_id && f.frame_index > last + 1 => {
                    f.frame_index - last - 1
                }
                _ => 0,
            };
            self.newest = Some((f.stream_id, f.frame_index));
            if gap > 0 {
                self.stats.missing += gap;
                step.replies.extend(self.keyframe_request(now));
            }
            step.video = Some(f);
        }
        step
    }

    /// `ready`, once the server stayed silent after our capabilities.
    pub fn on_tick(&mut self, now: Instant) -> Vec<ClientMessage> {
        match self.ready_at {
            Some(at) if now >= at => {
                self.ready_at = None;
                vec![ready(&self.config, self.negotiation_id)]
            }
            _ => Vec::new(),
        }
    }

    /// A key frame asked of the server (the decoder lost its references), unless one was asked
    /// a moment ago or no frame came yet.
    pub fn keyframe_request(&mut self, now: Instant) -> Option<ClientMessage> {
        let (stream, index) = self.newest?;
        if self
            .last_keyframe_ask
            .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_EVERY)
        {
            return None;
        }
        self.last_keyframe_ask = Some(now);
        self.stats.keyframe_requests += 1;
        Some(keyframe_request(stream, index, self.now_usec(now)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::display::Rect;

    fn tile(frame_index: u64, stream_id: u32, size: u32) -> TileUpdate {
        TileUpdate {
            frame_index,
            stream_id,
            rect: Some(Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            }),
            capture_resolution: None,
            stream_resolution: Some(Resolution {
                width: 1920,
                height: 1080,
            }),
            timestamp: 7,
            frame_slices: 0,
            frame_slice_offsets: Vec::new(),
            frame_size: size,
        }
    }

    fn framed(body: ServerBody, raw: &[u8]) -> Frame {
        Frame {
            message: ServerMessage { msg: Some(body) }.encode_to_vec(),
            raw: raw.to_vec(),
        }
    }

    fn video(update: TileUpdate, raw: &[u8]) -> Frame {
        framed(ServerBody::TileUpdate(update), raw)
    }

    #[test]
    fn a_valid_frame_keeps_its_metadata_and_payload() {
        let payload = [0, 0, 0, 1, 0x65, 9, 9];
        let got = Unpacker::default()
            .unpack(video(tile(3, 1, 7), &payload))
            .unwrap();
        let DisplayMessage::Video(frame) = got else {
            panic!("{got:?}")
        };
        assert_eq!(
            (frame.frame_index, frame.stream_id, frame.timestamp),
            (3, 1, 7)
        );
        assert_eq!(frame.data, payload);
        assert_eq!(
            frame.stream_resolution,
            Some(Resolution {
                width: 1920,
                height: 1080
            })
        );
        assert!(frame.slice_offsets.is_empty());
    }

    #[test]
    fn padding_after_frame_size_is_cut() {
        let got = Unpacker::default()
            .unpack(video(tile(1, 1, 4), &[1, 2, 3, 4, 0, 0, 0, 0]))
            .unwrap();
        let DisplayMessage::Video(frame) = got else {
            panic!("{got:?}")
        };
        assert_eq!(frame.data, [1, 2, 3, 4]);
        assert_eq!(frame.trailing_bytes, 4);
    }

    #[test]
    fn a_missing_frame_size_takes_the_whole_payload() {
        let got = Unpacker::default()
            .unpack(video(tile(1, 1, 0), &[1, 2, 3]))
            .unwrap();
        assert!(matches!(got, DisplayMessage::Video(f) if f.data == [1, 2, 3]));
    }

    #[test]
    fn a_sliced_frame_exposes_its_offsets() {
        let mut update = tile(1, 1, 10);
        update.frame_slices = 3;
        update.frame_slice_offsets = vec![0, 4, 7];
        let got = Unpacker::default().unpack(video(update, &[0; 10])).unwrap();
        assert!(matches!(got, DisplayMessage::Video(f) if f.slice_offsets == [0, 4, 7]));
    }

    #[test]
    fn bad_slice_offsets_are_rejected() {
        for offsets in [vec![4, 4], vec![5, 2], vec![0, 10], vec![0, 99]] {
            let mut update = tile(1, 1, 10);
            update.frame_slice_offsets = offsets.clone();
            let got = Unpacker::default().unpack(video(update, &[0; 10]));
            assert_eq!(
                got,
                Err(UnpackError::BadSliceOffsets { frame: 1 }),
                "{offsets:?}"
            );
        }
    }

    #[test]
    fn a_slice_count_that_disagrees_is_rejected() {
        let mut update = tile(1, 1, 10);
        update.frame_slices = 2;
        update.frame_slice_offsets = vec![0, 4, 7];
        let got = Unpacker::default().unpack(video(update, &[0; 10]));
        assert!(matches!(
            got,
            Err(UnpackError::SliceCount { offsets: 3, .. })
        ));
    }

    #[test]
    fn a_frame_size_larger_than_the_payload_is_rejected() {
        let got = Unpacker::default().unpack(video(tile(5, 1, 100), &[0; 10]));
        assert_eq!(
            got,
            Err(UnpackError::SizeMismatch {
                frame: 5,
                declared: 100,
                actual: 10
            })
        );
    }

    #[test]
    fn a_frame_without_payload_is_rejected() {
        let got = Unpacker::default().unpack(video(tile(5, 1, 0), &[]));
        assert_eq!(got, Err(UnpackError::EmptyPayload(5)));
    }

    #[test]
    fn an_older_frame_of_the_same_stream_is_stale_but_a_new_stream_restarts() {
        let mut unpacker = Unpacker::default();
        unpacker.unpack(video(tile(10, 1, 1), &[1])).unwrap();
        assert_eq!(
            unpacker.unpack(video(tile(9, 1, 1), &[1])),
            Ok(DisplayMessage::Stale {
                frame_index: 9,
                newest: 10
            })
        );
        assert!(matches!(
            unpacker.unpack(video(tile(1, 2, 1), &[1])),
            Ok(DisplayMessage::Video(_))
        ));
    }

    #[test]
    fn control_messages_are_not_video() {
        let mut unpacker = Unpacker::default();
        let start = framed(
            ServerBody::HandshakeStart(HandshakeStart { negotiation_id: 9 }),
            &[],
        );
        assert_eq!(
            unpacker.unpack(start),
            Ok(DisplayMessage::Handshake(HandshakeStart {
                negotiation_id: 9
            }))
        );
        let ping = framed(ServerBody::Ping(Ping { ping_timestamp: 5 }), &[]);
        assert_eq!(
            unpacker.unpack(ping),
            Ok(DisplayMessage::Ping(Ping { ping_timestamp: 5 }))
        );
    }

    #[test]
    fn an_unread_arm_reports_its_field_number() {
        // display_layout (field 20, empty) and a field 650 whose key takes two bytes.
        let layout = Frame {
            message: vec![0xa2, 0x01, 0x00],
            raw: Vec::new(),
        };
        assert_eq!(
            Unpacker::default().unpack(layout),
            Ok(DisplayMessage::Other(20))
        );
        let wide = Frame {
            message: vec![0xd2, 0x28, 0x00],
            raw: Vec::new(),
        };
        assert_eq!(
            Unpacker::default().unpack(wide),
            Ok(DisplayMessage::Other(650))
        );
    }

    #[test]
    fn garbage_is_a_decode_error() {
        let garbage = Frame {
            message: vec![0xff; 6],
            raw: Vec::new(),
        };
        assert!(matches!(
            Unpacker::default().unpack(garbage),
            Err(UnpackError::Decode(_))
        ));
    }

    fn round_trip(message: &ClientMessage) -> ClientMessage {
        ClientMessage::decode(message.encode_to_vec().as_slice()).unwrap()
    }

    #[test]
    fn handshake_start_is_answered_with_the_capabilities() {
        let config = DisplayConfig::default();
        let start = DisplayMessage::Handshake(HandshakeStart { negotiation_id: 42 });
        let [reply] = replies(&start, &config, 0).try_into().unwrap();
        let Some(ClientBody::DisplaySession(session)) = round_trip(&reply).msg else {
            panic!("not a display_session")
        };
        assert_eq!(session.negotiation_id, 42);
        assert_eq!(session.codecs.len(), 1);
        assert_eq!(session.codecs[0].video_codec, Codec::Avc as i32);
        assert_eq!(
            (session.codecs[0].profile, session.codecs[0].level),
            (8, 65536)
        );
        assert_eq!(
            session.display.and_then(|d| d.resolution),
            Some(Resolution {
                width: 1280,
                height: 720
            })
        );
        assert_eq!(session.max_bits_per_second, 12_000_000);
    }

    #[test]
    fn the_avc_codec_is_not_written_on_the_wire() {
        // AVC is 0, the proto3 default: the codec entry only carries profile and level.
        let codec = VideoCodec {
            video_codec: 0,
            profile: 8,
            level: 65536,
            tier: 0,
            max_reference_frames_num: 0,
        };
        assert_eq!(codec.encode_to_vec()[0], 0x10);
        let message = display_session(&DisplayConfig::default(), 1);
        let Some(ClientBody::DisplaySession(session)) = round_trip(&message).msg else {
            panic!()
        };
        assert_eq!(session.codecs[0].video_codec, 0);
    }

    #[test]
    fn a_ping_gets_the_same_timestamp_back() {
        let ping = DisplayMessage::Ping(Ping { ping_timestamp: 77 });
        let [reply] = replies(&ping, &DisplayConfig::default(), 0)
            .try_into()
            .unwrap();
        assert_eq!(
            round_trip(&reply).msg,
            Some(ClientBody::Pong(Pong { ping_timestamp: 77 }))
        );
    }

    #[test]
    fn a_video_frame_is_acknowledged_with_its_identifiers() {
        let frame = DisplayMessage::Video(EncodedFrame {
            frame_index: 9,
            stream_id: 3,
            stream_resolution: None,
            capture_resolution: None,
            timestamp: 0,
            data: vec![1],
            slice_offsets: Vec::new(),
            trailing_bytes: 0,
        });
        let [reply] = replies(&frame, &DisplayConfig::default(), 1234)
            .try_into()
            .unwrap();
        let Some(ClientBody::Acks(batch)) = round_trip(&reply).msg else {
            panic!("not an ack batch")
        };
        assert_eq!(batch.sent_usec, 1234);
        let ack = &batch.acks[0];
        assert_eq!((ack.stream_id, ack.frame_index), (3, 9));
        assert!(ack.message_ack && ack.frame_ack && !ack.force_i_frame);
    }

    #[test]
    fn a_keyframe_request_is_an_ack_with_force_i_frame() {
        let Some(ClientBody::Acks(batch)) = round_trip(&keyframe_request(2, 5, 10)).msg else {
            panic!()
        };
        let ack = &batch.acks[0];
        assert!(ack.force_i_frame && ack.frame_ack && ack.message_ack);
    }

    #[test]
    fn other_messages_need_no_reply() {
        let config = DisplayConfig::default();
        assert!(replies(&DisplayMessage::Other(20), &config, 0).is_empty());
        let stale = DisplayMessage::Stale {
            frame_index: 1,
            newest: 2,
        };
        assert!(replies(&stale, &config, 0).is_empty());
    }

    #[test]
    fn replies_follow_what_the_unpacker_produces() {
        let message = ServerMessage {
            msg: Some(ServerBody::Ping(Ping { ping_timestamp: 1 })),
        };
        let unpacked = Unpacker::default()
            .unpack(Frame {
                message: message.encode_to_vec(),
                raw: Vec::new(),
            })
            .unwrap();
        assert_eq!(replies(&unpacked, &DisplayConfig::default(), 0).len(), 1);
    }

    #[test]
    fn three_and_four_byte_start_codes_are_found() {
        let stream = [0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 0, 0, 1, 0x65, 8, 8];
        assert_eq!(nal_types(&stream), [NAL_SPS, NAL_PPS, NAL_IDR]);
    }

    #[test]
    fn a_stream_without_start_code_has_no_nal_unit() {
        assert!(nal_types(&[1, 2, 3, 4]).is_empty());
        assert!(nal_types(&[]).is_empty());
        assert!(nal_types(&[0, 0, 1]).is_empty());
    }

    #[test]
    fn only_an_idr_slice_makes_a_key_frame() {
        assert!(is_key_frame(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
        assert!(!is_key_frame(&[0, 0, 0, 1, 0x41, 1, 2, 3]));
    }

    // ---- Channel

    fn video_frame(stream: u32, index: u64, data: &[u8]) -> Frame {
        let update = TileUpdate {
            frame_index: index,
            stream_id: stream,
            frame_size: data.len() as u32,
            ..Default::default()
        };
        framed(ServerBody::TileUpdate(update), data)
    }

    #[test]
    fn a_frame_is_acknowledged_and_given_to_the_decoder() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        let key = [0, 0, 0, 1, 0x65, 1, 2];
        let step = ch.on_frame(video_frame(1, 1, &key), t0);
        assert_eq!(step.replies.len(), 1);
        assert_eq!(step.video.map(|f| f.frame_index), Some(1));
        assert_eq!(
            (ch.stats.frames, ch.stats.key_frames, ch.stats.bytes),
            (1, 1, 7)
        );
    }

    #[test]
    fn a_missing_frame_asks_for_a_key_frame_but_not_twice_in_a_row() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        ch.on_frame(video_frame(1, 1, &[1]), t0);
        let step = ch.on_frame(video_frame(1, 4, &[1]), t0);
        assert_eq!(ch.stats.missing, 2);
        let Some(ClientBody::Acks(batch)) = step.replies.last().and_then(|m| m.msg.clone()) else {
            panic!("no ack batch")
        };
        assert!(batch.acks[0].force_i_frame);
        // Another gap within the interval: acknowledged only.
        let step = ch.on_frame(video_frame(1, 6, &[1]), t0 + Duration::from_millis(100));
        assert_eq!(step.replies.len(), 1);
        let step = ch.on_frame(video_frame(1, 9, &[1]), t0 + Duration::from_millis(700));
        assert_eq!(step.replies.len(), 2);
        assert_eq!(ch.stats.keyframe_requests, 2);
    }

    #[test]
    fn a_new_stream_restarts_the_numbering_without_a_gap() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        ch.on_frame(video_frame(1, 50, &[1]), t0);
        let step = ch.on_frame(video_frame(2, 1, &[1]), t0);
        assert_eq!((ch.stats.missing, step.replies.len()), (0, 1));
    }

    #[test]
    fn nothing_asks_for_a_key_frame_before_a_frame_came() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        assert!(ch.keyframe_request(t0).is_none());
    }

    #[test]
    fn ready_follows_a_silent_handshake_once() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        let start = framed(
            ServerBody::HandshakeStart(HandshakeStart { negotiation_id: 7 }),
            &[],
        );
        let step = ch.on_frame(start, t0);
        assert!(matches!(
            step.replies[0].msg,
            Some(ClientBody::DisplaySession(_))
        ));
        assert!(ch.on_tick(t0 + Duration::from_secs(1)).is_empty());
        let [r] = ch.on_tick(t0 + READY_DELAY).try_into().unwrap();
        assert!(matches!(
            r.msg,
            Some(ClientBody::Ready(Ready {
                negotiation_id: 7,
                ..
            }))
        ));
        assert!(ch.on_tick(t0 + Duration::from_secs(9)).is_empty());
    }

    #[test]
    fn the_server_speaking_first_cancels_ready() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        let start = framed(
            ServerBody::HandshakeStart(HandshakeStart { negotiation_id: 7 }),
            &[],
        );
        ch.on_frame(start, t0);
        ch.on_frame(video_frame(1, 1, &[1]), t0 + Duration::from_millis(500));
        assert!(ch.on_tick(t0 + Duration::from_secs(3)).is_empty());
    }

    #[test]
    fn a_rejected_message_is_counted_and_answered_with_nothing() {
        let t0 = Instant::now();
        let mut ch = Channel::new(DisplayConfig::default(), t0);
        let step = ch.on_frame(video_frame(1, 1, &[]), t0);
        assert!(step.replies.is_empty() && step.video.is_none());
        assert_eq!(ch.stats.rejected, 1);
    }
}

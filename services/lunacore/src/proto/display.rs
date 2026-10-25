//! Hand-written `prost` subset of `dcv.display` (the desktop client's `docs/PROTOCOL.md` annex A).
//!
//! Only the messages the video channel reads or writes are declared; prost skips the other fields. Every field is
//! optional on the wire (proto3), so an absent field reads as its default.

use prost::{Enumeration, Message, Oneof};

#[derive(Clone, PartialEq, Message)]
pub struct Rect {
    #[prost(uint32, tag = "1")]
    pub x: u32,
    #[prost(uint32, tag = "2")]
    pub y: u32,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct Resolution {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SafeZones {
    #[prost(uint32, tag = "1")]
    pub left: u32,
    #[prost(uint32, tag = "2")]
    pub right: u32,
    #[prost(uint32, tag = "3")]
    pub top: u32,
    #[prost(uint32, tag = "4")]
    pub bottom: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Display {
    #[prost(message, optional, tag = "1")]
    pub resolution: Option<Resolution>,
    #[prost(uint32, tag = "2")]
    pub refresh_rate: u32,
    #[prost(message, optional, tag = "3")]
    pub safe_zones: Option<SafeZones>,
}

/// `VideoCodec.Codec`: `Avc` is 0, the proto3 default, so it is never written on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum Codec {
    Avc = 0,
    Hevc = 1,
    Av1 = 2,
}

#[derive(Clone, PartialEq, Message)]
pub struct VideoCodec {
    #[prost(enumeration = "Codec", tag = "1")]
    pub video_codec: i32,
    #[prost(uint32, tag = "2")]
    pub profile: u32,
    #[prost(uint32, tag = "3")]
    pub level: u32,
    #[prost(uint32, tag = "4")]
    pub tier: u32,
    #[prost(uint32, tag = "5")]
    pub max_reference_frames_num: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct DisplaySessionConfiguration {
    #[prost(uint64, tag = "1")]
    pub negotiation_id: u64,
    #[prost(message, optional, tag = "2")]
    pub capture_resolution: Option<Resolution>,
    #[prost(message, optional, tag = "3")]
    pub display: Option<Display>,
    #[prost(message, repeated, tag = "4")]
    pub codecs: Vec<VideoCodec>,
    #[prost(string, repeated, tag = "5")]
    pub features: Vec<String>,
    #[prost(uint32, tag = "6")]
    pub min_bits_per_second: u32,
    #[prost(uint32, tag = "7")]
    pub max_bits_per_second: u32,
    #[prost(uint32, tag = "8")]
    pub start_bits_per_second: u32,
    #[prost(uint32, tag = "9")]
    pub clamping_mode: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct HandshakeStart {
    #[prost(uint64, tag = "1")]
    pub negotiation_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ready {
    #[prost(uint64, tag = "1")]
    pub negotiation_id: u64,
    #[prost(uint32, tag = "2")]
    pub max_bit_rate: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamStart {
    #[prost(uint32, tag = "1")]
    pub stream_id: u32,
    #[prost(uint32, tag = "10")]
    pub codec_id: u32,
    #[prost(uint32, tag = "20")]
    pub layer: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamEnd {
    #[prost(uint32, tag = "1")]
    pub stream_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TileUpdate {
    #[prost(uint64, tag = "1")]
    pub frame_index: u64,
    #[prost(uint32, tag = "10")]
    pub stream_id: u32,
    #[prost(message, optional, tag = "20")]
    pub rect: Option<Rect>,
    #[prost(message, optional, tag = "21")]
    pub capture_resolution: Option<Resolution>,
    #[prost(message, optional, tag = "22")]
    pub stream_resolution: Option<Resolution>,
    #[prost(uint64, tag = "40")]
    pub timestamp: u64,
    #[prost(uint32, tag = "44")]
    pub frame_slices: u32,
    #[prost(uint32, repeated, tag = "45")]
    pub frame_slice_offsets: Vec<u32>,
    #[prost(uint32, tag = "60")]
    pub frame_size: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ping {
    #[prost(uint64, tag = "1")]
    pub ping_timestamp: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Pong {
    #[prost(uint64, tag = "1")]
    pub ping_timestamp: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Probe {
    #[prost(uint64, tag = "1")]
    pub index: u64,
    #[prost(uint64, tag = "2")]
    pub stream_id: u64,
    #[prost(uint64, tag = "3")]
    pub next_probe_size: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Ack2 {
    #[prost(uint64, tag = "1")]
    pub stream_id: u64,
    #[prost(uint64, tag = "2")]
    pub frame_index: u64,
    #[prost(bool, tag = "10")]
    pub message_ack: bool,
    #[prost(uint64, tag = "11")]
    pub received_usec: u64,
    #[prost(bool, tag = "40")]
    pub frame_ack: bool,
    #[prost(bool, tag = "41")]
    pub force_i_frame: bool,
    #[prost(uint64, tag = "42")]
    pub current_stream_id: u64,
    /// The times of one picture on the client's clock (µs): arrival, decoding, display.
    #[prost(bool, tag = "70")]
    pub frame_stats: bool,
    #[prost(uint64, tag = "71")]
    pub network_recv_usec: u64,
    #[prost(uint64, tag = "72")]
    pub decode_start_usec: u64,
    #[prost(uint64, tag = "73")]
    pub decode_end_usec: u64,
    #[prost(uint64, tag = "74")]
    pub render_start_usec: u64,
    #[prost(uint64, tag = "75")]
    pub render_end_usec: u64,
}

/// `Settings`: the quality range the client accepts and the largest picture it shows (meaning of the
/// numbers not established).
#[derive(Clone, PartialEq, Message)]
pub struct Settings {
    #[prost(uint32, tag = "1")]
    pub min_quality: u32,
    #[prost(uint32, tag = "2")]
    pub max_quality: u32,
    #[prost(uint32, tag = "10")]
    pub max_display_width: u32,
    #[prost(uint32, tag = "11")]
    pub max_display_height: u32,
}

/// `NotifyResolutionChange`: the size the client now shows.
#[derive(Clone, PartialEq, Message)]
pub struct NotifyResolutionChange {
    #[prost(message, optional, tag = "1")]
    pub resolution: Option<Resolution>,
}

/// `BitrateAdjustment`: a bandwidth (unit not established; bytes a second is assumed).
#[derive(Clone, PartialEq, Message)]
pub struct BitrateAdjustment {
    #[prost(double, tag = "10")]
    pub bandwidth_per_sec: f64,
}

#[derive(Clone, PartialEq, Message)]
pub struct BatchedAck2 {
    #[prost(uint64, tag = "1")]
    pub sent_usec: u64,
    #[prost(message, repeated, tag = "10")]
    pub acks: Vec<Ack2>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ClientBody {
    #[prost(message, tag = "30")]
    Ready(Ready),
    #[prost(message, tag = "41")]
    Acks(BatchedAck2),
    #[prost(message, tag = "50")]
    Settings(Settings),
    #[prost(message, tag = "60")]
    Pong(Pong),
    #[prost(message, tag = "80")]
    BitrateAdjustment(BitrateAdjustment),
    #[prost(message, tag = "81")]
    DisplaySession(DisplaySessionConfiguration),
    #[prost(message, tag = "82")]
    NotifyResolutionChange(NotifyResolutionChange),
}

/// `dcv.display.ClientMessage`.
#[derive(Clone, PartialEq, Message)]
pub struct ClientMessage {
    #[prost(oneof = "ClientBody", tags = "30, 41, 50, 60, 80, 81, 82")]
    pub msg: Option<ClientBody>,
}

#[derive(Clone, PartialEq, Oneof)]
pub enum ServerBody {
    #[prost(message, tag = "1")]
    HandshakeStart(HandshakeStart),
    #[prost(message, tag = "30")]
    StreamStart(StreamStart),
    #[prost(message, tag = "31")]
    StreamEnd(StreamEnd),
    #[prost(message, tag = "40")]
    TileUpdate(TileUpdate),
    #[prost(message, tag = "60")]
    Ping(Ping),
    #[prost(message, tag = "81")]
    DisplaySession(DisplaySessionConfiguration),
    #[prost(message, tag = "82")]
    Probe(Probe),
}

/// `dcv.display.ServerMessage`; `msg` is `None` for the arms this client does not read.
#[derive(Clone, PartialEq, Message)]
pub struct ServerMessage {
    #[prost(oneof = "ServerBody", tags = "1, 30, 31, 40, 60, 81, 82")]
    pub msg: Option<ServerBody>,
}

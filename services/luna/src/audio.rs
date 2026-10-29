//! The audio channel: Opus offered (mono, stereo), the configuration, the
//! stream started when the server offers it, then Opus packets (the raw
//! part of each `audio_packet`), handed to the `Player`.

use lunacore::proto::audio::{ClientMessage, Configuration};
use lunacore::stream::audio::{codecs_message, parse, start_stream_message, ServerEvent};
use lunacore::stream::framing::Frame;
use lunacore::stream::playout::Payload;

use crate::player::Player;

pub struct Audio {
    pub config: Option<Configuration>,
    player: Player,
    /// Packets without an Opus payload that were not marked silent.
    pub empty: u64,
}

/// Makeup gain (`gain_db=` of `service.txt`, 0 to 24): the stream measured
/// at -42 dBFS RMS with peaks at -23 dBFS (Tomb Raider's first minutes).
const DEFAULT_GAIN_DB: f32 = 12.0;

impl Default for Audio {
    fn default() -> Self {
        let gain = crate::setting("gain_db")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|g| (0.0..=24.0).contains(g))
            .unwrap_or(DEFAULT_GAIN_DB);
        Self {
            config: None,
            player: Player::start(gain),
            empty: 0,
        }
    }
}

impl Audio {
    /// A frame of the audio channel: what to answer.
    pub fn on_frame(&mut self, frame: &Frame) -> Vec<ClientMessage> {
        match parse(frame) {
            Ok(ServerEvent::HandshakeStart) => vec![codecs_message()],
            Ok(ServerEvent::Configuration(c)) => {
                println!(
                    "luna: audio {:?} type {:?}, {:?} ms, codec {:?}",
                    c.sample_rate,
                    c.r#type,
                    c.frame_length,
                    c.codec
                        .as_ref()
                        .map(|k| (&k.name, &k.audio_profile, &k.bitrates, k.level))
                );
                self.config = Some(c);
                Vec::new()
            }
            Ok(ServerEvent::StreamAvailable) => vec![start_stream_message()],
            Ok(ServerEvent::Packet(p)) => {
                if p.strength.is_some_and(|s| s <= -1.0) {
                    self.player.packet(p.frame_index, Payload::Silent);
                } else if frame.raw.is_empty() {
                    self.empty += 1;
                } else {
                    self.player
                        .packet(p.frame_index, Payload::Opus(frame.raw.clone()));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// One line on what came and how it plays.
    pub fn report(&self) -> String {
        if self.empty > 0 {
            format!("{} ({} without payload)", self.player.report(), self.empty)
        } else {
            self.player.report()
        }
    }
}

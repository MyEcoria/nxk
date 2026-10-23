//! The display channel: the capabilities answered, each frame acknowledged,
//! a key frame asked for after a gap. The frames are only counted for now:
//! decoding them comes next.

use std::time::Instant;

use lunacore::proto::display::ClientMessage;
use lunacore::stream::display::{nal_types, Channel, DisplayConfig, EncodedFrame};
use lunacore::stream::framing::Frame;

pub struct Video {
    channel: Channel,
    first: bool,
    shown: u64,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            channel: Channel::new(DisplayConfig::default(), Instant::now()),
            first: true,
            shown: 0,
        }
    }
}

impl Video {
    /// A frame of the display channel: what to send back.
    pub fn on_frame(&mut self, frame: Frame) -> Vec<ClientMessage> {
        let step = self.channel.on_frame(frame, Instant::now());
        if let Some(f) = step.video {
            self.on_video(f);
        }
        step.replies
    }

    fn on_video(&mut self, f: EncodedFrame) {
        if self.first {
            self.first = false;
            println!(
                "luna: first video frame {} bytes, stream {:?}, NAL types {:?}",
                f.data.len(),
                f.stream_resolution.map(|r| (r.width, r.height)),
                nal_types(&f.data)
            );
        }
    }

    /// `ready`, when the server stayed silent after our capabilities.
    pub fn on_tick(&mut self) -> Vec<ClientMessage> {
        self.channel.on_tick(Instant::now())
    }

    /// One line on what came, when it changed.
    pub fn report(&mut self) -> Option<String> {
        let s = &self.channel.stats;
        if s.frames == self.shown && s.rejected == 0 {
            return None;
        }
        self.shown = s.frames;
        Some(format!(
            "luna: video {} frames {} KB, {} key, {} missing, {} stale, {} rejected, {} key frames asked",
            s.frames,
            s.bytes / 1024,
            s.key_frames,
            s.missing,
            s.stale,
            s.rejected,
            s.keyframe_requests
        ))
    }
}

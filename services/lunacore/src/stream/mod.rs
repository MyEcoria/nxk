//! Luna's streaming protocol with no I/O, ported from the desktop client
//! (docs/PROTOCOL.md there): SPP packets over DTLS (`spp`), their
//! reassembly and ordering (`reliability`), the message framing of every
//! channel (`framing`), the session's setup messages (`setup`) and the
//! input channel's events (`input`).

pub mod framing;
pub mod input;
pub mod reliability;
pub mod setup;
pub mod spp;

use std::time::Duration;

/// A string that must never be printed: `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that puts it on the wire.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Why the session could not be established or has failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    ChannelName(String),
    Aborted(String),
    Decode(String),
    Unexpected(String),
    Timeout { what: String, timeout: Duration },
    Closed(String),
    TooManyChannels,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelName(n) => {
                write!(f, "channel name {:?} is empty or longer than 63 bytes", n)
            }
            Self::Aborted(r) => write!(f, "the server aborted the connection: {}", r),
            Self::Decode(e) => write!(f, "could not decode a server message: {}", e),
            Self::Unexpected(m) => write!(f, "unexpected server message: {}", m),
            Self::Timeout { what, timeout } => write!(
                f,
                "no answer within {:?} while waiting for {}",
                timeout, what
            ),
            Self::Closed(w) => write!(f, "the connection ended while waiting for {}", w),
            Self::TooManyChannels => f.write_str("all application channels are in use"),
        }
    }
}

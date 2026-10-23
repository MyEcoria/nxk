//! SPP framing: the 32-byte header, payload types, the connect message,
//! fragmentation and reassembly. One SPP packet is one DTLS datagram.

mod connect;
mod control;
mod header;
mod receive;
mod send;

pub use connect::{
    ConnectError, ConnectForm, ConnectRequest, ConnectResponse, FULL_LEN, SPP_VERSION,
};
pub use control::{rtcp_payload, time_reply, Clocks};
pub use header::{pt, Header, Packet, SppError, HEADER_LEN, MAX_CHANNEL, MAX_RX_PAYLOAD};
pub use receive::{Event, Message, Reassembler, Receiver, MAX_MESSAGE_LEN, ORDER_BUDGET};
pub use send::{SendState, CONTROL_PAYLOAD, MAX_TX_PAYLOAD};

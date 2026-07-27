//! `oxproto` — the oxrdp custom remote-app protocol (sans-io).
//!
//! Purpose-built to replace RDP for winpodx: a lean, low-latency protocol that streams
//! individual Windows application windows (not a full desktop) between the Windows guest
//! `oxagent` and the Linux `oxclient`.
//!
//! The wire format is specified in
//! [`docs/design/OXPROTO.md`](https://github.com/kernalix7/oxrdp/blob/main/docs/design/OXPROTO.md);
//! this crate implements it and nothing else — no IO, no threads.
//!
//! # Layout
//! - [`envelope`] — the 8-byte chunk header, fragmentation, per-channel reassembly, and the
//!   per-type size limits that stop a peer from making a receiver allocate.
//! - [`message`] — the type registry and every message body.
//! - [`wire`] — the primitive encoders shared by the bodies.
//!
//! # Sending
//! ```no_run
//! use oxproto::{envelope::channel, message::{Ping, Message}};
//! let msg = Message::Ping(Ping { seq: 1, sent_us: 0 });
//! for chunk in msg.to_chunks(channel::CONTROL).unwrap() {
//!     // write `chunk` to the transport
//!     let _ = chunk;
//! }
//! ```
//!
//! # Receiving
//! Feed each chunk to a [`envelope::Reassembler`]; when it yields a complete message, decode
//! it with [`message::Message::decode_known`] and **skip** types this build does not know —
//! that is what keeps the protocol extensible without a version break.
#![forbid(unsafe_code)]

pub mod envelope;
pub mod message;
pub mod wire;

/// Codec entry points, re-exported so callers can `oxproto::decode` / `oxproto::encode_vec`
/// without depending on `oxrdp-pdu` directly.
pub use oxrdp_pdu::{decode, encode_vec, Decode, DecodeError, Encode, EncodeError};

pub use envelope::{channel, ChunkHeader, Reassembler, CHUNK_HEADER_LEN, MAX_CHUNK_PAYLOAD};
pub use message::{
    codec, error_code, feature, msg_type, Message, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION,
};

//! `oxtransport` — async framing for the oxproto protocol over tokio streams.
//!
//! Reads/writes whole `oxproto::Message` envelopes on any `AsyncRead`/`AsyncWrite`. Used by
//! both the Linux client and the Windows agent. TCP today; QUIC is planned.
#![forbid(unsafe_code)]

pub mod stream;

pub use stream::{read_message_bytes, write_message};

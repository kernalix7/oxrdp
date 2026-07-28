//! `oxtransport` — async chunk IO for the oxproto protocol over tokio streams.
//!
//! Moves oxproto chunks on any `AsyncRead`/`AsyncWrite` and reassembles them per channel.
//! Used by both the Linux client and the Windows agent. TCP today; QUIC planned, where each
//! oxproto channel becomes an independent stream.
#![forbid(unsafe_code)]

pub mod stream;

pub use stream::{
    read_message, read_reassembled, write_message, write_raw, ChunkReader, ChunkWriter,
};

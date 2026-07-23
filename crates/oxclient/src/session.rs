//! Client session skeleton (in progress).
//!
//! The next step is to implement `ClientSession` over a tokio stream using
//! [`oxtransport`]: `connect` → send [`ClientHello`](oxproto::ClientHello) → read
//! [`ServerHello`](oxproto::ServerHello) → `next_event` reads one [`oxproto::Message`] per
//! call and maps it to a [`ClientEvent`]. See `docs/HANDOFF.md` (P2) for the intended shape.

/// An event surfaced to the display/render layer as the agent streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// A new remote window appeared; map it to a native Linux window.
    WindowOpened {
        window_id: u32,
        title: String,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    },
    /// An encoded (or raw) frame for a window; decode and present it.
    Frame {
        window_id: u32,
        codec: u8,
        keyframe: bool,
        timestamp: u32,
        data: Vec<u8>,
    },
    /// A remote window closed; destroy its native window.
    WindowClosed { window_id: u32 },
}

/// Negotiated session parameters after the ClientHello / ServerHello handshake.
///
/// TODO(P2): hold the tokio stream + a read buffer and implement `connect` / `next_event`
/// using `oxtransport::{read_message_bytes, write_message}` and `oxproto::decode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientSession {
    /// Codec the agent selected in its ServerHello.
    pub codec: u8,
    /// Session id assigned by the agent.
    pub session_id: u32,
}

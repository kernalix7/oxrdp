//! `oxproto` — the oxrdp custom remote-app protocol (sans-io wire messages).
//!
//! Purpose-built to replace RDP for winpodx: a lean, low-latency protocol that streams
//! individual Windows application windows (not a full desktop) between the Windows guest
//! `agent` and the Linux `client`. Messages encode/decode via the shared bounds-checked
//! codec from [`oxrdp_pdu`] (`Decode`/`Encode` over `ReadCursor`/`WriteCursor`).
//!
//! Pre-alpha. See docs/ARCHITECTURE.md.
#![forbid(unsafe_code)]

pub mod message;

/// Codec entry points, re-exported so callers can `oxproto::decode` / `oxproto::encode_vec`
/// oxproto messages without depending on `oxrdp-pdu` directly.
pub use oxrdp_pdu::{decode, encode_vec};

pub use message::{
    codec, msg_type, ClientHello, FrameData, Message, PointerEvent, ServerHello, WindowCreated,
};

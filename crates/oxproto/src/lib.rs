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

pub use message::{
    codec, msg_type, ClientHello, FrameData, Message, PointerEvent, ServerHello, WindowCreated,
};

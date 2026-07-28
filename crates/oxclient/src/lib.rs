//! `oxclient` — the Linux client session for the oxproto protocol.
//!
//! Connects to the Windows `oxagent`, performs the handshake, answers protocol housekeeping,
//! and surfaces window-lifecycle, frame and cursor events for the display/render layer.
//!
//! Frames are decoded here, on the session side ([`decode`]), so the display layer only ever
//! sees `RAW_BGRA` and stays codec-agnostic.
#![forbid(unsafe_code)]

pub mod decode;
pub mod geometry;
pub mod model;
pub mod session;

pub use decode::{DecodeError, Decoder, WindowDecoders};
pub use geometry::GeometrySync;
pub use model::{ModelChange, RemoteWindow, WindowModel};
pub use session::{ClientEvent, ClientSession, SessionConfig};

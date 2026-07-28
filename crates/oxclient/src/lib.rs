//! `oxclient` — the Linux client session for the oxproto protocol.
//!
//! Connects to the Windows `oxagent`, performs the handshake, answers protocol housekeeping,
//! and surfaces window-lifecycle, frame and cursor events for the display/render layer.
#![forbid(unsafe_code)]

pub mod model;
pub mod session;

pub use model::{ModelChange, RemoteWindow, WindowModel};
pub use session::{ClientEvent, ClientSession, SessionConfig};

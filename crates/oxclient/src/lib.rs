//! `oxclient` — the Linux client session for the oxproto protocol.
//!
//! Connects to the Windows `oxagent`, performs the ClientHello/ServerHello handshake, and
//! exposes received window-lifecycle and frame events for the display/render layer to
//! consume.
#![forbid(unsafe_code)]

pub mod session;

pub use session::{ClientEvent, ClientSession};

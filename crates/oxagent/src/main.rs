//! `oxagent` — the oxrdp Windows guest agent.
//!
//! Captures individual application windows via Windows.Graphics.Capture and streams them to the
//! Linux client over the oxproto protocol. See `docs/design/agent-runtime.md` for the session
//! and deployment model, and `docs/design/OXPROTO.md` for the wire protocol.
//!
//! The handshake and pacing logic is platform-independent and unit-tested on the build host;
//! only capture and input injection are Windows-only.
#![allow(unsafe_code)] // windows-rs COM/WinRT calls require unsafe

pub mod config;
pub mod handshake;
pub mod pacing;

#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("oxagent runs on the Windows guest; build with --target x86_64-pc-windows-gnu");
}

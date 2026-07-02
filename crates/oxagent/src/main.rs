//! `oxagent` — the oxrdp Windows guest agent (Windows-only).
//!
//! Captures individual application windows via Windows.Graphics.Capture, encodes them via
//! Media Foundation, and streams them to the Linux client over the `oxproto` protocol.
//! On non-Windows hosts this is a stub so the workspace still builds; the real agent is
//! cross-compiled to `x86_64-pc-windows-gnu`.
#![allow(unsafe_code)] // windows-rs COM/WinRT calls require unsafe

#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("oxagent runs on the Windows guest; build with --target x86_64-pc-windows-gnu");
}

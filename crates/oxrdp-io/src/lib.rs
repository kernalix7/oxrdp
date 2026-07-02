//! `oxrdp-io` — the tokio transport shell.
//!
//! Owns the socket and (later) TLS, framing bytes into TPKT messages and pumping the
//! sans-io [`oxrdp-core`](https://docs.rs/oxrdp-core) connection state machine. For now it
//! provides the async TPKT [`frame`] codec; the full connect driver lands next.
//!
//! Part of the [oxrdp](https://github.com/kernalix7/oxrdp) workspace. Pre-alpha.

pub mod connect;
pub mod frame;

pub use connect::{connect, Session};
pub use frame::{read_frame, write_frame};

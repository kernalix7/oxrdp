//! `oxrdp` — memory-safe RDP client engine.
//!
//! This is the facade crate winpodx links: it will expose the high-level `Session`
//! API and re-exports the workspace crates that implement the protocol core
//! (sans-io) and the IO / display / render / input shells.
//!
//! Pre-alpha: the API is not yet implemented. See
//! [docs/ARCHITECTURE.md](https://github.com/kernalix7/oxrdp/blob/main/docs/ARCHITECTURE.md).
#![forbid(unsafe_code)]

// sans-io pure core
pub use oxrdp_channels;
pub use oxrdp_core;
pub use oxrdp_graphics;
pub use oxrdp_pdu;
pub use oxrdp_rail;

// crypto glue + impure shells
pub use oxrdp_crypto;
pub use oxrdp_display;
pub use oxrdp_input;
pub use oxrdp_io;
pub use oxrdp_render;

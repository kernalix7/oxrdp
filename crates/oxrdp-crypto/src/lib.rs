//! `oxrdp-crypto` — security glue for the RDP transport.
//!
//! Provides the rustls TLS [`ClientConfig`](tls::tls_client_config) the IO shell uses to
//! upgrade the connection after the X.224 negotiation selects a TLS-based protocol. NLA /
//! CredSSP (via `sspi-rs`) is deferred — winpodx uses plain `/sec:tls`, so v0 does not need
//! it.
//!
//! Part of the [oxrdp](https://github.com/kernalix7/oxrdp) workspace. Pre-alpha.

pub mod tls;

pub use tls::{tls_client_config, TofuVerifier};

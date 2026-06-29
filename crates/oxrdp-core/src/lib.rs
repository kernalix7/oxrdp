//! `oxrdp-core` — the sans-io RDP connection state machine.
//!
//! Drives the RDP connection sequence (X.224 negotiation, MCS, capability exchange) as a
//! pure state machine: it performs no IO and emits [`CoreOutput`]s telling the caller what
//! bytes to send and when to upgrade the transport to TLS. An IO shell (`oxrdp-io`) owns
//! the socket and TLS and feeds complete frames back in.
//!
//! Pre-alpha: currently implements the X.224 negotiation phase. See
//! [docs/ARCHITECTURE.md](https://github.com/kernalix7/oxrdp/blob/main/docs/ARCHITECTURE.md).
#![forbid(unsafe_code)]

pub mod connector;

pub use connector::{
    ClientConnector, ConnectorConfig, ConnectorState, CoreError, CoreOutput, CoreResult,
};

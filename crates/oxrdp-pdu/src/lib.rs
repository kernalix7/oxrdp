//! `oxrdp-pdu` — wire types: bounds-checked encode/decode of RDP PDUs (sans-io).
//!
//! This is the foundation crate of the oxrdp workspace. It defines:
//!
//! - [`Decode`] / [`Encode`] — the two traits every PDU implements.
//! - [`ReadCursor`] / [`WriteCursor`] — bounds-checked cursors that **never panic** on a
//!   short or malformed buffer. Server input is untrusted, so all decoding goes through
//!   them.
//! - The first concrete framing PDUs: the [`TpktHeader`] (RFC 1006) and the
//!   [`X224DataHeader`] — the outermost layers every RDP message rides inside.
//!
//! Zero external dependencies by design: this is the most security-critical, fuzzed crate
//! in the workspace, so its trust surface is kept minimal.
//!
//! See [docs/ARCHITECTURE.md](https://github.com/kernalix7/oxrdp/blob/main/docs/ARCHITECTURE.md).
#![forbid(unsafe_code)]

pub mod ber;
pub mod client_info;
mod codec;
pub mod connect;
pub mod connect_initial;
pub mod connect_response;
mod cursor;
pub mod domain_params;
mod error;
pub mod frame;
pub mod gcc;
pub mod gcc_server;
pub mod mcs;
pub mod nego;
pub mod security;
pub mod send_data;
pub mod tpkt;
pub mod x224;

pub use client_info::{ClientInfo, ExtendedInfo};
pub use codec::{decode, encode_vec, Decode, Encode};
pub use connect::{ConnectionConfirm, ConnectionRequest, NegotiationConfirm};
pub use connect_initial::ConnectInitial;
pub use connect_response::ConnectResponse;
pub use cursor::{ReadCursor, WriteCursor};
pub use domain_params::DomainParameters;
pub use error::{DecodeError, DecodeResult, EncodeError, EncodeResult};
pub use frame::{mcs_payload, wrap_mcs};
pub use gcc::{ChannelDef, ClientCoreData, ClientNetworkData, ClientSecurityData};
pub use gcc_server::{ServerCoreData, ServerNetworkData};
pub use mcs::{
    AttachUserConfirm, AttachUserRequest, ChannelJoinConfirm, ChannelJoinRequest,
    ErectDomainRequest, MCS_USERCHANNEL_BASE,
};
pub use nego::{NegotiationFailure, NegotiationRequest, NegotiationResponse};
pub use security::SecurityHeader;
pub use send_data::{SendDataIndication, SendDataRequest};
pub use tpkt::TpktHeader;
pub use x224::X224DataHeader;

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare RDP data frame is a TPKT header followed by an X.224 data header and the
    /// MCS/RDP payload. Exercise the two framing layers together.
    #[test]
    fn tpkt_then_x224_round_trip() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let total = (TpktHeader::new(0).size() + X224DataHeader.size() + payload.len()) as u16;

        let mut frame = encode_vec(&TpktHeader::new(total)).unwrap();
        frame.extend_from_slice(&encode_vec(&X224DataHeader).unwrap());
        frame.extend_from_slice(&payload);

        let mut cursor = ReadCursor::new(&frame);
        let tpkt = TpktHeader::decode(&mut cursor).unwrap();
        let _x224 = X224DataHeader::decode(&mut cursor).unwrap();
        let rest = cursor.read_slice(cursor.remaining(), "payload").unwrap();

        assert_eq!(tpkt.length as usize, frame.len());
        assert_eq!(rest, payload);
        assert!(cursor.is_empty());
    }
}

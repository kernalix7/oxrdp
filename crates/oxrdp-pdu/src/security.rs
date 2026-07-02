//! The RDP Basic Security Header (MS-RDPBCGR 2.2.8.1.1.2.1).
//!
//! Prefixes MCS Send Data payloads after channel join. With external security (TLS), it
//! carries only the `flags` that tag the payload type — e.g. `SEC_INFO_PKT` for the Client
//! Info PDU, `SEC_LICENSE_PKT` for licensing — and no encryption signature.

use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeResult, EncodeResult};

/// Size of a Basic Security Header in bytes.
pub const SECURITY_HEADER_LEN: usize = 4;

/// Security header flag bits (`flags` field).
pub mod sec_flag {
    /// The payload is a Security Exchange PDU.
    pub const EXCHANGE_PKT: u16 = 0x0001;
    /// The payload is encrypted.
    pub const ENCRYPT: u16 = 0x0008;
    /// The payload is the Client Info PDU.
    pub const INFO_PKT: u16 = 0x0040;
    /// The payload is a licensing PDU.
    pub const LICENSE_PKT: u16 = 0x0080;
}

/// A Basic Security Header: a `flags` word plus a `flags_hi` word (unused unless
/// `SEC_FLAGSHI_VALID` is set; normally zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SecurityHeader {
    /// Security flags (see [`sec_flag`]).
    pub flags: u16,
    /// High flags word; reserved, normally zero.
    pub flags_hi: u16,
}

impl SecurityHeader {
    /// A header tagging the payload with `flags` (and `flags_hi` = 0).
    pub fn new(flags: u16) -> Self {
        Self { flags, flags_hi: 0 }
    }
}

impl<'de> Decode<'de> for SecurityHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let flags = src.read_u16_le("security header flags")?;
        let flags_hi = src.read_u16_le("security header flagsHi")?;
        Ok(Self { flags, flags_hi })
    }
}

impl Encode for SecurityHeader {
    fn size(&self) -> usize {
        SECURITY_HEADER_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.flags, "security header flags")?;
        dst.write_u16_le(self.flags_hi, "security header flagsHi")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn info_pkt_round_trip() {
        let header = SecurityHeader::new(sec_flag::INFO_PKT);
        let bytes = encode_vec(&header).unwrap();
        assert_eq!(bytes, [0x40, 0x00, 0x00, 0x00]);
        assert_eq!(decode::<SecurityHeader>(&bytes).unwrap(), header);
    }

    #[test]
    fn license_pkt_flag() {
        let bytes = encode_vec(&SecurityHeader::new(sec_flag::LICENSE_PKT)).unwrap();
        assert_eq!(bytes, [0x80, 0x00, 0x00, 0x00]);
    }
}

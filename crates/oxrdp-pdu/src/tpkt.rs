//! TPKT header (RFC 1006) — the outermost framing of an RDP connection.
//!
//! Every RDP PDU that travels over TCP is wrapped in a 4-byte TPKT header:
//!
//! ```text
//! +---------+----------+----------------------+
//! | version | reserved |   length (u16 BE)    |
//! |  = 3    |   = 0    |  total incl. header  |
//! +---------+----------+----------------------+
//! ```
//!
//! `length` covers the whole PDU including these 4 bytes.

use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeResult};

/// Size of a TPKT header in bytes.
pub const TPKT_HEADER_LEN: usize = 4;

/// The only TPKT version RDP uses.
pub const TPKT_VERSION: u8 = 3;

/// A decoded TPKT header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpktHeader {
    /// Total PDU length in bytes, including this 4-byte header.
    pub length: u16,
}

impl TpktHeader {
    /// Build a header for a PDU whose total on-wire length (including the header) is
    /// `length` bytes.
    pub fn new(length: u16) -> Self {
        Self { length }
    }

    /// Length of the payload that follows this header (total minus the 4-byte header).
    pub fn payload_len(&self) -> usize {
        (self.length as usize).saturating_sub(TPKT_HEADER_LEN)
    }
}

impl<'de> Decode<'de> for TpktHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let version = src.read_u8("TPKT version")?;
        if version != TPKT_VERSION {
            return Err(DecodeError::InvalidField {
                context: "TPKT",
                field: "version",
                reason: "expected 3",
            });
        }
        let _reserved = src.read_u8("TPKT reserved")?;
        let length = src.read_u16_be("TPKT length")?;
        if (length as usize) < TPKT_HEADER_LEN {
            return Err(DecodeError::InvalidLength {
                context: "TPKT",
                reason: "length is smaller than the header",
            });
        }
        Ok(Self { length })
    }
}

impl Encode for TpktHeader {
    fn size(&self) -> usize {
        TPKT_HEADER_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(TPKT_VERSION, "TPKT version")?;
        dst.write_u8(0, "TPKT reserved")?;
        dst.write_u16_be(self.length, "TPKT length")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};
    use crate::error::DecodeError;

    #[test]
    fn round_trips() {
        let header = TpktHeader::new(19);
        let bytes = encode_vec(&header).unwrap();
        assert_eq!(bytes, [0x03, 0x00, 0x00, 0x13]);
        assert_eq!(decode::<TpktHeader>(&bytes).unwrap(), header);
    }

    #[test]
    fn payload_len_excludes_header() {
        assert_eq!(TpktHeader::new(19).payload_len(), 15);
        // Never underflows, even on a nonsensical length.
        assert_eq!(TpktHeader::new(0).payload_len(), 0);
    }

    #[test]
    fn rejects_wrong_version() {
        let err = decode::<TpktHeader>(&[0x02, 0x00, 0x00, 0x04]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "version",
                ..
            }
        ));
    }

    #[test]
    fn rejects_length_below_header() {
        let err = decode::<TpktHeader>(&[0x03, 0x00, 0x00, 0x03]).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidLength { .. }));
    }

    #[test]
    fn rejects_truncated_buffer() {
        let err = decode::<TpktHeader>(&[0x03, 0x00]).unwrap_err();
        assert!(matches!(err, DecodeError::NotEnoughBytes { .. }));
    }
}

//! X.224 (TPDU) data header — the layer between TPKT and the MCS/RDP payload.
//!
//! After the connection-setup exchange (Connection Request / Confirm, added later), every
//! RDP data PDU rides inside a 3-byte X.224 Class 0 *Data* TPDU header:
//!
//! ```text
//! +------+----------+-----------+
//! |  LI  | TPDU code|  EOT/nr   |
//! | =0x02|  =0xF0   |  =0x80    |
//! +------+----------+-----------+
//! ```
//!
//! - `LI` (length indicator) = number of header bytes after itself = 2.
//! - TPDU code `0xF0` = DT (data).
//! - `0x80` = EOT bit set, TPDU-NR 0.

use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeResult};

/// Size of an X.224 Class 0 data header in bytes.
pub const X224_DATA_HEADER_LEN: usize = 3;

const X224_DATA_LI: u8 = 0x02;
const X224_DT_DATA_CODE: u8 = 0xF0;
const X224_EOT: u8 = 0x80;

/// The X.224 Class 0 data TPDU header. It carries no variable fields — it is a fixed
/// 3-byte marker — but is modeled as a type so framing is explicit and validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct X224DataHeader;

impl<'de> Decode<'de> for X224DataHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let li = src.read_u8("X.224 length indicator")?;
        if li != X224_DATA_LI {
            return Err(DecodeError::InvalidField {
                context: "X.224 data",
                field: "length indicator",
                reason: "expected 2",
            });
        }
        let code = src.read_u8("X.224 TPDU code")?;
        if code != X224_DT_DATA_CODE {
            return Err(DecodeError::InvalidField {
                context: "X.224 data",
                field: "TPDU code",
                reason: "expected DT data (0xF0)",
            });
        }
        let eot = src.read_u8("X.224 EOT")?;
        if eot != X224_EOT {
            return Err(DecodeError::InvalidField {
                context: "X.224 data",
                field: "EOT",
                reason: "expected 0x80",
            });
        }
        Ok(Self)
    }
}

impl Encode for X224DataHeader {
    fn size(&self) -> usize {
        X224_DATA_HEADER_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(X224_DATA_LI, "X.224 length indicator")?;
        dst.write_u8(X224_DT_DATA_CODE, "X.224 TPDU code")?;
        dst.write_u8(X224_EOT, "X.224 EOT")?;
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
        let bytes = encode_vec(&X224DataHeader).unwrap();
        assert_eq!(bytes, [0x02, 0xF0, 0x80]);
        assert_eq!(decode::<X224DataHeader>(&bytes).unwrap(), X224DataHeader);
    }

    #[test]
    fn rejects_wrong_code() {
        let err = decode::<X224DataHeader>(&[0x02, 0xE0, 0x80]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "TPDU code",
                ..
            }
        ));
    }

    #[test]
    fn rejects_truncated_buffer() {
        let err = decode::<X224DataHeader>(&[0x02, 0xF0]).unwrap_err();
        assert!(matches!(err, DecodeError::NotEnoughBytes { .. }));
    }
}

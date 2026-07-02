use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeResult, EncodeResult};

/// Protocol version bits OR'd into the pduType field.
pub const TS_PROTOCOL_VERSION: u16 = 0x0010;

/// Well-known `pduType` values for the share control header.
pub mod pdu_type {
    /// Demand Active PDU.
    pub const DEMAND_ACTIVE: u16 = 1;
    /// Confirm Active PDU.
    pub const CONFIRM_ACTIVE: u16 = 3;
    /// Deactivate All PDU.
    pub const DEACTIVATE_ALL: u16 = 6;
    /// Data PDU.
    pub const DATA: u16 = 7;
}

/// Well-known `pduType2` values for the share data header.
pub mod pdu_type2 {
    /// Update PDU.
    pub const UPDATE: u8 = 2;
    /// Control PDU.
    pub const CONTROL: u8 = 20;
    /// Synchronize PDU.
    pub const SYNCHRONIZE: u8 = 31;
    /// Font List PDU.
    pub const FONT_LIST: u8 = 39;
    /// Font Map PDU.
    pub const FONT_MAP: u8 = 40;
}

/// TS_SHARECONTROLHEADER (6 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareControlHeader {
    /// Total PDU length including this header.
    pub total_length: u16,
    /// Low 4 bits are the type (`pdu_type::*`), OR'd with `TS_PROTOCOL_VERSION`.
    pub pdu_type: u16,
    /// The source channel (e.g. the user channel id).
    pub pdu_source: u16,
}

impl ShareControlHeader {
    /// Creates a new share control header with the protocol version bits set.
    pub fn new(pdu_type: u16, pdu_source: u16, total_length: u16) -> Self {
        Self {
            total_length,
            pdu_type: pdu_type | TS_PROTOCOL_VERSION,
            pdu_source,
        }
    }

    /// Returns the raw PDU type without the protocol version bits.
    pub fn kind(&self) -> u16 {
        self.pdu_type & 0x000F
    }
}

impl Encode for ShareControlHeader {
    fn size(&self) -> usize {
        6
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.total_length, "share control total_length")?;
        dst.write_u16_le(self.pdu_type, "share control pdu_type")?;
        dst.write_u16_le(self.pdu_source, "share control pdu_source")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ShareControlHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let total_length = src.read_u16_le("share control total_length")?;
        let pdu_type = src.read_u16_le("share control pdu_type")?;
        let pdu_source = src.read_u16_le("share control pdu_source")?;
        Ok(Self {
            total_length,
            pdu_type,
            pdu_source,
        })
    }
}

/// TS_SHAREDATAHEADER (18 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareDataHeader {
    /// The enclosing share control header.
    pub control: ShareControlHeader,
    /// Share identifier.
    pub share_id: u32,
    /// Stream identifier.
    pub stream_id: u8,
    /// Uncompressed payload length.
    pub uncompressed_length: u16,
    /// PDU type 2 (`pdu_type2::*`).
    pub pdu_type2: u8,
    /// Compression type flags.
    pub compressed_type: u8,
    /// Compressed payload length.
    pub compressed_length: u16,
}

impl Encode for ShareDataHeader {
    fn size(&self) -> usize {
        18
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.control.encode(dst)?;
        dst.write_u32_le(self.share_id, "share data share_id")?;
        dst.write_u8(0, "share data pad1")?;
        dst.write_u8(self.stream_id, "share data stream_id")?;
        dst.write_u16_le(self.uncompressed_length, "share data uncompressed_length")?;
        dst.write_u8(self.pdu_type2, "share data pdu_type2")?;
        dst.write_u8(self.compressed_type, "share data compressed_type")?;
        dst.write_u16_le(self.compressed_length, "share data compressed_length")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ShareDataHeader {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let control = ShareControlHeader::decode(src)?;
        let share_id = src.read_u32_le("share data share_id")?;
        let _pad = src.read_u8("share data pad1")?;
        let stream_id = src.read_u8("share data stream_id")?;
        let uncompressed_length = src.read_u16_le("share data uncompressed_length")?;
        let pdu_type2 = src.read_u8("share data pdu_type2")?;
        let compressed_type = src.read_u8("share data compressed_type")?;
        let compressed_length = src.read_u16_le("share data compressed_length")?;
        Ok(Self {
            control,
            share_id,
            stream_id,
            uncompressed_length,
            pdu_type2,
            compressed_type,
            compressed_length,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn control_header_confirm_active() {
        let h = ShareControlHeader::new(pdu_type::CONFIRM_ACTIVE, 1007, 100);
        let bytes = encode_vec(&h).unwrap();
        // total_length=100 (0x64), pdu_type = 3 | 0x10 = 0x13, pdu_source=1007 (0x03EF)
        assert_eq!(bytes, [0x64, 0x00, 0x13, 0x00, 0xEF, 0x03]);
        assert_eq!(h.kind(), pdu_type::CONFIRM_ACTIVE);
        assert_eq!(decode::<ShareControlHeader>(&bytes).unwrap(), h);
    }

    #[test]
    fn data_header_round_trip() {
        let h = ShareDataHeader {
            control: ShareControlHeader::new(pdu_type::DATA, 1007, 30),
            share_id: 0x0001_00EA,
            stream_id: 1,
            uncompressed_length: 12,
            pdu_type2: pdu_type2::SYNCHRONIZE,
            compressed_type: 0,
            compressed_length: 0,
        };
        let bytes = encode_vec(&h).unwrap();
        assert_eq!(bytes.len(), 18);
        assert_eq!(decode::<ShareDataHeader>(&bytes).unwrap(), h);
    }
}

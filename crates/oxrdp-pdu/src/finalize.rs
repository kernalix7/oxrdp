use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeResult, EncodeResult};

/// Control PDU action codes.
pub mod control_action {
    pub const REQUEST_CONTROL: u16 = 0x0001;
    pub const GRANTED_CONTROL: u16 = 0x0002;
    pub const DETACH: u16 = 0x0003;
    pub const COOPERATE: u16 = 0x0004;
}

/// Synchronize message type (SYNCMSGTYPE_SYNC).
const SYNCMSGTYPE_SYNC: u16 = 0x0001;

/// TS_SYNCHRONIZE_PDU body (MS-RDPBCGR 2.2.1.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynchronizePdu {
    /// Target user identifier.
    pub target_user: u16,
}

impl Encode for SynchronizePdu {
    /// Encoded size in bytes: `messageType` (u16) + `targetUser` (u16).
    fn size(&self) -> usize {
        4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(SYNCMSGTYPE_SYNC, "finalize")?;
        dst.write_u16_le(self.target_user, "finalize")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for SynchronizePdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let _message_type = src.read_u16_le("finalize")?;
        let target_user = src.read_u16_le("finalize")?;
        Ok(Self { target_user })
    }
}

/// TS_CONTROL_PDU body (MS-RDPBCGR 2.2.1.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPdu {
    /// Action code (see [`control_action`]).
    pub action: u16,
    /// Grant identifier.
    pub grant_id: u16,
    /// Control identifier.
    pub control_id: u32,
}

impl Encode for ControlPdu {
    /// Encoded size in bytes: `action` (u16) + `grantId` (u16) + `controlId` (u32).
    fn size(&self) -> usize {
        8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(self.action, "finalize")?;
        dst.write_u16_le(self.grant_id, "finalize")?;
        dst.write_u32_le(self.control_id, "finalize")?;
        Ok(())
    }
}

impl<'de> Decode<'de> for ControlPdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let action = src.read_u16_le("finalize")?;
        let grant_id = src.read_u16_le("finalize")?;
        let control_id = src.read_u32_le("finalize")?;
        Ok(Self {
            action,
            grant_id,
            control_id,
        })
    }
}

/// TS_FONT_LIST_PDU body (MS-RDPBCGR 2.2.1.18): the standard "no fonts" list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontListPdu;

impl Encode for FontListPdu {
    /// Encoded size in bytes: four u16 fields.
    fn size(&self) -> usize {
        8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u16_le(0, "finalize")?; // numberFonts
        dst.write_u16_le(0, "finalize")?; // totalNumFonts
        dst.write_u16_le(0x0003, "finalize")?; // listFlags
        dst.write_u16_le(0x0032, "finalize")?; // entrySize
        Ok(())
    }
}

impl<'de> Decode<'de> for FontListPdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let _number_fonts = src.read_u16_le("finalize")?;
        let _total_num_fonts = src.read_u16_le("finalize")?;
        let _list_flags = src.read_u16_le("finalize")?;
        let _entry_size = src.read_u16_le("finalize")?;
        Ok(FontListPdu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn synchronize_round_trip() {
        let s = SynchronizePdu {
            target_user: 0x03EA,
        };
        let b = encode_vec(&s).unwrap();
        assert_eq!(b, [0x01, 0x00, 0xEA, 0x03]); // messageType 1, targetUser 0x03EA
        assert_eq!(decode::<SynchronizePdu>(&b).unwrap(), s);
    }

    #[test]
    fn control_cooperate() {
        let c = ControlPdu {
            action: control_action::COOPERATE,
            grant_id: 0,
            control_id: 0,
        };
        let b = encode_vec(&c).unwrap();
        assert_eq!(b, [0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(decode::<ControlPdu>(&b).unwrap(), c);
    }

    #[test]
    fn font_list_bytes() {
        let b = encode_vec(&FontListPdu).unwrap();
        assert_eq!(b, [0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x32, 0x00]);
        assert_eq!(decode::<FontListPdu>(&b).unwrap(), FontListPdu);
    }
}

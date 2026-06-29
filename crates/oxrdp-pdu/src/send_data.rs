use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

const SEND_DATA_REQUEST: u8 = 0x64;
const SEND_DATA_INDICATION: u8 = 0x68;
const DATA_PRIORITY_SEGMENTATION: u8 = 0x70;
const MCS_USERCHANNEL_BASE: u16 = 1001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendDataRequest<'a> {
    pub initiator: u16,
    pub channel_id: u16,
    pub user_data: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendDataIndication<'a> {
    pub initiator: u16,
    pub channel_id: u16,
    pub user_data: &'a [u8],
}

fn per_length_size(len: usize) -> usize {
    if len < 0x80 {
        1
    } else {
        2
    }
}

fn per_write_length(dst: &mut WriteCursor<'_>, len: usize, ctx: &'static str) -> EncodeResult<()> {
    if len < 0x80 {
        dst.write_u8(len as u8, ctx)
    } else if len <= 0x7FFF {
        let first = 0x80 | ((len >> 8) as u8);
        let second = (len & 0xFF) as u8;
        dst.write_u8(first, ctx)?;
        dst.write_u8(second, ctx)?;
        Ok(())
    } else {
        Err(EncodeError::FieldTooLarge {
            context: ctx,
            field: "per length",
        })
    }
}

fn per_read_length(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<usize> {
    let b = src.read_u8(ctx)?;
    if b & 0x80 == 0 {
        Ok(b as usize)
    } else {
        let b2 = src.read_u8(ctx)?;
        Ok((((b & 0x7F) as usize) << 8) | b2 as usize)
    }
}

impl Encode for SendDataRequest<'_> {
    fn size(&self) -> usize {
        1 + 2 + 2 + 1 + per_length_size(self.user_data.len()) + self.user_data.len()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        const CTX: &str = "SendDataRequest";

        dst.write_u8(SEND_DATA_REQUEST, CTX)?;
        let initiator =
            self.initiator
                .checked_sub(MCS_USERCHANNEL_BASE)
                .ok_or(EncodeError::FieldTooLarge {
                    context: CTX,
                    field: "initiator",
                })?;
        dst.write_u16_be(initiator, CTX)?;
        dst.write_u16_be(self.channel_id, CTX)?;
        dst.write_u8(DATA_PRIORITY_SEGMENTATION, CTX)?;
        per_write_length(dst, self.user_data.len(), CTX)?;
        dst.write_slice(self.user_data, CTX)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for SendDataRequest<'de> {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        const CTX: &str = "SendDataRequest";

        let choice = src.read_u8(CTX)?;
        if choice != SEND_DATA_REQUEST {
            return Err(DecodeError::InvalidField {
                context: CTX,
                field: "mcs choice",
                reason: "unexpected send-data tag",
            });
        }
        let initiator = src.read_u16_be(CTX)?.saturating_add(MCS_USERCHANNEL_BASE);
        let channel_id = src.read_u16_be(CTX)?;
        let _ = src.read_u8(CTX)?;
        let len = per_read_length(src, CTX)?;
        let user_data = src.read_slice(len, CTX)?;
        Ok(Self {
            initiator,
            channel_id,
            user_data,
        })
    }
}

impl Encode for SendDataIndication<'_> {
    fn size(&self) -> usize {
        1 + 2 + 2 + 1 + per_length_size(self.user_data.len()) + self.user_data.len()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        const CTX: &str = "SendDataIndication";

        dst.write_u8(SEND_DATA_INDICATION, CTX)?;
        let initiator =
            self.initiator
                .checked_sub(MCS_USERCHANNEL_BASE)
                .ok_or(EncodeError::FieldTooLarge {
                    context: CTX,
                    field: "initiator",
                })?;
        dst.write_u16_be(initiator, CTX)?;
        dst.write_u16_be(self.channel_id, CTX)?;
        dst.write_u8(DATA_PRIORITY_SEGMENTATION, CTX)?;
        per_write_length(dst, self.user_data.len(), CTX)?;
        dst.write_slice(self.user_data, CTX)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for SendDataIndication<'de> {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        const CTX: &str = "SendDataIndication";

        let choice = src.read_u8(CTX)?;
        if choice != SEND_DATA_INDICATION {
            return Err(DecodeError::InvalidField {
                context: CTX,
                field: "mcs choice",
                reason: "unexpected send-data tag",
            });
        }
        let initiator = src.read_u16_be(CTX)?.saturating_add(MCS_USERCHANNEL_BASE);
        let channel_id = src.read_u16_be(CTX)?;
        let _ = src.read_u8(CTX)?;
        let len = per_read_length(src, CTX)?;
        let user_data = src.read_slice(len, CTX)?;
        Ok(Self {
            initiator,
            channel_id,
            user_data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn send_data_request_round_trip() {
        let r = SendDataRequest {
            initiator: 1007,
            channel_id: 1003,
            user_data: &[0xAA, 0xBB],
        };
        let bytes = encode_vec(&r).unwrap();
        assert_eq!(
            bytes,
            [0x64, 0x00, 0x06, 0x03, 0xEB, 0x70, 0x02, 0xAA, 0xBB]
        );
        assert_eq!(decode::<SendDataRequest>(&bytes).unwrap(), r);
    }

    #[test]
    fn send_data_indication_round_trip() {
        let r = SendDataIndication {
            initiator: 1002,
            channel_id: 1003,
            user_data: &[0xCC],
        };
        let bytes = encode_vec(&r).unwrap();
        assert_eq!(bytes, [0x68, 0x00, 0x01, 0x03, 0xEB, 0x70, 0x01, 0xCC]);
        assert_eq!(decode::<SendDataIndication>(&bytes).unwrap(), r);
    }

    #[test]
    fn two_byte_length() {
        let payload = vec![0x5A; 130]; // 130 = 0x82 -> per length encodes as [0x80, 0x82]
        let r = SendDataRequest {
            initiator: 1007,
            channel_id: 1003,
            user_data: &payload,
        };
        let bytes = encode_vec(&r).unwrap();
        assert_eq!(&bytes[..7], [0x64, 0x00, 0x06, 0x03, 0xEB, 0x70, 0x80]);
        assert_eq!(bytes[7], 0x82);
        assert_eq!(bytes.len(), 8 + 130);
        assert_eq!(decode::<SendDataRequest>(&bytes).unwrap(), r);
    }

    #[test]
    fn rejects_wrong_choice() {
        let err =
            decode::<SendDataRequest>(&[0x68, 0x00, 0x06, 0x03, 0xEB, 0x70, 0x00]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "mcs choice",
                ..
            }
        ));
    }
}

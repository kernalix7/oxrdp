use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};

const ERECT_DOMAIN_REQUEST: u8 = 0x04;
const ATTACH_USER_REQUEST: u8 = 0x28;
const ATTACH_USER_CONFIRM: u8 = 0x2E;
const CHANNEL_JOIN_REQUEST: u8 = 0x38;
const CHANNEL_JOIN_CONFIRM: u8 = 0x3E;

/// MCS user channels are assigned starting at this id; the on-wire `initiator` is `user_id - 1001`.
pub const MCS_USERCHANNEL_BASE: u16 = 1001;

fn per_write_integer16(
    dst: &mut WriteCursor<'_>,
    value: u16,
    min: u16,
    ctx: &'static str,
) -> EncodeResult<()> {
    let field = value.checked_sub(min).ok_or(EncodeError::FieldTooLarge {
        context: ctx,
        field: "integer16",
    })?;
    dst.write_u16_be(field, ctx)
}

fn per_read_integer16(src: &mut ReadCursor<'_>, min: u16, ctx: &'static str) -> DecodeResult<u16> {
    let field = src.read_u16_be(ctx)?;
    Ok(field.saturating_add(min))
}

/// ErectDomainRequest (client->server).
///
/// Wire form: `04 01 00 01 00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErectDomainRequest;

impl Encode for ErectDomainRequest {
    fn size(&self) -> usize {
        5
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(ERECT_DOMAIN_REQUEST, "MCS ErectDomainRequest")?;
        dst.write_slice(&[0x01, 0x00, 0x01, 0x00], "MCS ErectDomainRequest body")
    }
}

impl<'de> Decode<'de> for ErectDomainRequest {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("MCS ErectDomainRequest")?;
        if tag != ERECT_DOMAIN_REQUEST {
            return Err(DecodeError::InvalidField {
                context: "MCS ErectDomainRequest",
                field: "mcs choice",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        let mut body = [0u8; 4];
        let slice = src.read_slice(4, "MCS ErectDomainRequest body")?;
        body.copy_from_slice(slice);
        if body != [0x01, 0x00, 0x01, 0x00] {
            return Err(DecodeError::InvalidField {
                context: "MCS ErectDomainRequest",
                field: "erect domain body",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        Ok(ErectDomainRequest)
    }
}

/// AttachUserRequest (client->server).
///
/// Wire form: single byte `28`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachUserRequest;

impl Encode for AttachUserRequest {
    fn size(&self) -> usize {
        1
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(ATTACH_USER_REQUEST, "MCS AttachUserRequest")
    }
}

impl<'de> Decode<'de> for AttachUserRequest {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("MCS AttachUserRequest")?;
        if tag != ATTACH_USER_REQUEST {
            return Err(DecodeError::InvalidField {
                context: "MCS AttachUserRequest",
                field: "mcs choice",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        Ok(AttachUserRequest)
    }
}

/// AttachUserConfirm (server->client).
///
/// Wire form: `2E <result:u8> <initiator: per_integer16(min=1001)>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachUserConfirm {
    /// Result code; 0 indicates success.
    pub result: u8,
    /// Assigned user id (`user_id = on_wire_field + 1001`).
    pub user_id: u16,
}

impl Encode for AttachUserConfirm {
    fn size(&self) -> usize {
        4
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(ATTACH_USER_CONFIRM, "MCS AttachUserConfirm")?;
        dst.write_u8(self.result, "MCS AttachUserConfirm result")?;
        per_write_integer16(
            dst,
            self.user_id,
            MCS_USERCHANNEL_BASE,
            "MCS AttachUserConfirm initiator",
        )
    }
}

impl<'de> Decode<'de> for AttachUserConfirm {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("MCS AttachUserConfirm")?;
        if tag != ATTACH_USER_CONFIRM {
            return Err(DecodeError::InvalidField {
                context: "MCS AttachUserConfirm",
                field: "mcs choice",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        let result = src.read_u8("MCS AttachUserConfirm result")?;
        let user_id =
            per_read_integer16(src, MCS_USERCHANNEL_BASE, "MCS AttachUserConfirm initiator")?;
        Ok(AttachUserConfirm { result, user_id })
    }
}

/// ChannelJoinRequest (client->server).
///
/// Wire form: `38 <initiator: per_integer16(min=1001)> <channel_id: per_integer16(min=0)>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelJoinRequest {
    /// Initiator user id.
    pub user_id: u16,
    /// Channel id to join.
    pub channel_id: u16,
}

impl Encode for ChannelJoinRequest {
    fn size(&self) -> usize {
        5
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(CHANNEL_JOIN_REQUEST, "MCS ChannelJoinRequest")?;
        per_write_integer16(
            dst,
            self.user_id,
            MCS_USERCHANNEL_BASE,
            "MCS ChannelJoinRequest initiator",
        )?;
        per_write_integer16(dst, self.channel_id, 0, "MCS ChannelJoinRequest channel id")
    }
}

impl<'de> Decode<'de> for ChannelJoinRequest {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("MCS ChannelJoinRequest")?;
        if tag != CHANNEL_JOIN_REQUEST {
            return Err(DecodeError::InvalidField {
                context: "MCS ChannelJoinRequest",
                field: "mcs choice",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        let user_id = per_read_integer16(
            src,
            MCS_USERCHANNEL_BASE,
            "MCS ChannelJoinRequest initiator",
        )?;
        let channel_id = per_read_integer16(src, 0, "MCS ChannelJoinRequest channel id")?;
        Ok(ChannelJoinRequest {
            user_id,
            channel_id,
        })
    }
}

/// ChannelJoinConfirm (server->client).
///
/// Wire form: `3E <result:u8> <initiator: per_integer16(min=1001)>
/// <requested: per_integer16(min=0)> <channel_id: per_integer16(min=0)>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelJoinConfirm {
    /// Result code; 0 indicates success.
    pub result: u8,
    /// Initiator user id.
    pub user_id: u16,
    /// Requested channel id.
    pub requested_channel_id: u16,
    /// Joined channel id.
    pub channel_id: u16,
}

impl Encode for ChannelJoinConfirm {
    fn size(&self) -> usize {
        8
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(CHANNEL_JOIN_CONFIRM, "MCS ChannelJoinConfirm")?;
        dst.write_u8(self.result, "MCS ChannelJoinConfirm result")?;
        per_write_integer16(
            dst,
            self.user_id,
            MCS_USERCHANNEL_BASE,
            "MCS ChannelJoinConfirm initiator",
        )?;
        per_write_integer16(
            dst,
            self.requested_channel_id,
            0,
            "MCS ChannelJoinConfirm requested channel id",
        )?;
        per_write_integer16(dst, self.channel_id, 0, "MCS ChannelJoinConfirm channel id")
    }
}

impl<'de> Decode<'de> for ChannelJoinConfirm {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("MCS ChannelJoinConfirm")?;
        if tag != CHANNEL_JOIN_CONFIRM {
            return Err(DecodeError::InvalidField {
                context: "MCS ChannelJoinConfirm",
                field: "mcs choice",
                reason: "unexpected DomainMCSPDU tag",
            });
        }
        let result = src.read_u8("MCS ChannelJoinConfirm result")?;
        let user_id = per_read_integer16(
            src,
            MCS_USERCHANNEL_BASE,
            "MCS ChannelJoinConfirm initiator",
        )?;
        let requested_channel_id =
            per_read_integer16(src, 0, "MCS ChannelJoinConfirm requested channel id")?;
        let channel_id = per_read_integer16(src, 0, "MCS ChannelJoinConfirm channel id")?;
        Ok(ChannelJoinConfirm {
            result,
            user_id,
            requested_channel_id,
            channel_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn erect_domain_request_bytes() {
        let bytes = encode_vec(&ErectDomainRequest).unwrap();
        assert_eq!(bytes, [0x04, 0x01, 0x00, 0x01, 0x00]);
        assert_eq!(
            decode::<ErectDomainRequest>(&bytes).unwrap(),
            ErectDomainRequest
        );
    }

    #[test]
    fn attach_user_request_bytes() {
        let bytes = encode_vec(&AttachUserRequest).unwrap();
        assert_eq!(bytes, [0x28]);
        assert_eq!(
            decode::<AttachUserRequest>(&bytes).unwrap(),
            AttachUserRequest
        );
    }

    #[test]
    fn attach_user_confirm_user_1007() {
        let c = AttachUserConfirm {
            result: 0,
            user_id: 1007,
        };
        let bytes = encode_vec(&c).unwrap();
        assert_eq!(bytes, [0x2E, 0x00, 0x00, 0x06]); // initiator field = 1007 - 1001 = 6
        assert_eq!(decode::<AttachUserConfirm>(&bytes).unwrap(), c);
    }

    #[test]
    fn channel_join_request_user_1007_chan_1003() {
        let r = ChannelJoinRequest {
            user_id: 1007,
            channel_id: 1003,
        };
        let bytes = encode_vec(&r).unwrap();
        assert_eq!(bytes, [0x38, 0x00, 0x06, 0x03, 0xEB]); // 6, then 1003=0x03EB
        assert_eq!(decode::<ChannelJoinRequest>(&bytes).unwrap(), r);
    }

    #[test]
    fn channel_join_confirm_round_trip() {
        let c = ChannelJoinConfirm {
            result: 0,
            user_id: 1007,
            requested_channel_id: 1003,
            channel_id: 1003,
        };
        let bytes = encode_vec(&c).unwrap();
        assert_eq!(bytes, [0x3E, 0x00, 0x00, 0x06, 0x03, 0xEB, 0x03, 0xEB]);
        assert_eq!(decode::<ChannelJoinConfirm>(&bytes).unwrap(), c);
    }

    #[test]
    fn rejects_wrong_choice() {
        let err = decode::<AttachUserConfirm>(&[0x38, 0x00, 0x00, 0x06]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "mcs choice",
                ..
            }
        ));
    }
}

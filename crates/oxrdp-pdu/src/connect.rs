use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeError, EncodeResult};
use crate::nego::{NegotiationFailure, NegotiationRequest, NegotiationResponse};

const CR_CDT: u8 = 0xE0;
const CC_CDT: u8 = 0xD0;
const COOKIE_PREFIX: &[u8] = b"Cookie: mstshash=";
const FIXED_HEADER_LEN: usize = 6;

/// X.224 Connection Request TPDU (client -> server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRequest {
    /// The `mstshash` identifier (WITHOUT the `Cookie: mstshash=` prefix or trailing CR LF). None = no cookie.
    pub cookie: Option<String>,
    pub negotiation: Option<NegotiationRequest>,
}

/// Negotiation result carried in a Connection Confirm TPDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiationConfirm {
    Response(NegotiationResponse),
    Failure(NegotiationFailure),
}

/// X.224 Connection Confirm TPDU (server -> client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionConfirm {
    pub negotiation: Option<NegotiationConfirm>,
}

impl ConnectionRequest {
    fn variable_size(&self) -> usize {
        let cookie_len = self
            .cookie
            .as_ref()
            .map(|c| COOKIE_PREFIX.len() + c.len() + 2)
            .unwrap_or(0);
        let nego_len = self
            .negotiation
            .as_ref()
            .map(NegotiationRequest::size)
            .unwrap_or(0);
        cookie_len + nego_len
    }
}

impl Encode for ConnectionRequest {
    fn size(&self) -> usize {
        1 + FIXED_HEADER_LEN + self.variable_size()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let li = u8::try_from(FIXED_HEADER_LEN + self.variable_size()).map_err(|_| {
            EncodeError::FieldTooLarge {
                context: "X.224 CR",
                field: "length indicator",
            }
        })?;
        dst.write_u8(li, "X.224 CR LI")?;
        dst.write_u8(CR_CDT, "X.224 CR CDT")?;
        dst.write_u16_be(0x0000, "X.224 CR DST-REF")?;
        dst.write_u16_be(0x0000, "X.224 CR SRC-REF")?;
        dst.write_u8(0x00, "X.224 CR CLASS-OPTION")?;

        if let Some(cookie) = &self.cookie {
            dst.write_slice(COOKIE_PREFIX, "X.224 CR cookie prefix")?;
            dst.write_slice(cookie.as_bytes(), "X.224 CR cookie identifier")?;
            dst.write_slice(b"\r\n", "X.224 CR cookie terminator")?;
        }

        if let Some(negotiation) = &self.negotiation {
            negotiation.encode(dst)?;
        }

        Ok(())
    }
}

impl<'de> Decode<'de> for ConnectionRequest {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let li = src.read_u8("X.224 CR LI")?;
        let cdt = src.read_u8("X.224 CR CDT")?;
        if cdt != CR_CDT {
            return Err(DecodeError::InvalidField {
                context: "X.224 CR",
                field: "tpdu code",
                reason: "expected CR-CDT 0xE0",
            });
        }
        let _dst_ref = src.read_u16_be("X.224 CR DST-REF")?;
        let _src_ref = src.read_u16_be("X.224 CR SRC-REF")?;
        let _class = src.read_u8("X.224 CR CLASS-OPTION")?;

        let variable_len =
            (li as usize)
                .checked_sub(FIXED_HEADER_LEN)
                .ok_or(DecodeError::InvalidLength {
                    context: "X.224 CR",
                    reason: "LI smaller than fixed header length",
                })?;

        let sub = src.read_slice(variable_len, "X.224 CR variable part")?;
        let mut sub_cursor = ReadCursor::new(sub);

        let mut cookie = None;
        if sub.starts_with(COOKIE_PREFIX) {
            sub_cursor.read_slice(COOKIE_PREFIX.len(), "X.224 CR cookie prefix")?;
            let remaining = sub_cursor.remaining();
            let body = sub_cursor.read_slice(remaining, "X.224 CR cookie body")?;
            let crlf_pos =
                body.windows(2)
                    .position(|w| w == b"\r\n")
                    .ok_or(DecodeError::InvalidField {
                        context: "X.224 CR",
                        field: "cookie",
                        reason: "missing CR LF terminator",
                    })?;
            cookie = Some(String::from_utf8_lossy(&body[..crlf_pos]).into_owned());
            sub_cursor = ReadCursor::new(&body[crlf_pos + 2..]);
        }

        let negotiation = if sub_cursor.is_empty() {
            None
        } else {
            Some(NegotiationRequest::decode(&mut sub_cursor)?)
        };

        Ok(ConnectionRequest {
            cookie,
            negotiation,
        })
    }
}

impl ConnectionConfirm {
    fn variable_size(&self) -> usize {
        self.negotiation
            .as_ref()
            .map(NegotiationConfirm::size)
            .unwrap_or(0)
    }
}

impl NegotiationConfirm {
    fn size(&self) -> usize {
        match self {
            NegotiationConfirm::Response(r) => r.size(),
            NegotiationConfirm::Failure(f) => f.size(),
        }
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        match self {
            NegotiationConfirm::Response(r) => r.encode(dst),
            NegotiationConfirm::Failure(f) => f.encode(dst),
        }
    }
}

impl Encode for ConnectionConfirm {
    fn size(&self) -> usize {
        1 + FIXED_HEADER_LEN + self.variable_size()
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let li = u8::try_from(FIXED_HEADER_LEN + self.variable_size()).map_err(|_| {
            EncodeError::FieldTooLarge {
                context: "X.224 CC",
                field: "length indicator",
            }
        })?;
        dst.write_u8(li, "X.224 CC LI")?;
        dst.write_u8(CC_CDT, "X.224 CC CDT")?;
        dst.write_u16_be(0x0000, "X.224 CC DST-REF")?;
        dst.write_u16_be(0x0000, "X.224 CC SRC-REF")?;
        dst.write_u8(0x00, "X.224 CC CLASS-OPTION")?;

        if let Some(negotiation) = &self.negotiation {
            negotiation.encode(dst)?;
        }

        Ok(())
    }
}

impl<'de> Decode<'de> for ConnectionConfirm {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let li = src.read_u8("X.224 CC LI")?;
        let cdt = src.read_u8("X.224 CC CDT")?;
        if cdt != CC_CDT {
            return Err(DecodeError::InvalidField {
                context: "X.224 CC",
                field: "tpdu code",
                reason: "expected CC-CDT 0xD0",
            });
        }
        let _dst_ref = src.read_u16_be("X.224 CC DST-REF")?;
        let _src_ref = src.read_u16_be("X.224 CC SRC-REF")?;
        let _class = src.read_u8("X.224 CC CLASS-OPTION")?;

        let variable_len =
            (li as usize)
                .checked_sub(FIXED_HEADER_LEN)
                .ok_or(DecodeError::InvalidLength {
                    context: "X.224 CC",
                    reason: "LI smaller than fixed header length",
                })?;

        let sub = src.read_slice(variable_len, "X.224 CC variable part")?;
        let mut sub_cursor = ReadCursor::new(sub);

        let negotiation = if sub_cursor.is_empty() {
            None
        } else {
            let nego_type = sub_cursor.peek_u8("X.224 CC negotiation type")?;
            match nego_type {
                0x02 => Some(NegotiationConfirm::Response(NegotiationResponse::decode(
                    &mut sub_cursor,
                )?)),
                0x03 => Some(NegotiationConfirm::Failure(NegotiationFailure::decode(
                    &mut sub_cursor,
                )?)),
                _ => {
                    return Err(DecodeError::InvalidField {
                        context: "X.224 CC",
                        field: "negotiation type",
                        reason: "expected 0x02 or 0x03",
                    });
                }
            }
        };

        Ok(ConnectionConfirm { negotiation })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};
    use crate::nego::protocol;

    #[test]
    fn cr_no_cookie_ssl() {
        let cr = ConnectionRequest {
            cookie: None,
            negotiation: Some(NegotiationRequest {
                flags: 0,
                requested_protocols: protocol::SSL,
            }),
        };
        let bytes = encode_vec(&cr).unwrap();
        // LI=0x0E, CR=0xE0, DST=0000, SRC=0000, CLASS=00, then NEG_REQ(SSL)
        assert_eq!(
            bytes,
            [0x0E, 0xE0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode::<ConnectionRequest>(&bytes).unwrap(), cr);
    }

    #[test]
    fn cr_with_cookie_round_trip() {
        let cr = ConnectionRequest {
            cookie: Some("elton".to_string()),
            negotiation: Some(NegotiationRequest {
                flags: 0,
                requested_protocols: protocol::SSL,
            }),
        };
        let bytes = encode_vec(&cr).unwrap();
        let token = b"Cookie: mstshash=elton\r\n";
        let mut expected = Vec::new();
        expected.push((6 + token.len() + 8) as u8); // LI
        expected.push(0xE0);
        expected.extend_from_slice(&[0, 0, 0, 0, 0]); // DST, SRC, CLASS
        expected.extend_from_slice(token);
        expected.extend_from_slice(&[0x01, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(bytes, expected);
        assert_eq!(decode::<ConnectionRequest>(&bytes).unwrap(), cr);
    }

    #[test]
    fn cc_response_tls() {
        let cc = ConnectionConfirm {
            negotiation: Some(NegotiationConfirm::Response(NegotiationResponse {
                flags: 0,
                selected_protocol: protocol::SSL,
            })),
        };
        let bytes = encode_vec(&cc).unwrap();
        assert_eq!(
            bytes,
            [0x0E, 0xD0, 0, 0, 0, 0, 0, 0x02, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode::<ConnectionConfirm>(&bytes).unwrap(), cc);
    }

    #[test]
    fn cc_failure() {
        let cc = ConnectionConfirm {
            negotiation: Some(NegotiationConfirm::Failure(NegotiationFailure {
                failure_code: 0x0000_0001,
            })),
        };
        let bytes = encode_vec(&cc).unwrap();
        assert_eq!(
            bytes,
            [0x0E, 0xD0, 0, 0, 0, 0, 0, 0x03, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
        assert_eq!(decode::<ConnectionConfirm>(&bytes).unwrap(), cc);
    }

    #[test]
    fn cr_rejects_bad_code() {
        let err = decode::<ConnectionRequest>(&[
            0x0E, 0xD0, 0, 0, 0, 0, 0, 0x01, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00,
        ])
        .unwrap_err();
        assert!(matches!(err, DecodeError::InvalidField { .. }));
    }
}

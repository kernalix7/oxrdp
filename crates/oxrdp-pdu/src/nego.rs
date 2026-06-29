use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeResult};

pub mod protocol {
    pub const RDP: u32 = 0x0000_0000;
    pub const SSL: u32 = 0x0000_0001;
    pub const HYBRID: u32 = 0x0000_0002;
    pub const RDSTLS: u32 = 0x0000_0004;
    pub const HYBRID_EX: u32 = 0x0000_0008;
    pub const RDSAAD: u32 = 0x0000_0010;
}

pub const RDP_NEG_LEN: usize = 8;

pub const TYPE_NEG_REQ: u8 = 0x01;
pub const TYPE_NEG_RSP: u8 = 0x02;
pub const TYPE_NEG_FAILURE: u8 = 0x03;

pub mod failure_code {
    pub const SSL_REQUIRED_BY_SERVER: u32 = 0x0000_0001;
    pub const SSL_NOT_ALLOWED_BY_SERVER: u32 = 0x0000_0002;
    pub const SSL_CERT_NOT_ON_SERVER: u32 = 0x0000_0003;
    pub const INCONSISTENT_FLAGS: u32 = 0x0000_0004;
    pub const HYBRID_REQUIRED_BY_SERVER: u32 = 0x0000_0005;
    pub const SSL_WITH_USER_AUTH_REQUIRED_BY_SERVER: u32 = 0x0000_0006;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiationRequest {
    pub flags: u8,
    pub requested_protocols: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiationResponse {
    pub flags: u8,
    pub selected_protocol: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiationFailure {
    pub failure_code: u32,
}

impl NegotiationRequest {
    const CTX: &'static str = "RDP_NEG_REQ";
}

impl NegotiationResponse {
    const CTX: &'static str = "RDP_NEG_RSP";
}

impl NegotiationFailure {
    const CTX: &'static str = "RDP_NEG_FAILURE";
}

impl Encode for NegotiationRequest {
    fn size(&self) -> usize {
        RDP_NEG_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(TYPE_NEG_REQ, Self::CTX)?;
        dst.write_u8(self.flags, Self::CTX)?;
        dst.write_u16_le(RDP_NEG_LEN as u16, Self::CTX)?;
        dst.write_u32_le(self.requested_protocols, Self::CTX)?;
        Ok(())
    }
}

impl Encode for NegotiationResponse {
    fn size(&self) -> usize {
        RDP_NEG_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(TYPE_NEG_RSP, Self::CTX)?;
        dst.write_u8(self.flags, Self::CTX)?;
        dst.write_u16_le(RDP_NEG_LEN as u16, Self::CTX)?;
        dst.write_u32_le(self.selected_protocol, Self::CTX)?;
        Ok(())
    }
}

impl Encode for NegotiationFailure {
    fn size(&self) -> usize {
        RDP_NEG_LEN
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_u8(TYPE_NEG_FAILURE, Self::CTX)?;
        dst.write_u8(0, Self::CTX)?;
        dst.write_u16_le(RDP_NEG_LEN as u16, Self::CTX)?;
        dst.write_u32_le(self.failure_code, Self::CTX)?;
        Ok(())
    }
}

impl<'de> Decode<'de> for NegotiationRequest {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let typ = src.read_u8(Self::CTX)?;
        if typ != TYPE_NEG_REQ {
            return Err(DecodeError::InvalidField {
                context: Self::CTX,
                field: "type",
                reason: "unexpected negotiation type",
            });
        }
        let flags = src.read_u8(Self::CTX)?;
        let length = src.read_u16_le(Self::CTX)?;
        if length as usize != RDP_NEG_LEN {
            return Err(DecodeError::InvalidLength {
                context: Self::CTX,
                reason: "length must be 8",
            });
        }
        let requested_protocols = src.read_u32_le(Self::CTX)?;
        Ok(Self {
            flags,
            requested_protocols,
        })
    }
}

impl<'de> Decode<'de> for NegotiationResponse {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let typ = src.read_u8(Self::CTX)?;
        if typ != TYPE_NEG_RSP {
            return Err(DecodeError::InvalidField {
                context: Self::CTX,
                field: "type",
                reason: "unexpected negotiation type",
            });
        }
        let flags = src.read_u8(Self::CTX)?;
        let length = src.read_u16_le(Self::CTX)?;
        if length as usize != RDP_NEG_LEN {
            return Err(DecodeError::InvalidLength {
                context: Self::CTX,
                reason: "length must be 8",
            });
        }
        let selected_protocol = src.read_u32_le(Self::CTX)?;
        Ok(Self {
            flags,
            selected_protocol,
        })
    }
}

impl<'de> Decode<'de> for NegotiationFailure {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let typ = src.read_u8(Self::CTX)?;
        if typ != TYPE_NEG_FAILURE {
            return Err(DecodeError::InvalidField {
                context: Self::CTX,
                field: "type",
                reason: "unexpected negotiation type",
            });
        }
        let _ = src.read_u8(Self::CTX)?;
        let length = src.read_u16_le(Self::CTX)?;
        if length as usize != RDP_NEG_LEN {
            return Err(DecodeError::InvalidLength {
                context: Self::CTX,
                reason: "length must be 8",
            });
        }
        let failure_code = src.read_u32_le(Self::CTX)?;
        Ok(Self { failure_code })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn neg_req_ssl_round_trip() {
        let req = NegotiationRequest {
            flags: 0,
            requested_protocols: protocol::SSL,
        };
        let bytes = encode_vec(&req).unwrap();
        assert_eq!(bytes, [0x01, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(decode::<NegotiationRequest>(&bytes).unwrap(), req);
    }

    #[test]
    fn neg_rsp_tls_selected() {
        let rsp = NegotiationResponse {
            flags: 0x01,
            selected_protocol: protocol::SSL,
        };
        let bytes = encode_vec(&rsp).unwrap();
        assert_eq!(bytes, [0x02, 0x01, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(decode::<NegotiationResponse>(&bytes).unwrap(), rsp);
    }

    #[test]
    fn neg_failure_round_trip() {
        let f = NegotiationFailure {
            failure_code: 0x0000_0001,
        };
        let bytes = encode_vec(&f).unwrap();
        assert_eq!(bytes, [0x03, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(decode::<NegotiationFailure>(&bytes).unwrap(), f);
    }

    #[test]
    fn rejects_wrong_type() {
        let err = decode::<NegotiationRequest>(&[0x02, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00])
            .unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField { field: "type", .. }
        ));
    }

    #[test]
    fn rejects_bad_length() {
        let err = decode::<NegotiationRequest>(&[0x01, 0x00, 0x07, 0x00, 0x01, 0x00, 0x00, 0x00])
            .unwrap_err();
        assert!(matches!(err, DecodeError::InvalidLength { .. }));
    }
}

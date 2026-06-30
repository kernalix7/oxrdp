use crate::ber::{length_size, read_length, write_length};
use crate::codec::{Decode, Encode};
use crate::cursor::{ReadCursor, WriteCursor};
use crate::error::{DecodeError, DecodeResult, EncodeResult};

/// Minimal content bytes (without tag/length) of `value` as an unsigned BER INTEGER.
fn integer_content(value: u32) -> Vec<u8> {
    let be = value.to_be_bytes();
    let mut i = 0;
    while i < 3 && be[i] == 0 {
        i += 1;
    }
    let mut content = be[i..].to_vec();
    if content[0] & 0x80 != 0 {
        content.insert(0, 0x00);
    }
    content
}

fn integer_size(value: u32) -> usize {
    let c = integer_content(value).len();
    1 + length_size(c) + c
}

fn write_integer(dst: &mut WriteCursor<'_>, value: u32, ctx: &'static str) -> EncodeResult<()> {
    let c = integer_content(value);
    dst.write_u8(0x02, ctx)?;
    write_length(dst, c.len(), ctx)?;
    dst.write_slice(&c, ctx)
}

fn read_integer(src: &mut ReadCursor<'_>, ctx: &'static str) -> DecodeResult<u32> {
    let tag = src.read_u8(ctx)?;
    if tag != 0x02 {
        return Err(DecodeError::InvalidField {
            context: ctx,
            field: "ber tag",
            reason: "expected INTEGER 0x02",
        });
    }
    let len = read_length(src, ctx)?;
    if len == 0 || len > 5 {
        return Err(DecodeError::InvalidLength {
            context: ctx,
            reason: "bad INTEGER length",
        });
    }
    let mut v: u32 = 0;
    for _ in 0..len {
        v = (v << 8) | u32::from(src.read_u8(ctx)?);
    }
    Ok(v)
}

/// MCS `DomainParameters` BER structure (T.125).
///
/// Encoded as a `SEQUENCE` of eight `INTEGER` values used inside
/// `Connect-Initial` / `Connect-Response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainParameters {
    pub max_channel_ids: u32,
    pub max_user_ids: u32,
    pub max_token_ids: u32,
    pub num_priorities: u32,
    pub min_throughput: u32,
    pub max_height: u32,
    pub max_mcs_pdu_size: u32,
    pub protocol_version: u32,
}

impl DomainParameters {
    /// Standard RDP target parameters.
    pub fn target() -> Self {
        Self {
            max_channel_ids: 34,
            max_user_ids: 2,
            max_token_ids: 0,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 65_535,
            protocol_version: 2,
        }
    }

    /// Standard RDP minimum parameters.
    pub fn minimum() -> Self {
        Self {
            max_channel_ids: 1,
            max_user_ids: 1,
            max_token_ids: 1,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 1_056,
            protocol_version: 2,
        }
    }

    /// Standard RDP maximum parameters.
    pub fn maximum() -> Self {
        Self {
            max_channel_ids: 65_535,
            max_user_ids: 64_535,
            max_token_ids: 65_535,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 65_535,
            protocol_version: 2,
        }
    }

    fn fields(&self) -> [u32; 8] {
        [
            self.max_channel_ids,
            self.max_user_ids,
            self.max_token_ids,
            self.num_priorities,
            self.min_throughput,
            self.max_height,
            self.max_mcs_pdu_size,
            self.protocol_version,
        ]
    }

    fn content_len(&self) -> usize {
        self.fields().iter().map(|&v| integer_size(v)).sum()
    }
}

impl Encode for DomainParameters {
    fn size(&self) -> usize {
        let content_len = self.content_len();
        1 + length_size(content_len) + content_len
    }

    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let content_len = self.content_len();
        dst.write_u8(0x30, "domain params sequence tag")?;
        write_length(dst, content_len, "domain params sequence length")?;
        for value in self.fields() {
            write_integer(dst, value, "domain params integer")?;
        }
        Ok(())
    }
}

impl<'de> Decode<'de> for DomainParameters {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        let tag = src.read_u8("domain params sequence tag")?;
        if tag != 0x30 {
            return Err(DecodeError::InvalidField {
                context: "domain params sequence tag",
                field: "ber tag",
                reason: "expected SEQUENCE 0x30",
            });
        }
        let _len = read_length(src, "domain params sequence length")?;
        Ok(Self {
            max_channel_ids: read_integer(src, "max_channel_ids")?,
            max_user_ids: read_integer(src, "max_user_ids")?,
            max_token_ids: read_integer(src, "max_token_ids")?,
            num_priorities: read_integer(src, "num_priorities")?,
            min_throughput: read_integer(src, "min_throughput")?,
            max_height: read_integer(src, "max_height")?,
            max_mcs_pdu_size: read_integer(src, "max_mcs_pdu_size")?,
            protocol_version: read_integer(src, "protocol_version")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode_vec};

    #[test]
    fn target_exact_bytes() {
        let bytes = encode_vec(&DomainParameters::target()).unwrap();
        assert_eq!(
            bytes,
            [
                0x30, 0x1A, 0x02, 0x01, 0x22, // 34
                0x02, 0x01, 0x02, // 2
                0x02, 0x01, 0x00, // 0
                0x02, 0x01, 0x01, // 1
                0x02, 0x01, 0x00, // 0
                0x02, 0x01, 0x01, // 1
                0x02, 0x03, 0x00, 0xFF, 0xFF, // 65535
                0x02, 0x01, 0x02, // 2
            ]
        );
        assert_eq!(
            decode::<DomainParameters>(&bytes).unwrap(),
            DomainParameters::target()
        );
    }

    #[test]
    fn min_and_max_round_trip() {
        for dp in [DomainParameters::minimum(), DomainParameters::maximum()] {
            let bytes = encode_vec(&dp).unwrap();
            assert_eq!(decode::<DomainParameters>(&bytes).unwrap(), dp);
        }
    }

    #[test]
    fn max_high_value_has_sign_byte() {
        let bytes = encode_vec(&DomainParameters::maximum()).unwrap();
        assert!(bytes
            .windows(5)
            .any(|w| w == [0x02, 0x03, 0x00, 0xFC, 0x17]));
    }

    #[test]
    fn rejects_wrong_sequence_tag() {
        let err = decode::<DomainParameters>(&[0x31, 0x00]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField {
                field: "ber tag",
                ..
            }
        ));
    }
}

use crate::codec::decode;
use crate::cursor::ReadCursor;
use crate::error::{DecodeError, DecodeResult};
use crate::gcc_server::{ServerCoreData, ServerNetworkData};

/// The server's MCS Connect-Response PDU.
///
/// This structure is the result of parsing the BER-encoded MCS Connect-Response,
/// which wraps a GCC Conference Create Response (PER), which in turn wraps the
/// server data blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectResponse {
    /// The MCS Connect-Response result code.
    pub result: u8,
    /// The server core data block, if present.
    pub server_core: Option<ServerCoreData>,
    /// The server network data block, if present.
    pub server_network: Option<ServerNetworkData>,
}

impl ConnectResponse {
    /// Parse an MCS Connect-Response PDU from a byte slice.
    pub fn from_bytes(data: &[u8]) -> DecodeResult<Self> {
        let mut cursor = ReadCursor::new(data);

        // BER envelope: Connect-Response application tag 0x7F 0x66.
        let tag0 = cursor.read_u8("MCS Connect-Response")?;
        let tag1 = cursor.read_u8("MCS Connect-Response")?;
        if tag0 != 0x7F || tag1 != 0x66 {
            return Err(DecodeError::InvalidField {
                context: "MCS Connect-Response",
                field: "tag",
                reason: "expected 0x7F 0x66",
            });
        }

        // BER length of the Connect-Response body.
        let _body_len = read_ber_length(&mut cursor, "MCS Connect-Response")?;

        // result: ENUMERATED (0x0A 0x01 <byte>).
        let result_tag = cursor.read_u8("MCS Connect-Response")?;
        if result_tag != 0x0A {
            return Err(DecodeError::InvalidField {
                context: "MCS Connect-Response",
                field: "result tag",
                reason: "expected 0x0A",
            });
        }
        let result_len = read_ber_length(&mut cursor, "MCS Connect-Response")?;
        let result = cursor.read_u8("MCS Connect-Response")?;
        // The length should be 1, but we are lenient about the exact value as long
        // as we can read a single byte.
        if result_len > 1 {
            cursor.read_slice(result_len - 1, "MCS Connect-Response")?;
        }

        // calledConnectId: INTEGER (0x02 <len> <bytes>).
        let cci_tag = cursor.read_u8("MCS Connect-Response")?;
        if cci_tag != 0x02 {
            return Err(DecodeError::InvalidField {
                context: "MCS Connect-Response",
                field: "calledConnectId tag",
                reason: "expected 0x02",
            });
        }
        let cci_len = read_ber_length(&mut cursor, "MCS Connect-Response")?;
        cursor.read_slice(cci_len, "MCS Connect-Response")?;

        // DomainParameters: SEQUENCE (0x30 <len> <bytes>).
        let dp_tag = cursor.read_u8("MCS Connect-Response")?;
        if dp_tag != 0x30 {
            return Err(DecodeError::InvalidField {
                context: "MCS Connect-Response",
                field: "DomainParameters tag",
                reason: "expected 0x30",
            });
        }
        let dp_len = read_ber_length(&mut cursor, "MCS Connect-Response")?;
        cursor.read_slice(dp_len, "MCS Connect-Response")?;

        // userData: OCTET STRING (0x04 <ber len> <gccCCResp>).
        let ud_tag = cursor.read_u8("MCS Connect-Response")?;
        if ud_tag != 0x04 {
            return Err(DecodeError::InvalidField {
                context: "MCS Connect-Response",
                field: "userData tag",
                reason: "expected 0x04",
            });
        }
        let ud_len = read_ber_length(&mut cursor, "MCS Connect-Response")?;
        let gcc_cc_resp = cursor.read_slice(ud_len, "MCS Connect-Response")?;

        // Inside gccCCResp (PER), locate the H.221 server key "McDn".
        let mcdn_pattern: [u8; 4] = [0x4D, 0x63, 0x44, 0x6E];
        let mcdn_pos =
            find_pattern(gcc_cc_resp, &mcdn_pattern).ok_or(DecodeError::InvalidField {
                context: "GCC Conference Create Response",
                field: "h221 key",
                reason: "McDn not found",
            })?;

        let after_mcdn = &gcc_cc_resp[mcdn_pos + 4..];
        let mut per_cursor = ReadCursor::new(after_mcdn);
        let _per_len = read_per_length(&mut per_cursor, "GCC Conference Create Response")?;
        // The server data blocks are everything after the PER length.
        let blocks = &after_mcdn[per_cursor.position()..];

        let mut server_core = None;
        let mut server_network = None;

        let mut off = 0usize;
        while off + 4 <= blocks.len() {
            let ty = u16::from_le_bytes([blocks[off], blocks[off + 1]]);
            let blen = u16::from_le_bytes([blocks[off + 2], blocks[off + 3]]) as usize;

            if blen < 4 || off + blen > blocks.len() {
                return Err(DecodeError::InvalidLength {
                    context: "GCC server block",
                    reason: "bad block length",
                });
            }

            let block_bytes = &blocks[off..off + blen];
            match ty {
                0x0C01 => server_core = Some(decode::<ServerCoreData>(block_bytes)?),
                0x0C03 => server_network = Some(decode::<ServerNetworkData>(block_bytes)?),
                _ => {}
            }
            off += blen;
        }

        Ok(Self {
            result,
            server_core,
            server_network,
        })
    }
}

/// Read a BER length from the cursor.
///
/// A length byte with the high bit clear indicates a short form (0..=127).
/// A length byte with the high bit set indicates a long form, where the low 7
/// bits give the number of subsequent bytes containing the big-endian length.
fn read_ber_length(c: &mut ReadCursor, ctx: &'static str) -> DecodeResult<usize> {
    let b = c.read_u8(ctx)?;
    if b < 0x80 {
        Ok(b as usize)
    } else {
        let n = (b & 0x7F) as usize;
        if n == 0 || n > 4 {
            return Err(DecodeError::InvalidLength {
                context: ctx,
                reason: "invalid BER length form",
            });
        }
        let mut len = 0usize;
        for _ in 0..n {
            let next = c.read_u8(ctx)?;
            len = (len << 8) | (next as usize);
        }
        Ok(len)
    }
}

/// Read a PER length from the cursor.
///
/// A length byte with the high bit clear indicates a short form (0..=127).
/// A length byte with the high bit set indicates a two-byte form, where the
/// low 7 bits of the first byte and the 8 bits of the second byte form the
/// length.
fn read_per_length(c: &mut ReadCursor, ctx: &'static str) -> DecodeResult<usize> {
    let b = c.read_u8(ctx)?;
    if b & 0x80 == 0 {
        Ok(b as usize)
    } else {
        let b2 = c.read_u8(ctx)?;
        Ok((((b & 0x7F) as usize) << 8) | (b2 as usize))
    }
}

/// Find the first occurrence of a pattern in a slice.
fn find_pattern(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let end = haystack.len() - needle.len();
    for i in 0..=end {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::encode_vec;

    fn ber_len(out: &mut Vec<u8>, len: usize) {
        if len < 0x80 {
            out.push(len as u8);
        } else {
            let mut b = Vec::new();
            let mut v = len;
            while v > 0 {
                b.insert(0, (v & 0xFF) as u8);
                v >>= 8;
            }
            out.push(0x80 | b.len() as u8);
            out.extend_from_slice(&b);
        }
    }
    fn per_len(out: &mut Vec<u8>, len: usize) {
        if len < 0x80 {
            out.push(len as u8);
        } else {
            out.push(0x80 | (len >> 8) as u8);
            out.push((len & 0xFF) as u8);
        }
    }

    fn build_response(core: &ServerCoreData, net: &ServerNetworkData) -> Vec<u8> {
        let mut blocks = Vec::new();
        blocks.extend_from_slice(&encode_vec(core).unwrap());
        blocks.extend_from_slice(&encode_vec(net).unwrap());

        let mut gcc = Vec::new();
        gcc.extend_from_slice(&[0x00, 0x05, 0x00, 0x14, 0x7C, 0x00, 0x01]);
        per_len(&mut gcc, blocks.len() + 14);
        gcc.extend_from_slice(&[0x2A, 0x14, 0x76, 0x0A, 0x01, 0x01, 0x00, 0x01, 0xC0, 0x00]);
        gcc.extend_from_slice(b"McDn");
        per_len(&mut gcc, blocks.len());
        gcc.extend_from_slice(&blocks);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x0A, 0x01, 0x00]); // result = 0
        body.extend_from_slice(&[0x02, 0x01, 0x00]); // calledConnectId = 0
        body.extend_from_slice(&[0x30, 0x00]); // empty DomainParameters
        body.push(0x04);
        ber_len(&mut body, gcc.len());
        body.extend_from_slice(&gcc);

        let mut out = Vec::new();
        out.extend_from_slice(&[0x7F, 0x66]);
        ber_len(&mut out, body.len());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parses_server_core_and_network() {
        let core = ServerCoreData {
            version: 0x0008_0004,
            client_requested_protocols: Some(1),
            early_capability_flags: None,
        };
        let net = ServerNetworkData {
            mcs_channel_id: 1003,
            channel_ids: vec![1004],
        };
        let bytes = build_response(&core, &net);
        let resp = ConnectResponse::from_bytes(&bytes).unwrap();
        assert_eq!(resp.result, 0);
        assert_eq!(resp.server_core, Some(core));
        assert_eq!(resp.server_network, Some(net));
    }

    #[test]
    fn rejects_wrong_tag() {
        let err = ConnectResponse::from_bytes(&[0x7F, 0x65, 0x00]).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::InvalidField { field: "tag", .. }
        ));
    }
}

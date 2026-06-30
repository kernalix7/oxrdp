use crate::codec::{decode, encode_vec};
use crate::error::{DecodeError, DecodeResult, EncodeResult};
use crate::tpkt::{TpktHeader, TPKT_HEADER_LEN};
use crate::x224::{X224DataHeader, X224_DATA_HEADER_LEN};

/// Wrap an MCS PDU payload in a TPKT header + X.224 data header.
pub fn wrap_mcs(payload: &[u8]) -> EncodeResult<Vec<u8>> {
    let total = TPKT_HEADER_LEN + X224_DATA_HEADER_LEN + payload.len();
    let mut out = encode_vec(&TpktHeader::new(total as u16))?;
    out.extend_from_slice(&encode_vec(&X224DataHeader)?);
    out.extend_from_slice(payload);
    Ok(out)
}

/// Strip the TPKT + X.224 data headers from a received frame, returning the MCS PDU bytes.
/// Validates both headers. Returns the payload slice (everything after the 7-byte prefix,
/// bounded by the TPKT length field).
pub fn mcs_payload(frame: &[u8]) -> DecodeResult<&[u8]> {
    let tpkt = decode::<TpktHeader>(frame)?;
    let end = (tpkt.length as usize).min(frame.len());
    if end < TPKT_HEADER_LEN + X224_DATA_HEADER_LEN {
        return Err(DecodeError::InvalidLength {
            context: "MCS frame",
            reason: "frame shorter than TPKT+X224 headers",
        });
    }
    let _x224 = decode::<X224DataHeader>(&frame[TPKT_HEADER_LEN..])?;
    Ok(&frame[TPKT_HEADER_LEN + X224_DATA_HEADER_LEN..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_produces_tpkt_x224_frame() {
        let f = wrap_mcs(&[0xAA, 0xBB]).unwrap();
        assert_eq!(f, [0x03, 0x00, 0x00, 0x09, 0x02, 0xF0, 0x80, 0xAA, 0xBB]);
    }

    #[test]
    fn round_trip() {
        let payload = [0x01, 0x02, 0x03, 0x04];
        let f = wrap_mcs(&payload).unwrap();
        assert_eq!(mcs_payload(&f).unwrap(), payload);
    }

    #[test]
    fn rejects_short_frame() {
        assert!(mcs_payload(&[0x03, 0x00, 0x00, 0x05, 0x02]).is_err());
    }
}

use crate::caps::default_client_capabilities;
use crate::codec::{encode_vec, Decode};
use crate::cursor::ReadCursor;
use crate::error::{DecodeError, DecodeResult, EncodeResult};
use crate::share::{pdu_type, ShareControlHeader};

/// Demand Active PDU (server -> client).
///
/// We only parse it far enough to recover the `share_id` the server assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemandActive {
    /// Share control header source field.
    pub pdu_source: u16,
    /// Share id assigned by the server.
    pub share_id: u32,
}

impl DemandActive {
    /// Parse a Demand Active PDU from the share-layer bytes (starting at the ShareControlHeader).
    pub fn from_bytes(data: &[u8]) -> DecodeResult<Self> {
        let mut c = ReadCursor::new(data);
        let ctrl = ShareControlHeader::decode(&mut c)?;

        if ctrl.kind() != pdu_type::DEMAND_ACTIVE {
            return Err(DecodeError::InvalidField {
                context: "Demand Active",
                field: "pdu type",
                reason: "not a Demand Active PDU",
            });
        }

        let share_id = c.read_u32_le("Demand Active")?;
        let length_source_descriptor = c.read_u16_le("Demand Active")?;
        let _length_combined_capabilities = c.read_u16_le("Demand Active")?;

        // Skip the source descriptor; we do not need the capability sets here.
        let _ = c.read_slice(length_source_descriptor as usize, "Demand Active")?;

        Ok(DemandActive {
            pdu_source: ctrl.pdu_source,
            share_id,
        })
    }
}

/// Build a Confirm Active PDU (share-layer bytes: ShareControlHeader + body).
///
/// `share_id` echoes the Demand Active; `pdu_source` is the client's MCS user channel.
pub fn build_confirm_active(
    share_id: u32,
    pdu_source: u16,
    width: u16,
    height: u16,
    bpp: u16,
) -> EncodeResult<Vec<u8>> {
    const SOURCE_DESCRIPTOR: &[u8] = b"oxrdp\0";

    let (num_caps, caps) = default_client_capabilities(width, height, bpp)?;
    let length_combined_capabilities = 2 + 2 + caps.len(); // numberCapabilities + pad2 + sets

    let mut body = Vec::new();
    body.extend_from_slice(&share_id.to_le_bytes());
    body.extend_from_slice(&0x03EAu16.to_le_bytes()); // originatorId (server channel)
    body.extend_from_slice(&(SOURCE_DESCRIPTOR.len() as u16).to_le_bytes()); // lengthSourceDescriptor
    body.extend_from_slice(&(length_combined_capabilities as u16).to_le_bytes());
    body.extend_from_slice(SOURCE_DESCRIPTOR);
    body.extend_from_slice(&num_caps.to_le_bytes()); // numberCapabilities
    body.extend_from_slice(&0u16.to_le_bytes()); // pad2Octets
    body.extend_from_slice(&caps);

    let total_length = (6 + body.len()) as u16; // 6 = ShareControlHeader
    let header = ShareControlHeader::new(pdu_type::CONFIRM_ACTIVE, pdu_source, total_length);
    let mut out = encode_vec(&header)?;
    out.extend_from_slice(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_active_structure() {
        let bytes = build_confirm_active(0x0001_00EA, 1007, 1024, 768, 32).unwrap();
        // ShareControlHeader: total_length (LE) matches, pduType = CONFIRM_ACTIVE|0x10 = 0x13
        assert_eq!(
            u16::from_le_bytes([bytes[0], bytes[1]]) as usize,
            bytes.len()
        );
        assert_eq!(
            u16::from_le_bytes([bytes[2], bytes[3]]),
            pdu_type::CONFIRM_ACTIVE | 0x0010
        );
        // shareId at offset 6
        assert_eq!(
            u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
            0x0001_00EA
        );
    }

    #[test]
    fn demand_active_round_trips_share_id() {
        // Build a minimal Demand Active: ShareControlHeader(DEMAND_ACTIVE, src 0x03EA, total),
        // shareId, lengthSourceDescriptor=4, lengthCombinedCapabilities=0, source "abc\0", then nothing.
        let mut body = Vec::new();
        body.extend_from_slice(&0x0001_00EAu32.to_le_bytes());
        body.extend_from_slice(&4u16.to_le_bytes()); // lengthSourceDescriptor
        body.extend_from_slice(&0u16.to_le_bytes()); // lengthCombinedCapabilities
        body.extend_from_slice(b"abc\0");
        let total = (6 + body.len()) as u16;
        let header = ShareControlHeader::new(pdu_type::DEMAND_ACTIVE, 0x03EA, total);
        let mut pdu = encode_vec(&header).unwrap();
        pdu.extend_from_slice(&body);

        let da = DemandActive::from_bytes(&pdu).unwrap();
        assert_eq!(da.share_id, 0x0001_00EA);
        assert_eq!(da.pdu_source, 0x03EA);
    }

    #[test]
    fn rejects_non_demand_active() {
        let header = ShareControlHeader::new(pdu_type::CONFIRM_ACTIVE, 0x03EA, 20);
        let bytes = encode_vec(&header).unwrap();
        assert!(DemandActive::from_bytes(&bytes).is_err());
    }
}
